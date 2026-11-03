//! AI object removal and heal-source picking (#51, ADR-0051), promoted from `spikes/groom`.
//!
//! The flow for one removal ([`remove::RemovalEngine::remove`]): a click/box prompt on the photo
//! goes to MobileSAM ([`sam`]) for an object mask; the mask picks a square crop with context around
//! it; LaMa ([`lama`]) inpaints the crop at 512x512; the result is mapped back into the heal stage's
//! own color space ([`space`]) and returned as a `nicti_tapetum::heal::RemovalPatch` the GPU
//! composites over the frame. [`job::RemoveJob`] runs that on Pounce's GPU lane.
//!
//! Everything model-independent (geometry, color mapping, the pipeline itself) is unit-tested with
//! fake segmenter/inpainter implementations; the ONNX wrappers are exercised against the real
//! weights by `#[ignore]`d tests (see each module). Weights are never bundled or fetched from here:
//! paths come from `nicti_stalk::models::RemovalModels`, installed only by an explicit user action
//! (ADR-0218).

use std::path::PathBuf;
use std::sync::Arc;

use nicti_cornea::LinearFrame;

pub mod geom;
pub mod install;
pub mod job;
pub mod lama;
pub mod provider;
pub mod real;
pub mod remove;
pub mod sam;
pub mod source;
pub mod space;

#[derive(Debug, thiserror::Error)]
pub enum RemovalError {
    #[error("model file not found: {0}")]
    ModelNotFound(PathBuf),
    #[error("ONNX Runtime error: {0}")]
    Ort(String),
    #[error("no object found at the clicked spot")]
    NoObject,
    #[error("the selected region is too large to remove ({0} px across; the limit is {1})")]
    RegionTooLarge(i32, i32),
    #[error("invalid removal request: {0}")]
    BadInput(String),
    #[error("a downloaded model failed its integrity check: {0}")]
    Integrity(String),
}

pub(crate) fn ort_err(e: impl std::fmt::Display) -> RemovalError {
    RemovalError::Ort(e.to_string())
}

/// Random access to the heal stage's input image: linear camera RGB, black-subtracted and scaled
/// to roughly [0, 1] (exactly what `nicti_tapetum::stages::normalize_pixels` produces, but read
/// lazily -- a full-resolution frame as `[f32; 4]` would be ~730 MB).
pub trait PixelSource: Send + Sync {
    fn width(&self) -> u32;
    fn height(&self) -> u32;
    /// `x < width()`, `y < height()`.
    fn pixel(&self, x: u32, y: u32) -> [f32; 3];
}

/// [`PixelSource`] over a decoded RAW frame, using `normalize.wgsl`'s formula.
///
/// [`Self::new`] is the decode as is; [`Self::lensed`] is what the baked frame holds after the
/// lens stage (#358): the same warp, lateral-CA scale and vignette, evaluated lazily per pixel on
/// the CPU (`LensPlan::sample`, proven against the GPU kernel), so an AI mask's alpha or a removal
/// patch lines up with the rendered frame without ever holding a second full-resolution copy. The
/// plan (which may estimate lateral CA from the pixels, ~0.2-0.4 s at 45 MP, memoised per frame)
/// is resolved on the first `pixel` call, so it runs on the worker that samples, not on the UI
/// thread that built the source.
pub struct FramePixels {
    frame: Arc<LinearFrame>,
    clip: nicti_tapetum::stages::ClipNeutral,
    lens: Option<LensSource>,
}

struct LensSource {
    params: nicti_tapetum::coat::LensParams,
    plan: std::sync::OnceLock<nicti_tapetum::slit::LensPlan>,
}

impl FramePixels {
    pub fn new(frame: Arc<LinearFrame>) -> Self {
        let clip = nicti_tapetum::stages::ClipNeutral::for_frame(&frame);
        Self {
            frame,
            clip,
            lens: None,
        }
    }

    /// The frame as the lens stage leaves it under `params`.
    pub fn lensed(frame: Arc<LinearFrame>, params: nicti_tapetum::coat::LensParams) -> Self {
        let mut s = Self::new(frame);
        s.lens = Some(LensSource {
            params,
            plan: std::sync::OnceLock::new(),
        });
        s
    }

    /// The frame under an already-resolved lens plan (tests; and any caller that resolved one).
    pub fn with_plan(frame: Arc<LinearFrame>, plan: nicti_tapetum::slit::LensPlan) -> Self {
        let s = Self::lensed(frame, nicti_tapetum::coat::LensParams::default());
        if let Some(lens) = &s.lens {
            let _ = lens.plan.set(plan);
        }
        s
    }

    pub fn frame(&self) -> &Arc<LinearFrame> {
        &self.frame
    }

    /// The normalized, clip-neutral pre-lens value at integer coordinates, clamped to the frame.
    fn base(&self, x: i32, y: i32) -> [f32; 3] {
        let f = &*self.frame;
        let x = x.clamp(0, f.width as i32 - 1) as usize;
        let y = y.clamp(0, f.height as i32 - 1) as usize;
        let range = f.maximum as f32 - f.black as f32;
        let at = (y * f.width as usize + x) * 3;
        let sample =
            |c: usize| (f.pixels[at + c] as f32 - f.black as f32 - f.cblack[c] as f32) / range;
        self.clip.apply([sample(0), sample(1), sample(2)])
    }
}

impl PixelSource for FramePixels {
    fn width(&self) -> u32 {
        self.frame.width
    }
    fn height(&self) -> u32 {
        self.frame.height
    }
    fn pixel(&self, x: u32, y: u32) -> [f32; 3] {
        if let Some(lens) = &self.lens {
            let plan = lens.plan.get_or_init(|| {
                nicti_tapetum::slit::LensPlan::resolve(
                    &lens.params,
                    &self.frame,
                    nicti_tapetum::frame::Extent {
                        width: self.frame.width,
                        height: self.frame.height,
                    },
                )
            });
            if !plan.is_identity() {
                return plan.sample([x as f32 + 0.5, y as f32 + 0.5], |ix, iy| self.base(ix, iy));
            }
        }
        self.base(x as i32, y as i32)
    }
}

/// An in-memory [`PixelSource`], used by tests and anywhere a small image is already in hand.
pub struct RgbBuffer {
    pub width: u32,
    pub height: u32,
    pub data: Vec<[f32; 3]>,
}

impl PixelSource for RgbBuffer {
    fn width(&self) -> u32 {
        self.width
    }
    fn height(&self) -> u32 {
        self.height
    }
    fn pixel(&self, x: u32, y: u32) -> [f32; 3] {
        self.data[y as usize * self.width as usize + x as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nicti_tapetum::stages::normalize_pixels;

    #[test]
    fn frame_pixels_match_the_tapetum_normalize_reference() {
        let frame = LinearFrame {
            make: "T".into(),
            model: "S".into(),
            width: 3,
            height: 2,
            black: 100,
            maximum: 1100,
            cam_mul: [2.0, 1.0, 1.5, 1.0],
            pre_mul: [2.0, 1.0, 1.5, 1.0],
            cam_xyz: [0.0; 12],
            cblack: [10, 20, 5, 0],
            pixels: (0..3 * 2 * 3).map(|i| 100 + i * 50).collect(),
            dng_opcode_list3: None,
            nikon_lens_info: None,
            orientation: 0,
        };
        let expected = normalize_pixels(&frame);
        let src = FramePixels::new(Arc::new(frame));
        for y in 0..2u32 {
            for x in 0..3u32 {
                let e = expected[(y * 3 + x) as usize];
                let a = src.pixel(x, y);
                for c in 0..3 {
                    assert!((a[c] - e[c]).abs() < 1e-6, "({x},{y}) c{c}");
                }
            }
        }
    }

    #[test]
    fn frame_pixels_apply_the_same_clip_neutral_ceiling() {
        let frame = LinearFrame {
            make: "T".into(),
            model: "S".into(),
            width: 3,
            height: 1,
            black: 100,
            maximum: 1100,
            cam_mul: [2.0, 1.0, 1.5, 1.0],
            pre_mul: [2.0, 1.0, 1.5, 1.0],
            cam_xyz: [0.0; 12],
            cblack: [0; 4],
            pixels: vec![1100, 1100, 1100, 500, 400, 300, 900, 300, 300],
            dng_opcode_list3: None,
            nikon_lens_info: None,
            orientation: 0,
        };
        let expected = normalize_pixels(&frame);
        let src = FramePixels::new(Arc::new(frame));
        for x in 0..3u32 {
            let a = src.pixel(x, 0);
            for c in 0..3 {
                assert!((a[c] - expected[x as usize][c]).abs() < 1e-6, "x{x} c{c}");
            }
        }
        // The clipped pixel really is limited (not left at 1.0 in R).
        assert!(src.pixel(0, 0)[0] < 0.51);
        // ...while an unclipped red above that ceiling (0.8 > 0.5) is left whole.
        assert!((src.pixel(2, 0)[0] - 0.8).abs() < 1e-6);
    }

    fn textured_frame(w: u32, h: u32) -> LinearFrame {
        LinearFrame {
            make: "T".into(),
            model: "S".into(),
            width: w,
            height: h,
            black: 0,
            maximum: 1000,
            cam_mul: [1.0, 1.0, 1.0, 1.0],
            pre_mul: [1.0, 1.0, 1.0, 1.0],
            cam_xyz: [0.0; 12],
            cblack: [0; 4],
            pixels: (0..w * h * 3)
                .map(|i| (100 + (i * 37) % 700) as u16)
                .collect(),
            dng_opcode_list3: None,
            nikon_lens_info: None,
            orientation: 0,
        }
    }

    #[test]
    fn a_lensed_source_matches_the_lens_stage_on_the_same_pixels() {
        use nicti_tapetum::frame::Extent;
        use nicti_tapetum::slit::LensPlan;
        let frame = Arc::new(textured_frame(16, 12));
        let extent = Extent {
            width: 16,
            height: 12,
        };
        // A red/blue lateral-CA scale is a real, non-identity plan that moves pixels by sub-pixel
        // to pixel amounts; no embedded profile is needed to prove the wiring.
        let plan = LensPlan::new(None, [0.04, -0.03], None, extent);
        assert!(!plan.is_identity());
        let lensed = FramePixels::with_plan(Arc::clone(&frame), plan);
        let plain = normalize_pixels(&frame);
        let load = |x: i32, y: i32| {
            let x = x.clamp(0, 15) as usize;
            let y = y.clamp(0, 11) as usize;
            let p = plain[y * 16 + x];
            [p[0], p[1], p[2]]
        };
        let mut moved = 0;
        for y in 0..12u32 {
            for x in 0..16u32 {
                let want = plan.sample([x as f32 + 0.5, y as f32 + 0.5], load);
                let got = lensed.pixel(x, y);
                for c in 0..3 {
                    assert!((got[c] - want[c]).abs() < 1e-6, "({x},{y}) c{c}");
                }
                let unlensed = FramePixels::new(Arc::clone(&frame)).pixel(x, y);
                if (0..3).any(|c| (got[c] - unlensed[c]).abs() > 1e-4) {
                    moved += 1;
                }
            }
        }
        assert!(moved > 20, "the lens must actually move pixels ({moved})");
    }

    #[test]
    fn a_frame_with_no_lens_work_is_read_as_is_and_the_plan_is_resolved_lazily() {
        let frame = Arc::new(textured_frame(8, 6));
        // Default params on a frame with no profile: identity plan, so the source is the plain decode.
        let lensed = FramePixels::lensed(
            Arc::clone(&frame),
            nicti_tapetum::coat::LensParams::default(),
        );
        let plain = FramePixels::new(frame);
        for y in 0..6u32 {
            for x in 0..8u32 {
                assert_eq!(lensed.pixel(x, y), plain.pixel(x, y));
            }
        }
    }
}
