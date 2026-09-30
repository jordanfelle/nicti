//! [`MaskEngine`]: turns the `nicti.masks` stage's params into the atlas the live shader reads
//! (#49), caching every expensive intermediate so an edit only redoes what it changed.
//!
//! What each kind of edit costs (each is a test, via [`MaskStats`]):
//! - **A slider or Amount drag:** nothing on the GPU here at all. The deltas live in uniforms
//!   (`local::LocalUniform`); the composites and the atlas are reused as-is.
//! - **A geometry edit on one correction:** exactly that correction recomposes (and re-packs the
//!   atlas); every other correction's composite is a cache hit.
//! - **A brush stroke while painting:** one stroke pass. The field after strokes `[..n-1]` is
//!   cached, so each frame only the growing last stroke is re-run.
//! - **An AI alpha arriving:** only corrections that use that alpha recompose, and only its guided
//!   refine runs.
//! - **A new guide frame** (a heal edit, another photo): only AI and range components rebuild --
//!   gradient and brush corrections don't depend on the image.
//! - **Undoing back to an earlier state:** composites are still cached, so zero recomposes (as long
//!   as the byte budgets haven't evicted them).
//!
//! Masks render at the *mask extent*, the frame extent capped at [`MAX_MASK_LONG_EDGE`]; the live
//! shader samples the atlas bilinearly.

use std::collections::HashMap;
use std::sync::Arc;

use super::atlas::{Atlas, Bases, MaskFrame, CHANNELS};
use super::bases::{self, BasesKernels, ThumbTexture};
use super::compose::{ai_bake_key, hash_group, stable_hash};
use super::guided::{self, GuidedKernels};
use super::kernels::{bin_stroke, FieldTexture, MaskKernels};
use super::local::pack_active;
use super::params::{LocalCorrection, MaskParams, MaskSource};
use super::raster;
use super::Field;
use crate::cache::Tier;
use crate::color::Mat3;
use crate::frame::{Extent, FrameTexture};
use crate::gpu::GpuContext;

/// Longest mask edge. A 45 MP frame's masks are 4096 px on the long edge (~45 MB per `R32Float`
/// field) rather than 8280; the live shader's bilinear sample hides the difference.
pub const MAX_MASK_LONG_EDGE: u32 = 4096;

const COMPOSITE_BUDGET: u64 = 512 << 20;
const REFINED_BUDGET: u64 = 256 << 20;
const GUIDE_BUDGET: u64 = 128 << 20;
const BRUSH_BUDGET: u64 = 384 << 20;
const BANDS_BUDGET: u64 = 128 << 20;
const HAZE_BUDGET: u64 = 64 << 20;

/// The mask extent for a frame of `width x height`.
pub fn mask_extent(width: u32, height: u32) -> (u32, u32) {
    let long = width.max(height);
    if long <= MAX_MASK_LONG_EDGE {
        return (width.max(1), height.max(1));
    }
    let scale = MAX_MASK_LONG_EDGE as f32 / long as f32;
    (
        ((width as f32 * scale).round() as u32).max(1),
        ((height as f32 * scale).round() as u32).max(1),
    )
}

/// A finished model alpha, at the model's own resolution.
pub struct AiAlpha {
    pub width: usize,
    pub height: usize,
    pub alpha: Vec<f32>,
    /// Hash of the values, so a replaced alpha (same recipe, new pixels) changes cache keys.
    pub content_hash: blake3::Hash,
}

impl AiAlpha {
    /// `None` for a zero-sized or mismatched buffer. Values are clamped to 0..=1 and NaN becomes 0
    /// (a model output is untrusted too).
    pub fn new(width: usize, height: usize, mut alpha: Vec<f32>) -> Option<Self> {
        if width == 0 || height == 0 || alpha.len() != width * height {
            return None;
        }
        for v in &mut alpha {
            *v = if v.is_finite() {
                v.clamp(0.0, 1.0)
            } else {
                0.0
            };
        }
        let content_hash = blake3::hash(bytemuck::cast_slice(&alpha));
        Some(Self {
            width,
            height,
            alpha,
            content_hash,
        })
    }
}

/// Counters of the expensive things the engine did, cumulative. Tests diff them around a call.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MaskStats {
    /// Corrections recomposed (a composite cache miss).
    pub composites: usize,
    /// Guided refines of an AI alpha.
    pub ai_refines: usize,
    /// Guide-luminance builds.
    pub guide_builds: usize,
    /// Brush stroke GPU passes.
    pub brush_passes: usize,
    /// Range-mask GPU passes.
    pub range_passes: usize,
    /// Atlas repacks.
    pub packs: usize,
    /// Clarity/texture band builds.
    pub band_builds: usize,
    /// Dehaze transmission + airlight builds.
    pub haze_builds: usize,
}

/// Everything one `prepare` reads.
pub struct MaskInputs<'a> {
    pub params: &'a MaskParams,
    /// Finished model alphas by `compose::ai_bake_key`.
    pub ai_alphas: &'a HashMap<blake3::Hash, Arc<AiAlpha>>,
    /// Key of the neutral render the models run on (`nicti.neutral`'s graph cache key).
    pub neutral_key: blake3::Hash,
    /// The baked frame that AI refines and range masks follow (camera-linear).
    pub guide: &'a FrameTexture,
    /// Cache key of `guide`: when it changes, AI and range masks rebuild.
    pub guide_key: blake3::Hash,
    /// Camera -> working space, for range masks. Pass the *as-shot* matrix, not the live one, so a
    /// white-balance drag doesn't rebuild every range mask.
    pub range_matrix: Mat3,
}

/// A finished dehaze base: the refined transmission and the airlight it was built against.
#[derive(Clone)]
struct HazeState {
    transmission: Arc<FieldTexture>,
    airlight: [f32; 3],
}

#[derive(Clone)]
struct BrushState {
    tex: Arc<FieldTexture>,
    /// Dabs spent so far across the strokes folded into `tex` (the document-wide dab budget).
    dabs_used: usize,
}

struct Cx<'a> {
    mask: (u32, u32),
    inputs: &'a MaskInputs<'a>,
}

pub struct MaskEngine {
    kernels: Arc<MaskKernels>,
    guided: Arc<GuidedKernels>,
    bases_kernels: Arc<BasesKernels>,
    band_cache: Tier<Arc<FrameTexture>>,
    haze_cache: Tier<HazeState>,
    composites: Tier<Arc<FieldTexture>>,
    refined: Tier<Arc<FieldTexture>>,
    guides: Tier<Arc<FieldTexture>>,
    brushes: Tier<BrushState>,
    atlas: Option<(blake3::Hash, Arc<Atlas>)>,
    stats: MaskStats,
}

fn field_size(t: &Arc<FieldTexture>) -> u64 {
    t.byte_size()
}

fn submit(gpu: &GpuContext, build: impl FnOnce(&mut wgpu::CommandEncoder)) {
    let mut encoder = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("mask engine"),
        });
    build(&mut encoder);
    gpu.queue.submit(Some(encoder.finish()));
}

impl MaskEngine {
    pub fn new(gpu: &GpuContext) -> Self {
        Self::with_kernels(
            Arc::new(MaskKernels::new(gpu)),
            Arc::new(GuidedKernels::new(gpu)),
            Arc::new(BasesKernels::new(gpu)),
        )
    }

    /// An engine over already-built pipelines (so several engines, or a test binary, compile them
    /// once).
    pub fn with_kernels(
        kernels: Arc<MaskKernels>,
        guided: Arc<GuidedKernels>,
        bases_kernels: Arc<BasesKernels>,
    ) -> Self {
        Self {
            kernels,
            guided,
            bases_kernels,
            band_cache: Tier::new(BANDS_BUDGET, |f: &Arc<FrameTexture>| f.byte_size()),
            haze_cache: Tier::new(HAZE_BUDGET, |h: &HazeState| h.transmission.byte_size()),
            composites: Tier::new(COMPOSITE_BUDGET, field_size),
            refined: Tier::new(REFINED_BUDGET, field_size),
            guides: Tier::new(GUIDE_BUDGET, field_size),
            brushes: Tier::new(BRUSH_BUDGET, |s: &BrushState| s.tex.byte_size()),
            atlas: None,
            stats: MaskStats::default(),
        }
    }

    pub fn stats(&self) -> MaskStats {
        self.stats
    }

    /// Builds (or reuses) everything the live shader needs for `inputs`. `None` when no correction
    /// is active -- the caller should then `set_masks(None)`, which costs the shader nothing.
    pub fn prepare(&mut self, gpu: &GpuContext, inputs: &MaskInputs) -> Option<MaskFrame> {
        let params = inputs.params.sanitized();
        let active: Vec<&LocalCorrection> = params.active().collect();
        if active.is_empty() {
            return None;
        }
        let mask = mask_extent(inputs.guide.extent.width, inputs.guide.extent.height);
        let cx = Cx { mask, inputs };

        let mut composites = Vec::with_capacity(active.len());
        let mut atlas_key = blake3::Hasher::new();
        atlas_key.update(&mask.0.to_le_bytes());
        atlas_key.update(&mask.1.to_le_bytes());
        for correction in &active {
            let (key, texture) = self.composite(gpu, correction, &cx);
            atlas_key.update(key.as_bytes());
            composites.push(texture);
        }
        let atlas_key = atlas_key.finalize();

        let atlas = match &self.atlas {
            Some((k, a)) if *k == atlas_key => Arc::clone(a),
            _ => {
                self.stats.packs += 1;
                let atlas = Atlas::new(gpu, mask.0, mask.1, composites.len());
                submit(gpu, |enc| {
                    for (layer, chunk) in composites.chunks(CHANNELS).enumerate() {
                        let slot = |i: usize| chunk.get(i).map(|t| t.as_ref());
                        self.kernels.pack(
                            gpu,
                            enc,
                            [slot(0), slot(1), slot(2), slot(3)],
                            &atlas.layer_views[layer],
                            mask.0,
                            mask.1,
                        );
                    }
                });
                let atlas = Arc::new(atlas);
                self.atlas = Some((atlas_key, Arc::clone(&atlas)));
                atlas
            }
        };
        // The spatial bases are built only when some active correction uses them, and depend only
        // on the guide frame -- never on a slider, an Amount or a mask edit.
        let needs_bands = active
            .iter()
            .any(|c| c.adjust.clarity != 0.0 || c.adjust.texture != 0.0);
        let needs_haze = active.iter().any(|c| c.adjust.dehaze != 0.0);
        let mut bases = Bases::default();
        if needs_bands {
            bases.bands = Some(self.bands(gpu, &cx));
        }
        if needs_haze {
            let haze = self.haze(gpu, &cx);
            bases.haze = Some(haze.transmission);
            bases.airlight = haze.airlight;
        }
        Some(MaskFrame {
            atlas,
            uniforms: pack_active(&params),
            bases,
        })
    }

    /// The cache key of a correction's composite: its group, the extent, and -- only for the
    /// component kinds that depend on the image -- what they depend on.
    fn composite_key(&self, c: &LocalCorrection, cx: &Cx) -> blake3::Hash {
        let mut h = blake3::Hasher::new();
        h.update(b"mask-composite-1");
        h.update(hash_group(&c.mask).as_bytes());
        h.update(&cx.mask.0.to_le_bytes());
        h.update(&cx.mask.1.to_le_bytes());
        for comp in &c.mask.components {
            match &comp.source {
                MaskSource::Ai(_) => {
                    h.update(cx.inputs.guide_key.as_bytes());
                    let alpha = ai_bake_key(&comp.source, cx.inputs.neutral_key)
                        .and_then(|k| cx.inputs.ai_alphas.get(&k));
                    match alpha {
                        Some(a) => h.update(a.content_hash.as_bytes()),
                        None => h.update(&[0u8; 32]),
                    };
                }
                MaskSource::LuminanceRange { .. } | MaskSource::ColorRange { .. } => {
                    h.update(cx.inputs.guide_key.as_bytes());
                    for row in cx.inputs.range_matrix {
                        for v in row {
                            h.update(&v.to_le_bytes());
                        }
                    }
                }
                _ => {}
            }
        }
        h.finalize()
    }

    fn composite(
        &mut self,
        gpu: &GpuContext,
        c: &LocalCorrection,
        cx: &Cx,
    ) -> (blake3::Hash, Arc<FieldTexture>) {
        let key = self.composite_key(c, cx);
        if let Some(t) = self.composites.get(&key) {
            return (key, Arc::clone(t));
        }
        self.stats.composites += 1;
        let (mw, mh) = cx.mask;
        let mut acc = FieldTexture::new(gpu, mw, mh);
        let mut spare = FieldTexture::new(gpu, mw, mh);
        for comp in &c.mask.components {
            let Some(weight) = self.component_weight(gpu, &comp.source, cx) else {
                continue; // unavailable (model missing / not baked yet): contributes nothing
            };
            submit(gpu, |enc| {
                self.kernels.compose(
                    gpu,
                    enc,
                    &acc,
                    &weight,
                    &spare,
                    comp.invert,
                    comp.opacity,
                    comp.op,
                );
            });
            std::mem::swap(&mut acc, &mut spare);
        }
        let texture = Arc::new(acc);
        self.composites.put(key, Arc::clone(&texture));
        (key, texture)
    }

    /// One component's raw weight field, or `None` when it isn't available yet.
    fn component_weight(
        &mut self,
        gpu: &GpuContext,
        source: &MaskSource,
        cx: &Cx,
    ) -> Option<Arc<FieldTexture>> {
        let (mw, mh) = cx.mask;
        let (w, h) = (mw as f32, mh as f32);
        let long = w.max(h);
        match source {
            MaskSource::LinearGradient { p0, p1 } => {
                let out = FieldTexture::new(gpu, mw, mh);
                submit(gpu, |enc| {
                    self.kernels.linear(
                        gpu,
                        enc,
                        &out,
                        (p0[0] * w, p0[1] * h),
                        (p1[0] * w, p1[1] * h),
                    );
                });
                Some(Arc::new(out))
            }
            MaskSource::RadialGradient {
                center,
                radii,
                angle_deg,
                feather,
            } => {
                let out = FieldTexture::new(gpu, mw, mh);
                submit(gpu, |enc| {
                    self.kernels.radial(
                        gpu,
                        enc,
                        &out,
                        (center[0] * w, center[1] * h),
                        (radii[0] * long, radii[1] * long),
                        *angle_deg,
                        feather * long,
                    );
                });
                Some(Arc::new(out))
            }
            MaskSource::Brush { strokes } => Some(self.brush_field(gpu, strokes, cx)),
            MaskSource::Ai(_) => {
                let key = ai_bake_key(source, cx.inputs.neutral_key)?;
                let alpha = Arc::clone(cx.inputs.ai_alphas.get(&key)?);
                Some(self.refined_alpha(gpu, &alpha, cx))
            }
            MaskSource::LuminanceRange { .. } | MaskSource::ColorRange { .. } => {
                self.stats.range_passes += 1;
                let out = FieldTexture::new(gpu, mw, mh);
                let guide = cx.inputs.guide;
                submit(gpu, |enc| {
                    self.kernels.range(
                        gpu,
                        enc,
                        &guide.view,
                        (guide.extent.width, guide.extent.height),
                        &out,
                        source,
                        cx.inputs.range_matrix,
                    );
                });
                Some(Arc::new(out))
            }
        }
    }

    /// The brush field for `strokes`, resuming from the longest cached prefix.
    fn brush_field(
        &mut self,
        gpu: &GpuContext,
        strokes: &[super::params::Stroke],
        cx: &Cx,
    ) -> Arc<FieldTexture> {
        let (mw, mh) = cx.mask;
        let n = strokes.len();
        // prefix[k] identifies the field after strokes[..k] at this extent.
        let mut prefix = Vec::with_capacity(n + 1);
        let mut chain = blake3::Hasher::new();
        chain.update(b"brush-prefix-1");
        chain.update(&mw.to_le_bytes());
        chain.update(&mh.to_le_bytes());
        prefix.push(chain.finalize());
        for s in strokes {
            let mut h = blake3::Hasher::new();
            h.update(prefix.last().expect("seeded").as_bytes());
            h.update(stable_hash(s).as_bytes());
            prefix.push(h.finalize());
        }

        let mut start = 0;
        let mut state = None;
        for k in (1..=n).rev() {
            if let Some(s) = self.brushes.get(&prefix[k]) {
                start = k;
                state = Some(s.clone());
                break;
            }
        }
        let mut state = state.unwrap_or_else(|| BrushState {
            tex: Arc::new(FieldTexture::new(gpu, mw, mh)),
            dabs_used: 0,
        });
        let mut scratch: Option<FieldTexture> = None;

        for i in start..n {
            let stroke = &strokes[i];
            let budget = raster::MAX_DABS_TOTAL.saturating_sub(state.dabs_used);
            let dabs = raster::dabs_for_stroke(stroke, mw as usize, mh as usize, budget);
            let used = state.dabs_used + dabs.len();
            let next_tex = match bin_stroke(&dabs, mw as usize, mh as usize) {
                None => Arc::clone(&state.tex),
                Some(binned) => {
                    self.stats.brush_passes += 1;
                    let out = scratch
                        .take()
                        .unwrap_or_else(|| FieldTexture::new(gpu, mw, mh));
                    submit(gpu, |enc| {
                        self.kernels.brush_stroke(
                            gpu,
                            enc,
                            &state.tex,
                            &out,
                            &binned,
                            stroke.erase,
                        );
                    });
                    Arc::new(out)
                }
            };
            let next = BrushState {
                tex: next_tex,
                dabs_used: used,
            };
            // Only the last two states can be a resume point for the next edit (painting grows the
            // last stroke), so only those are worth their memory.
            if i + 1 == n || i + 2 == n {
                self.brushes.put(prefix[i + 1], next.clone());
            }
            let old = std::mem::replace(&mut state, next);
            // An intermediate nobody else holds (not cached, not the new state) is recycled as the
            // next stroke's output; the copy in `brush_stroke` fully overwrites it.
            if !Arc::ptr_eq(&old.tex, &state.tex) {
                if let Ok(tex) = Arc::try_unwrap(old.tex) {
                    scratch = Some(tex);
                }
            }
        }
        state.tex
    }

    fn guide_luma(&mut self, gpu: &GpuContext, cx: &Cx) -> Arc<FieldTexture> {
        self.guide_luma_at(gpu, cx, cx.mask)
    }

    /// The guide luminance resampled to `extent` (the mask extent, or the smaller bases extent).
    fn guide_luma_at(
        &mut self,
        gpu: &GpuContext,
        cx: &Cx,
        extent: (u32, u32),
    ) -> Arc<FieldTexture> {
        let (mw, mh) = extent;
        let mut h = blake3::Hasher::new();
        h.update(b"guide-luma-1");
        h.update(cx.inputs.guide_key.as_bytes());
        h.update(&mw.to_le_bytes());
        h.update(&mh.to_le_bytes());
        let key = h.finalize();
        if let Some(t) = self.guides.get(&key) {
            return Arc::clone(t);
        }
        self.stats.guide_builds += 1;
        let out = FieldTexture::new(gpu, mw, mh);
        let guide = cx.inputs.guide;
        submit(gpu, |enc| {
            self.guided.guide_luma(
                gpu,
                enc,
                &guide.view,
                (guide.extent.width, guide.extent.height),
                &out,
            );
        });
        let out = Arc::new(out);
        self.guides.put(key, Arc::clone(&out));
        out
    }

    /// The clarity/texture bands for the current guide: `(g - fine, fine - coarse, g)` at the bases
    /// extent, cached per (guide, extent).
    fn bands(&mut self, gpu: &GpuContext, cx: &Cx) -> Arc<FrameTexture> {
        let (bw, bh) = bases::bases_extent(cx.mask);
        let mut h = blake3::Hasher::new();
        h.update(b"bands-1");
        h.update(cx.inputs.guide_key.as_bytes());
        h.update(&bw.to_le_bytes());
        h.update(&bh.to_le_bytes());
        let key = h.finalize();
        if let Some(t) = self.band_cache.get(&key) {
            return Arc::clone(t);
        }
        self.stats.band_builds += 1;
        let g = self.guide_luma_at(gpu, cx, (bw, bh));
        let fine = FieldTexture::new(gpu, bw, bh);
        let coarse = FieldTexture::new(gpu, bw, bh);
        let out = FrameTexture::new(
            gpu,
            Extent {
                width: bw,
                height: bh,
            },
        );
        let (w, hh) = (bw as usize, bh as usize);
        submit(gpu, |enc| {
            // Self-guided: the luminance smooths itself, so edges survive and detail does not.
            self.guided.refine(
                gpu,
                enc,
                &g,
                &g,
                &fine,
                bases::fine_radius(w, hh),
                bases::FINE_EPS,
            );
            self.guided.refine(
                gpu,
                enc,
                &g,
                &g,
                &coarse,
                bases::coarse_radius(w, hh),
                bases::COARSE_EPS,
            );
            self.bases_kernels
                .combine(gpu, enc, &g, &fine, &coarse, &out);
        });
        let out = Arc::new(out);
        self.band_cache.put(key, Arc::clone(&out));
        out
    }

    /// The dehaze transmission (refined) and airlight for the current guide, cached per
    /// (guide, extent). The airlight needs a small CPU readback (a ~128 px thumbnail).
    fn haze(&mut self, gpu: &GpuContext, cx: &Cx) -> HazeState {
        let (bw, bh) = bases::bases_extent(cx.mask);
        let mut h = blake3::Hasher::new();
        h.update(b"haze-1");
        h.update(cx.inputs.guide_key.as_bytes());
        h.update(&bw.to_le_bytes());
        h.update(&bh.to_le_bytes());
        let key = h.finalize();
        if let Some(s) = self.haze_cache.get(&key) {
            return s.clone();
        }
        self.stats.haze_builds += 1;
        let guide = cx.inputs.guide;
        let fextent = (guide.extent.width, guide.extent.height);

        let (tw, th) = bases::thumb_extent(fextent.0, fextent.1);
        let thumb = ThumbTexture::new(gpu, tw, th);
        submit(gpu, |enc| {
            self.bases_kernels
                .thumb(gpu, enc, &guide.view, fextent, &thumb);
        });
        let airlight = bases::estimate_airlight(&thumb.read(gpu), tw as usize, th as usize);

        let g = self.guide_luma_at(gpu, cx, (bw, bh));
        let scratch = FieldTexture::new(gpu, bw, bh);
        let raw = FieldTexture::new(gpu, bw, bh);
        let refined = FieldTexture::new(gpu, bw, bh);
        submit(gpu, |enc| {
            self.bases_kernels.transmission(
                gpu,
                enc,
                &guide.view,
                fextent,
                airlight,
                &scratch,
                &raw,
            );
            self.guided.refine(
                gpu,
                enc,
                &raw,
                &g,
                &refined,
                bases::dark_radius(bw as usize, bh as usize),
                bases::TRANSMISSION_EPS,
            );
        });
        let state = HazeState {
            transmission: Arc::new(refined),
            airlight,
        };
        self.haze_cache.put(key, state.clone());
        state
    }

    fn refined_alpha(&mut self, gpu: &GpuContext, alpha: &AiAlpha, cx: &Cx) -> Arc<FieldTexture> {
        let (mw, mh) = cx.mask;
        let mut h = blake3::Hasher::new();
        h.update(b"ai-refined-1");
        h.update(alpha.content_hash.as_bytes());
        h.update(cx.inputs.guide_key.as_bytes());
        h.update(&mw.to_le_bytes());
        h.update(&mh.to_le_bytes());
        let key = h.finalize();
        if let Some(t) = self.refined.get(&key) {
            return Arc::clone(t);
        }
        self.stats.ai_refines += 1;
        let guide = self.guide_luma(gpu, cx);
        let low = FieldTexture::upload(
            gpu,
            &Field {
                width: alpha.width,
                height: alpha.height,
                data: alpha.alpha.clone(),
            },
        );
        let out = FieldTexture::new(gpu, mw, mh);
        submit(gpu, |enc| {
            self.guided.refine(
                gpu,
                enc,
                &low,
                &guide,
                &out,
                guided::radius_for(alpha.width, alpha.height),
                guided::EPS,
            );
        });
        let out = Arc::new(out);
        self.refined.put(key, Arc::clone(&out));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::super::params::{LocalAdjust, MaskComponent, MaskGroup, Op, Stroke, TintColor};
    use super::*;
    use crate::coat::MaskRecipe;
    use crate::frame::Extent;
    use crate::test_util::{shared_test_gpu, upload_frame};
    use serde_json::json;

    const W: usize = 96;
    const H: usize = 64;

    fn guide_frame(gpu: &GpuContext, shift: f32) -> FrameTexture {
        let px: Vec<[f32; 4]> = (0..W * H)
            .map(|i| {
                let (x, y) = ((i % W) as f32 / W as f32, (i / W) as f32 / H as f32);
                let edge = if x > 0.5 + shift { 0.6 } else { 0.1 };
                [edge + 0.1 * y, edge, edge * 0.8, 1.0]
            })
            .collect();
        upload_frame(
            gpu,
            Extent {
                width: W as u32,
                height: H as u32,
            },
            &px,
        )
    }

    fn radial(cx: f32) -> MaskSource {
        MaskSource::RadialGradient {
            center: [cx, 0.5],
            radii: [0.2, 0.2],
            angle_deg: 0.0,
            feather: 0.05,
        }
    }

    fn correction(id: &str, source: MaskSource, exposure: f32) -> LocalCorrection {
        LocalCorrection {
            id: id.into(),
            name: id.into(),
            mask: MaskGroup {
                components: vec![MaskComponent {
                    source,
                    ..MaskComponent::default()
                }],
            },
            adjust: LocalAdjust {
                exposure,
                ..LocalAdjust::default()
            },
            ..LocalCorrection::default()
        }
    }

    fn recipe(target: &str) -> MaskRecipe {
        MaskRecipe {
            model_id: "test.model".into(),
            model_version: "1".into(),
            params: json!({ "target": target }),
            seed: None,
        }
    }

    struct Rig {
        gpu: Arc<GpuContext>,
        engine: MaskEngine,
        guide: FrameTexture,
        guide_key: blake3::Hash,
        neutral: blake3::Hash,
        alphas: HashMap<blake3::Hash, Arc<AiAlpha>>,
    }

    impl Rig {
        fn new() -> Option<Self> {
            let gpu = shared_test_gpu()?;
            let engine = MaskEngine::with_kernels(
                super::super::kernels::tests::shared_kernels(&gpu),
                super::super::guided::tests::shared_kernels(&gpu),
                super::super::bases::tests::shared_kernels(&gpu),
            );
            let guide = guide_frame(&gpu, 0.0);
            Some(Self {
                gpu,
                engine,
                guide,
                guide_key: blake3::hash(b"guide-1"),
                neutral: blake3::hash(b"neutral-1"),
                alphas: HashMap::new(),
            })
        }

        fn prepare(&mut self, corrections: Vec<LocalCorrection>) -> Option<MaskFrame> {
            let params = MaskParams { corrections };
            let inputs = MaskInputs {
                params: &params,
                ai_alphas: &self.alphas,
                neutral_key: self.neutral,
                guide: &self.guide,
                guide_key: self.guide_key,
                range_matrix: crate::color::mat3_identity(),
            };
            self.engine.prepare(&self.gpu, &inputs)
        }

        /// The composite of correction `channel` read back from the atlas (f16-quantized).
        fn atlas_channel(&self, frame: &MaskFrame, channel: usize) -> Vec<f32> {
            let a = &frame.atlas;
            let layer = channel / CHANNELS;
            let mut enc = self
                .gpu
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
            let padded = (a.width * 8).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
                * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
            let buf = self.gpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size: u64::from(padded) * u64::from(a.height),
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            enc.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo {
                    texture: &a.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d {
                        x: 0,
                        y: 0,
                        z: layer as u32,
                    },
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyBufferInfo {
                    buffer: &buf,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(padded),
                        rows_per_image: Some(a.height),
                    },
                },
                wgpu::Extent3d {
                    width: a.width,
                    height: a.height,
                    depth_or_array_layers: 1,
                },
            );
            self.gpu.queue.submit(Some(enc.finish()));
            let slice = buf.slice(..);
            slice.map_async(wgpu::MapMode::Read, |_| {});
            self.gpu
                .device
                .poll(wgpu::PollType::wait_indefinitely())
                .unwrap();
            let raw = slice.get_mapped_range().unwrap();
            let mut out = Vec::new();
            for y in 0..a.height {
                let row = &raw[(y * padded) as usize..(y * padded + a.width * 8) as usize];
                let halves: &[u16] = bytemuck::cast_slice(row);
                for px in halves.chunks(4) {
                    out.push(half::f16::from_bits(px[channel % CHANNELS]).to_f32());
                }
            }
            out
        }
    }

    fn cpu_group(group: &MaskGroup, w: usize, h: usize) -> Field {
        super::super::compose::compose(group, w, h, |s| raster::rasterize_source(s, w, h)).unwrap()
    }

    fn close(a: &[f32], b: &[f32], tol: f32) -> f32 {
        let worst = a
            .iter()
            .zip(b)
            .map(|(x, y)| (x - y).abs())
            .fold(0.0f32, f32::max);
        assert!(worst < tol, "differ by {worst}");
        worst
    }

    #[test]
    fn the_mask_extent_caps_the_long_edge_and_keeps_aspect() {
        assert_eq!(mask_extent(3000, 2000), (3000, 2000));
        assert_eq!(mask_extent(8280, 5520), (4096, 2731));
        assert_eq!(mask_extent(5520, 8280), (2731, 4096));
        assert_eq!(mask_extent(1, 1), (1, 1));
    }

    #[test]
    fn an_empty_or_inactive_document_prepares_nothing() {
        let Some(mut rig) = Rig::new() else { return };
        assert!(rig.prepare(vec![]).is_none());
        let mut off = correction("a", radial(0.5), 1.0);
        off.enabled = false;
        let mut noop = correction("b", radial(0.5), 1.0);
        noop.adjust = LocalAdjust::default();
        assert!(rig.prepare(vec![off, noop]).is_none());
        assert_eq!(
            rig.engine.stats(),
            MaskStats::default(),
            "nothing was built"
        );
    }

    #[test]
    fn a_geometry_correction_lands_in_the_atlas_matching_the_cpu_composite() {
        let Some(mut rig) = Rig::new() else { return };
        let c = correction("a", radial(0.4), 1.0);
        let frame = rig.prepare(vec![c.clone()]).unwrap();
        assert_eq!(frame.uniforms.len(), 1);
        let got = rig.atlas_channel(&frame, 0);
        let want = cpu_group(&c.mask, W, H);
        close(&got, &want.data, 2e-3);
        // Unused channels of the layer are zero.
        assert!(rig.atlas_channel(&frame, 1).iter().all(|&v| v == 0.0));
    }

    #[test]
    fn a_slider_or_amount_drag_recomposes_and_repacks_nothing() {
        let Some(mut rig) = Rig::new() else { return };
        let mut c = correction("a", radial(0.4), 1.0);
        rig.prepare(vec![c.clone()]).unwrap();
        let before = rig.engine.stats();
        for stops in [0.5, 1.5, -1.0] {
            c.adjust.exposure = stops;
            c.adjust.contrast = stops * 0.1;
            c.amount = 0.3;
            c.adjust.color = Some(TintColor {
                hue_deg: 100.0,
                saturation: 0.2,
            });
            let frame = rig.prepare(vec![c.clone()]).unwrap();
            assert_eq!(
                frame.uniforms[0].d0[1], stops,
                "the new delta reached the uniform"
            );
        }
        assert_eq!(rig.engine.stats(), before, "drags must be uniform-only");
    }

    #[test]
    fn a_geometry_edit_recomposes_exactly_that_correction() {
        let Some(mut rig) = Rig::new() else { return };
        let a = correction("a", radial(0.25), 1.0);
        let b = correction("b", radial(0.75), 1.0);
        rig.prepare(vec![a.clone(), b.clone()]).unwrap();
        let before = rig.engine.stats();
        let moved = correction("b", radial(0.6), 1.0);
        rig.prepare(vec![a, moved]).unwrap();
        let after = rig.engine.stats();
        assert_eq!(after.composites - before.composites, 1);
        assert_eq!(after.packs - before.packs, 1, "the atlas is repacked once");
    }

    #[test]
    fn undoing_back_to_an_earlier_state_is_a_cache_hit() {
        let Some(mut rig) = Rig::new() else { return };
        let a = correction("a", radial(0.25), 1.0);
        let moved = correction("a", radial(0.5), 1.0);
        let first = rig.prepare(vec![a.clone()]).unwrap();
        rig.prepare(vec![moved]).unwrap();
        let before = rig.engine.stats();
        let again = rig.prepare(vec![a]).unwrap();
        let after = rig.engine.stats();
        assert_eq!(after.composites, before.composites, "no recompose on undo");
        assert_eq!(after.packs, before.packs + 1, "but the atlas is repacked");
        close(
            &rig.atlas_channel(&first, 0),
            &rig.atlas_channel(&again, 0),
            1e-6,
        );
    }

    #[test]
    fn many_corrections_span_atlas_layers() {
        let Some(mut rig) = Rig::new() else { return };
        let cs: Vec<_> = (0..6)
            .map(|i| correction(&format!("c{i}"), radial(0.1 + 0.15 * i as f32), 0.5))
            .collect();
        let frame = rig.prepare(cs.clone()).unwrap();
        assert_eq!(frame.atlas.layers, 2);
        for (i, c) in cs.iter().enumerate() {
            let got = rig.atlas_channel(&frame, i);
            close(&got, &cpu_group(&c.mask, W, H).data, 2e-3);
        }
    }

    fn stroke(points: &[[f32; 2]], erase: bool) -> Stroke {
        Stroke {
            points: points.to_vec(),
            radius: 0.06,
            feather: 0.02,
            flow: 1.0,
            erase,
        }
    }

    fn brush(strokes: Vec<Stroke>) -> MaskSource {
        MaskSource::Brush { strokes }
    }

    #[test]
    fn painting_a_stroke_is_one_pass_per_frame_and_matches_a_full_rebuild() {
        let Some(mut rig) = Rig::new() else { return };
        let first = stroke(&[[0.2, 0.3], [0.5, 0.4]], false);
        let mut second = stroke(&[[0.3, 0.7]], false);
        rig.prepare(vec![correction(
            "a",
            brush(vec![first.clone(), second.clone()]),
            1.0,
        )])
        .unwrap();
        let mut last = None;
        // The user drags: the last stroke gains a point each frame.
        for i in 1..=4 {
            second
                .points
                .push([0.3 + 0.12 * i as f32, 0.7 - 0.05 * i as f32]);
            let before = rig.engine.stats().brush_passes;
            let c = correction("a", brush(vec![first.clone(), second.clone()]), 1.0);
            last = Some((rig.prepare(vec![c.clone()]).unwrap(), c));
            assert_eq!(
                rig.engine.stats().brush_passes - before,
                1,
                "frame {i}: only the growing stroke may be re-run"
            );
        }
        let (frame, c) = last.unwrap();
        let got = rig.atlas_channel(&frame, 0);
        // Identical to a cold engine building the same strokes from scratch, and to the CPU.
        let mut cold = Rig::new().unwrap();
        let cold_frame = cold.prepare(vec![c.clone()]).unwrap();
        close(&got, &cold.atlas_channel(&cold_frame, 0), 1e-6);
        close(&got, &cpu_group(&c.mask, W, H).data, 3e-3);
    }

    #[test]
    fn an_erase_stroke_and_a_long_history_resume_correctly() {
        let Some(mut rig) = Rig::new() else { return };
        let mut strokes: Vec<Stroke> = (0..6)
            .map(|i| {
                stroke(
                    &[[0.1 + 0.13 * i as f32, 0.3], [0.15 + 0.13 * i as f32, 0.6]],
                    false,
                )
            })
            .collect();
        strokes.push(stroke(&[[0.3, 0.45], [0.6, 0.5]], true));
        let c = correction("a", brush(strokes.clone()), 1.0);
        let frame = rig.prepare(vec![c.clone()]).unwrap();
        close(
            &rig.atlas_channel(&frame, 0),
            &cpu_group(&c.mask, W, H).data,
            3e-3,
        );
        // Appending one more stroke resumes from the cached prefix: a single pass.
        strokes.push(stroke(&[[0.8, 0.2]], false));
        let before = rig.engine.stats().brush_passes;
        let c2 = correction("a", brush(strokes), 1.0);
        let frame2 = rig.prepare(vec![c2.clone()]).unwrap();
        assert_eq!(rig.engine.stats().brush_passes - before, 1);
        close(
            &rig.atlas_channel(&frame2, 0),
            &cpu_group(&c2.mask, W, H).data,
            3e-3,
        );
    }

    fn ai_source() -> MaskSource {
        MaskSource::Ai(recipe("subject"))
    }

    fn install_alpha(rig: &mut Rig, source: &MaskSource, value: f32) {
        let key = ai_bake_key(source, rig.neutral).unwrap();
        let (lw, lh) = (24usize, 16usize);
        // A left-half subject at model resolution.
        let data = (0..lw * lh)
            .map(|i| if i % lw < lw / 2 { value } else { 0.0 })
            .collect();
        rig.alphas
            .insert(key, Arc::new(AiAlpha::new(lw, lh, data).unwrap()));
    }

    #[test]
    fn an_ai_mask_with_no_alpha_yet_selects_nothing_then_the_alpha_recomposes_only_it() {
        let Some(mut rig) = Rig::new() else { return };
        let geo = correction("geo", radial(0.3), 1.0);
        let ai = correction("ai", ai_source(), 1.0);
        let frame = rig.prepare(vec![geo.clone(), ai.clone()]).unwrap();
        assert!(
            rig.atlas_channel(&frame, 1).iter().all(|&v| v == 0.0),
            "a model that hasn't finished must never select anything"
        );
        let before = rig.engine.stats();

        install_alpha(&mut rig, &ai_source(), 1.0);
        let frame = rig.prepare(vec![geo, ai]).unwrap();
        let after = rig.engine.stats();
        assert_eq!(
            after.composites - before.composites,
            1,
            "only the AI correction"
        );
        assert_eq!(after.ai_refines - before.ai_refines, 1);
        let left: f32 = rig.atlas_channel(&frame, 1)[..4].iter().sum();
        assert!(left > 2.0, "the subject half is selected now: {left}");
    }

    #[test]
    fn a_mask_and_its_inverse_share_one_refine() {
        let Some(mut rig) = Rig::new() else { return };
        install_alpha(&mut rig, &ai_source(), 1.0);
        let subject = correction("subject", ai_source(), 1.0);
        let mut background = correction("background", ai_source(), 1.0);
        background.mask.components[0].invert = true;
        let frame = rig.prepare(vec![subject, background]).unwrap();
        assert_eq!(
            rig.engine.stats().ai_refines,
            1,
            "the model output is refined once"
        );
        let (s, b) = (rig.atlas_channel(&frame, 0), rig.atlas_channel(&frame, 1));
        for (x, y) in s.iter().zip(&b) {
            assert!(
                (x + y - 1.0).abs() < 2e-3,
                "subject + background must partition"
            );
        }
    }

    #[test]
    fn a_new_guide_rebuilds_ai_and_range_masks_but_not_geometry() {
        let Some(mut rig) = Rig::new() else { return };
        install_alpha(&mut rig, &ai_source(), 1.0);
        let geo = correction("geo", radial(0.3), 1.0);
        let ai = correction("ai", ai_source(), 1.0);
        let range = correction(
            "range",
            MaskSource::LuminanceRange {
                lo: 0.0,
                hi: 0.5,
                smooth: 0.1,
            },
            1.0,
        );
        let cs = vec![geo, ai, range];
        rig.prepare(cs.clone()).unwrap();
        let before = rig.engine.stats();
        // A heal edit (or another photo) changes the guide.
        rig.guide = guide_frame(&rig.gpu, 0.1);
        rig.guide_key = blake3::hash(b"guide-2");
        rig.prepare(cs).unwrap();
        let after = rig.engine.stats();
        assert_eq!(
            after.composites - before.composites,
            2,
            "ai + range, not geometry"
        );
        assert_eq!(after.ai_refines - before.ai_refines, 1);
        assert_eq!(after.range_passes - before.range_passes, 1);
    }

    #[test]
    fn a_replaced_alpha_with_the_same_recipe_recomposes() {
        let Some(mut rig) = Rig::new() else { return };
        let ai = correction("ai", ai_source(), 1.0);
        install_alpha(&mut rig, &ai_source(), 1.0);
        rig.prepare(vec![ai.clone()]).unwrap();
        let before = rig.engine.stats();
        install_alpha(&mut rig, &ai_source(), 0.5);
        rig.prepare(vec![ai]).unwrap();
        assert_eq!(rig.engine.stats().composites - before.composites, 1);
    }

    /// The refine is the whole point of the guide: the GPU path must put the mask edge where the
    /// *photo's* edge is, not where the coarse alpha blurs it. The photo's edge sits mid-way through
    /// a low-res pixel (x = 52 of 96; low-res column 6 covers 48..56), so a model's alpha there is
    /// honestly 0.5 -- and a plain bilinear upsample would leave the mask soft across ~8 px.
    #[test]
    fn the_ai_mask_edge_follows_the_photos_edge_not_the_coarse_alpha() {
        let Some(mut rig) = Rig::new() else { return };
        rig.guide = guide_frame(&rig.gpu, 4.0 / W as f32);
        let key = ai_bake_key(&ai_source(), rig.neutral).unwrap();
        let (lw, lh) = (12usize, 8usize);
        let data = (0..lw * lh)
            .map(|i| match i % lw {
                0..=5 => 1.0,
                6 => 0.5,
                _ => 0.0,
            })
            .collect();
        rig.alphas
            .insert(key, Arc::new(AiAlpha::new(lw, lh, data).unwrap()));
        let frame = rig
            .prepare(vec![correction("ai", ai_source(), 1.0)])
            .unwrap();
        let m = rig.atlas_channel(&frame, 0);
        let row = |x: usize| m[(H / 2) * W + x];
        // The mask steps down at the photo's edge (between pixel 52 and 53), in one pixel.
        assert!(
            row(52) - row(53) > 0.4,
            "no step at the edge: {} -> {}",
            row(52),
            row(53)
        );
        assert!(row(44) > 0.7 && row(60) < 0.05, "{} / {}", row(44), row(60));
        // Control: a plain bilinear upsample of the same alpha ramps gently over ~16 px, so its
        // largest one-pixel step is far smaller and is not at the photo's edge.
        let coarse = Field {
            width: lw,
            height: lh,
            data: (0..lw * lh)
                .map(|i| match i % lw {
                    0..=5 => 1.0,
                    6 => 0.5,
                    _ => 0.0,
                })
                .collect(),
        };
        let bilinear = guided::bilinear_upsample(&coarse, W, H);
        let biggest_step = (0..W - 1)
            .map(|x| (bilinear.data[(H / 2) * W + x] - bilinear.data[(H / 2) * W + x + 1]).abs())
            .fold(0.0f32, f32::max);
        assert!(
            biggest_step < 0.15,
            "control: bilinear steps by {biggest_step}"
        );
    }

    #[test]
    fn a_range_correction_selects_by_the_image() {
        let Some(mut rig) = Rig::new() else { return };
        // guide_frame is dark (0.1) left of x=0.5 and bright (0.6) right of it.
        let bright = correction(
            "bright",
            MaskSource::LuminanceRange {
                lo: 0.6,
                hi: 1.0,
                smooth: 0.05,
            },
            1.0,
        );
        let frame = rig.prepare(vec![bright]).unwrap();
        let m = rig.atlas_channel(&frame, 0);
        assert!(m[(H / 2) * W + 10] < 0.05, "the dark half is unselected");
        assert!(
            m[(H / 2) * W + W - 10] > 0.95,
            "the bright half is selected"
        );
    }

    #[test]
    fn a_huge_hostile_document_is_bounded_not_a_panic() {
        let Some(mut rig) = Rig::new() else { return };
        let many: Vec<_> = (0..200)
            .map(|i| correction(&format!("c{i}"), radial(0.5), 1.0))
            .collect();
        let frame = rig.prepare(many).unwrap();
        assert_eq!(frame.uniforms.len(), crate::mask::params::MAX_CORRECTIONS);
        let nan = correction(
            "nan",
            MaskSource::LinearGradient {
                p0: [f32::NAN, f32::INFINITY],
                p1: [1e30, -1e30],
            },
            1.0,
        );
        let frame = rig.prepare(vec![nan]).unwrap();
        assert!(rig.atlas_channel(&frame, 0).iter().all(|v| v.is_finite()));
    }

    #[test]
    fn composing_two_components_with_subtract_matches_the_cpu() {
        let Some(mut rig) = Rig::new() else { return };
        let mut c = correction("a", radial(0.5), 1.0);
        c.mask.components.push(MaskComponent {
            source: radial(0.5).clone_with_radii(0.08),
            op: Op::Subtract,
            ..MaskComponent::default()
        });
        let frame = rig.prepare(vec![c.clone()]).unwrap();
        close(
            &rig.atlas_channel(&frame, 0),
            &cpu_group(&c.mask, W, H).data,
            3e-3,
        );
    }

    #[test]
    fn the_spatial_bases_are_built_once_per_guide_and_never_for_a_slider_drag() {
        let Some(mut rig) = Rig::new() else { return };
        let mut c = correction("a", radial(0.5), 0.0);
        c.adjust = LocalAdjust {
            clarity: 0.5,
            dehaze: 0.4,
            ..LocalAdjust::default()
        };
        let frame = rig.prepare(vec![c.clone()]).unwrap();
        assert!(frame.bases.bands.is_some() && frame.bases.haze.is_some());
        let s = rig.engine.stats();
        assert_eq!((s.band_builds, s.haze_builds), (1, 1));

        // Dragging clarity/dehaze/texture or Amount touches only uniforms: no rebuild.
        for v in [0.1, -0.7, 1.0] {
            c.adjust.clarity = v;
            c.adjust.dehaze = -v;
            c.adjust.texture = v * 0.5;
            c.amount = 0.6;
            rig.prepare(vec![c.clone()]).unwrap();
        }
        let s = rig.engine.stats();
        assert_eq!(
            (s.band_builds, s.haze_builds),
            (1, 1),
            "drags must not rebuild bases"
        );

        // A new guide (a heal edit, another photo) rebuilds both, once.
        rig.guide = guide_frame(&rig.gpu, 0.1);
        rig.guide_key = blake3::hash(b"guide-3");
        rig.prepare(vec![c.clone()]).unwrap();
        rig.prepare(vec![c]).unwrap();
        let s = rig.engine.stats();
        assert_eq!((s.band_builds, s.haze_builds), (2, 2));
    }

    #[test]
    fn a_mask_that_needs_no_spatial_base_builds_none() {
        let Some(mut rig) = Rig::new() else { return };
        rig.prepare(vec![correction("a", radial(0.5), 1.0)])
            .unwrap();
        let s = rig.engine.stats();
        assert_eq!((s.band_builds, s.haze_builds), (0, 0));
    }

    trait CloneWithRadii {
        fn clone_with_radii(&self, r: f32) -> MaskSource;
    }
    impl CloneWithRadii for MaskSource {
        fn clone_with_radii(&self, r: f32) -> MaskSource {
            match self {
                MaskSource::RadialGradient {
                    center,
                    angle_deg,
                    feather,
                    ..
                } => MaskSource::RadialGradient {
                    center: *center,
                    radii: [r, r],
                    angle_deg: *angle_deg,
                    feather: *feather,
                },
                other => other.clone(),
            }
        }
    }
}
