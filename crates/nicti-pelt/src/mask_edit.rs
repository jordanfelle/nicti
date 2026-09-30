//! The masks tool's editing operations, free of egui so every one is unit-testable: creating,
//! duplicating and deleting corrections, adding components, the brush and gradient edits the
//! viewport gestures drive, colour sampling, and the CPU *preview* the overlay paints.
//!
//! Everything here works on the stored, normalized model (`nicti_tapetum::mask::params`): points
//! are `x / width, y / height` of the uncropped frame and lengths are fractions of its long edge.
//! The overlay preview is deliberately computed on the CPU at a small size from what the CPU
//! already has (geometry, the low-resolution AI alpha, a thumbnail of the frame for range masks):
//! it needs no GPU readback and no change to the colour-managed display shader, and being a
//! preview it does not need the guided refine the real render applies.

use std::sync::Arc;

use nicti_cornea::LinearFrame;
use nicti_groom::{FramePixels, PixelSource};
use nicti_siamese::providers::recipe_for;
use nicti_stalk::SegmentTarget;
use nicti_tapetum::coat::WbParams;
use nicti_tapetum::color::{self, Mat3};
use nicti_tapetum::mask::compose::{self, ai_bake_key};
use nicti_tapetum::mask::guided::sample_mapped;
use nicti_tapetum::mask::params::{
    LocalCorrection, MaskComponent, MaskGroup, MaskParams, MaskSource, Op, Stroke,
    MAX_COLOR_SAMPLES, MAX_COMPONENTS, MAX_CORRECTIONS,
};
use nicti_tapetum::mask::raster;
use nicti_tapetum::mask::Field;

use crate::render::DevelopView;

/// What a "New mask" button creates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewMask {
    Subject,
    Background,
    Sky,
    Brush,
    Linear,
    Radial,
    Luminance,
    Color,
}

impl NewMask {
    pub const ALL: [NewMask; 8] = [
        NewMask::Subject,
        NewMask::Background,
        NewMask::Sky,
        NewMask::Brush,
        NewMask::Linear,
        NewMask::Radial,
        NewMask::Luminance,
        NewMask::Color,
    ];

    pub fn label(self) -> &'static str {
        match self {
            NewMask::Subject => "Subject",
            NewMask::Background => "Background",
            NewMask::Sky => "Sky (beta)",
            NewMask::Brush => "Brush",
            NewMask::Linear => "Linear",
            NewMask::Radial => "Radial",
            NewMask::Luminance => "Luminance range",
            NewMask::Color => "Colour range",
        }
    }

    /// The name a new correction of this kind starts with.
    fn name(self) -> &'static str {
        match self {
            NewMask::Sky => "Sky",
            NewMask::Linear => "Linear gradient",
            NewMask::Radial => "Radial gradient",
            other => other.label(),
        }
    }

    /// The viewport tool to arm right after creating one, so the next drag edits it.
    pub fn arm(self) -> Arm {
        match self {
            NewMask::Brush => Arm::Brush,
            NewMask::Linear => Arm::Linear,
            NewMask::Radial => Arm::Radial,
            NewMask::Color => Arm::Pick,
            _ => Arm::None,
        }
    }

    /// The component source this kind stands for.
    pub fn source(self) -> MaskSource {
        match self {
            NewMask::Subject | NewMask::Background => {
                MaskSource::Ai(recipe_for(SegmentTarget::Subject))
            }
            NewMask::Sky => MaskSource::Ai(recipe_for(SegmentTarget::Sky)),
            NewMask::Brush => MaskSource::Brush {
                strokes: Vec::new(),
            },
            // Full weight at the top, fading to nothing by the bottom.
            NewMask::Linear => MaskSource::LinearGradient {
                p0: [0.5, 0.25],
                p1: [0.5, 0.75],
            },
            NewMask::Radial => MaskSource::RadialGradient {
                center: [0.5, 0.5],
                radii: [0.25, 0.25],
                angle_deg: 0.0,
                feather: 0.1,
            },
            NewMask::Luminance => MaskSource::LuminanceRange {
                lo: 0.65,
                hi: 1.0,
                smooth: 0.1,
            },
            NewMask::Color => MaskSource::ColorRange {
                samples: Vec::new(),
                tolerance: 25.0,
            },
        }
    }
}

/// Which gesture the viewport currently applies to the selected correction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Arm {
    #[default]
    None,
    Brush,
    Linear,
    Radial,
    /// Click to sample a colour into a colour-range component.
    Pick,
}

/// The next unused correction id (`mask-1`, `mask-2`, ...). Ids are stable identities that survive
/// renames and reordering.
pub fn next_id(params: &MaskParams) -> String {
    let used: std::collections::HashSet<&str> =
        params.corrections.iter().map(|c| c.id.as_str()).collect();
    (1..)
        .map(|n| format!("mask-{n}"))
        .find(|id| !used.contains(id.as_str()))
        .expect("an unused id always exists")
}

/// A new correction of `kind`, with no adjustment yet (the user picks what it does).
pub fn new_correction(kind: NewMask, id: String) -> LocalCorrection {
    LocalCorrection {
        id,
        name: kind.name().to_owned(),
        mask: MaskGroup {
            components: vec![MaskComponent {
                source: kind.source(),
                invert: kind == NewMask::Background,
                ..MaskComponent::default()
            }],
        },
        ..LocalCorrection::default()
    }
}

/// Appends `correction`, returning its index, or `None` at the corrections limit.
pub fn add_correction(params: &mut MaskParams, correction: LocalCorrection) -> Option<usize> {
    if params.corrections.len() >= MAX_CORRECTIONS {
        return None;
    }
    params.corrections.push(correction);
    Some(params.corrections.len() - 1)
}

/// Duplicates correction `index` right after itself under a fresh id. With `invert`, every
/// component's `invert` is flipped -- "Duplicate and invert" -- so the copy selects the complement
/// (a subject and its background, for instance) and shares the original's AI bake.
pub fn duplicate(params: &mut MaskParams, index: usize, invert: bool) -> Option<usize> {
    if params.corrections.len() >= MAX_CORRECTIONS {
        return None;
    }
    let mut copy = params.corrections.get(index)?.clone();
    copy.id = next_id(params);
    copy.name = if invert {
        format!("{} (inverted)", copy.name)
    } else {
        format!("{} copy", copy.name)
    };
    if invert {
        for c in &mut copy.mask.components {
            c.invert = !c.invert;
        }
    }
    params.corrections.insert(index + 1, copy);
    Some(index + 1)
}

pub fn delete(params: &mut MaskParams, index: usize) -> bool {
    if index < params.corrections.len() {
        params.corrections.remove(index);
        true
    } else {
        false
    }
}

/// Adds a component of `kind` with `op` to `c`, returning its index, or `None` at the limit.
pub fn add_component(c: &mut LocalCorrection, kind: NewMask, op: Op) -> Option<usize> {
    if c.mask.components.len() >= MAX_COMPONENTS {
        return None;
    }
    c.mask.components.push(MaskComponent {
        source: kind.source(),
        op,
        invert: kind == NewMask::Background,
        ..MaskComponent::default()
    });
    Some(c.mask.components.len() - 1)
}

/// The brush's defaults for the next stroke, all as fractions of the long edge (except `flow`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BrushSettings {
    pub radius: f32,
    pub feather: f32,
    pub flow: f32,
}

impl Default for BrushSettings {
    fn default() -> Self {
        Self {
            radius: 0.03,
            feather: 0.012,
            flow: 1.0,
        }
    }
}

pub const MIN_BRUSH_RADIUS: f32 = 0.002;
pub const MAX_BRUSH_RADIUS: f32 = 0.5;

impl BrushSettings {
    /// Scales the radius (keeping the feather in proportion), clamped to a usable range.
    pub fn resize(&mut self, factor: f32) {
        let old = self.radius;
        self.radius = (self.radius * factor).clamp(MIN_BRUSH_RADIUS, MAX_BRUSH_RADIUS);
        self.feather = (self.feather * self.radius / old).min(self.radius);
    }

    /// Scales the feather alone, up to the radius.
    pub fn refeather(&mut self, factor: f32) {
        self.feather = (self.feather * factor).clamp(0.0, self.radius);
    }
}

fn first_brush(c: &LocalCorrection) -> Option<usize> {
    c.mask
        .components
        .iter()
        .position(|comp| matches!(comp.source, MaskSource::Brush { .. }))
}

/// Starts a stroke at `p` (normalized) on the correction's first brush component -- adding one if
/// it has none -- and returns `(component, stroke)` indices.
pub fn begin_stroke(
    c: &mut LocalCorrection,
    brush: &BrushSettings,
    erase: bool,
    p: [f32; 2],
) -> Option<(usize, usize)> {
    let comp = match first_brush(c) {
        Some(i) => i,
        None => add_component(c, NewMask::Brush, Op::Add)?,
    };
    let MaskSource::Brush { strokes } = &mut c.mask.components[comp].source else {
        return None;
    };
    strokes.push(Stroke {
        points: vec![p],
        radius: brush.radius,
        feather: brush.feather.min(brush.radius),
        flow: brush.flow,
        erase,
    });
    Some((comp, strokes.len() - 1))
}

/// Appends `p` to a stroke if it moved at least `min_step` (normalized) from the last point, so a
/// slow drag doesn't store a point per frame. Returns whether it appended.
pub fn extend_stroke(
    c: &mut LocalCorrection,
    comp: usize,
    stroke: usize,
    p: [f32; 2],
    min_step: f32,
) -> bool {
    let Some(MaskSource::Brush { strokes }) =
        c.mask.components.get_mut(comp).map(|m| &mut m.source)
    else {
        return false;
    };
    let Some(s) = strokes.get_mut(stroke) else {
        return false;
    };
    let moved = s.points.last().is_none_or(|last| {
        ((p[0] - last[0]).powi(2) + (p[1] - last[1]).powi(2)).sqrt() >= min_step
    });
    if moved {
        s.points.push(p);
    }
    moved
}

/// Sets (or creates) the correction's first linear-gradient component.
pub fn set_linear(c: &mut LocalCorrection, p0: [f32; 2], p1: [f32; 2]) -> bool {
    for comp in &mut c.mask.components {
        if let MaskSource::LinearGradient { p0: a, p1: b } = &mut comp.source {
            *a = p0;
            *b = p1;
            return true;
        }
    }
    match add_component(c, NewMask::Linear, Op::Add) {
        Some(i) => {
            c.mask.components[i].source = MaskSource::LinearGradient { p0, p1 };
            true
        }
        None => false,
    }
}

pub fn linear_ends(c: &LocalCorrection) -> Option<([f32; 2], [f32; 2])> {
    c.mask
        .components
        .iter()
        .find_map(|comp| match &comp.source {
            MaskSource::LinearGradient { p0, p1 } => Some((*p0, *p1)),
            _ => None,
        })
}

/// Sets (or creates) the correction's first radial-gradient component's centre and radii, leaving
/// its angle and feather alone.
pub fn set_radial(c: &mut LocalCorrection, center: [f32; 2], radii: [f32; 2]) -> bool {
    for comp in &mut c.mask.components {
        if let MaskSource::RadialGradient {
            center: ce,
            radii: ra,
            ..
        } = &mut comp.source
        {
            *ce = center;
            *ra = radii;
            return true;
        }
    }
    match add_component(c, NewMask::Radial, Op::Add) {
        Some(i) => {
            if let MaskSource::RadialGradient {
                center: ce,
                radii: ra,
                ..
            } = &mut c.mask.components[i].source
            {
                *ce = center;
                *ra = radii;
            }
            true
        }
        None => false,
    }
}

/// `(center, radii, angle_deg, feather)` of the first radial component.
pub fn radial_shape(c: &LocalCorrection) -> Option<([f32; 2], [f32; 2], f32, f32)> {
    c.mask
        .components
        .iter()
        .find_map(|comp| match &comp.source {
            MaskSource::RadialGradient {
                center,
                radii,
                angle_deg,
                feather,
            } => Some((*center, *radii, *angle_deg, *feather)),
            _ => None,
        })
}

/// Adds a sampled Lab colour to the correction's first colour-range component (creating one).
/// Returns false at the sample limit.
pub fn add_color_sample(c: &mut LocalCorrection, lab: [f32; 3]) -> bool {
    let idx = match c
        .mask
        .components
        .iter()
        .position(|comp| matches!(comp.source, MaskSource::ColorRange { .. }))
    {
        Some(i) => i,
        None => match add_component(c, NewMask::Color, Op::Add) {
            Some(i) => i,
            None => return false,
        },
    };
    if let MaskSource::ColorRange { samples, .. } = &mut c.mask.components[idx].source {
        if samples.len() >= MAX_COLOR_SAMPLES {
            return false;
        }
        samples.push(lab);
        return true;
    }
    false
}

/// A small camera-linear thumbnail of the open photo, for colour sampling and range-mask previews.
pub struct Thumb {
    /// Normalized camera-linear RGBA, row-major.
    pub pixels: Vec<[f32; 4]>,
    pub width: usize,
    pub height: usize,
    /// Camera -> working space, as shot (no user white balance).
    pub matrix: Mat3,
}

/// Longest edge of the thumbnail.
pub const THUMB_LONG_EDGE: usize = 96;

impl Thumb {
    /// Averages an evenly strided grid of source pixels per thumbnail pixel (never more than 6x6
    /// taps), so a 45 MP frame costs ~55 k reads.
    pub fn build(frame: Arc<LinearFrame>) -> Self {
        let matrix = color::camera_to_working_space_matrix(
            frame.cam_mul,
            &frame.cam_xyz,
            &WbParams::default(),
        );
        let source = FramePixels(frame);
        let (w, h) = (source.width() as usize, source.height() as usize);
        // Never larger than the source: a tiny frame is its own thumbnail.
        let scale = (THUMB_LONG_EDGE as f32 / w.max(h).max(1) as f32).min(1.0);
        let (tw, th) = (
            ((w as f32 * scale).round() as usize).clamp(1, THUMB_LONG_EDGE),
            ((h as f32 * scale).round() as usize).clamp(1, THUMB_LONG_EDGE),
        );
        let mut pixels = Vec::with_capacity(tw * th);
        for ty in 0..th {
            for tx in 0..tw {
                let (x0, x1) = (tx * w / tw, ((tx + 1) * w / tw).max(tx * w / tw + 1).min(w));
                let (y0, y1) = (ty * h / th, ((ty + 1) * h / th).max(ty * h / th + 1).min(h));
                let step_x = ((x1 - x0) / 6).max(1);
                let step_y = ((y1 - y0) / 6).max(1);
                let mut acc = [0.0f32; 3];
                let mut n = 0.0f32;
                for y in (y0..y1).step_by(step_y) {
                    for x in (x0..x1).step_by(step_x) {
                        let p = source.pixel(x as u32, y as u32);
                        for c in 0..3 {
                            acc[c] += p[c];
                        }
                        n += 1.0;
                    }
                }
                pixels.push([acc[0] / n, acc[1] / n, acc[2] / n, 1.0]);
            }
        }
        Self {
            pixels,
            width: tw,
            height: th,
            matrix,
        }
    }

    /// Lab of the photo at normalized `(nx, ny)`, averaged over a 3x3 window (a click on a noisy
    /// pixel shouldn't pick up the noise).
    pub fn sample_lab(&self, nx: f32, ny: f32) -> [f32; 3] {
        let cx = (nx * self.width as f32).floor() as i64;
        let cy = (ny * self.height as f32).floor() as i64;
        let mut acc = [0.0f32; 3];
        let mut n = 0.0f32;
        for dy in -1..=1i64 {
            for dx in -1..=1i64 {
                let x = (cx + dx).clamp(0, self.width as i64 - 1) as usize;
                let y = (cy + dy).clamp(0, self.height as i64 - 1) as usize;
                let p = self.pixels[y * self.width + x];
                for c in 0..3 {
                    acc[c] += p[c];
                }
                n += 1.0;
            }
        }
        let cam = [acc[0] / n, acc[1] / n, acc[2] / n];
        raster::working_lab(color::mat3_apply(self.matrix, cam))
    }
}

/// The preview size for a frame of `width x height`: long edge `long`, aspect kept.
pub fn preview_extent(source: (f32, f32), long: usize) -> (usize, usize) {
    let scale = long as f32 / source.0.max(source.1).max(1.0);
    (
        ((source.0 * scale).round() as usize).max(1),
        ((source.1 * scale).round() as usize).max(1),
    )
}

/// The correction's mask at `w x h`, composed on the CPU from what the CPU has: geometry is
/// rasterized, an AI component is its finished (unrefined, low-resolution) alpha resampled, a range
/// component is measured on the thumbnail. A component that isn't available yet (a model still
/// running) contributes nothing, exactly as in the real render.
pub fn preview_field(
    c: &LocalCorrection,
    w: usize,
    h: usize,
    develop: &DevelopView,
    thumb: Option<&Thumb>,
) -> Field {
    let neutral = develop.neutral_key();
    let group = c.mask.clone();
    let sanitized = MaskParams {
        corrections: vec![LocalCorrection {
            mask: group,
            ..LocalCorrection::default()
        }],
    }
    .sanitized();
    let group = &sanitized.corrections[0].mask;
    compose::compose(group, w, h, |source| match source {
        MaskSource::Ai(_) => {
            let alpha = ai_bake_key(source, neutral).and_then(|k| develop.ai_alpha(&k))?;
            let low = Field {
                width: alpha.width,
                height: alpha.height,
                data: alpha.alpha.clone(),
            };
            let mut out = Field::new(w, h, 0.0);
            for y in 0..h {
                for x in 0..w {
                    out.data[y * w + x] = sample_mapped(&low, x, y, w, h);
                }
            }
            Some(out)
        }
        MaskSource::LuminanceRange { .. } | MaskSource::ColorRange { .. } => {
            let t = thumb?;
            raster::range_field(source, &t.pixels, t.width, t.height, w, h, t.matrix)
        }
        geometry => raster::rasterize_source(geometry, w, h),
    })
    .expect("every field is produced at the requested extent")
}

/// A key that changes exactly when the preview would: the mask group, and the content of every AI
/// alpha it depends on.
pub fn preview_key(
    c: &LocalCorrection,
    develop: &DevelopView,
    frame_key: u64,
    have_thumb: bool,
) -> blake3::Hash {
    let mut h = blake3::Hasher::new();
    h.update(&frame_key.to_le_bytes());
    h.update(compose::hash_group(&c.mask).as_bytes());
    h.update(&[u8::from(have_thumb)]);
    let neutral = develop.neutral_key();
    for comp in &c.mask.components {
        if let Some(a) = ai_bake_key(&comp.source, neutral).and_then(|k| develop.ai_alpha(&k)) {
            h.update(a.content_hash.as_bytes());
        }
    }
    h.finalize()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nicti_tapetum::mask::params::LocalAdjust;

    fn params(n: usize) -> MaskParams {
        MaskParams {
            corrections: (0..n)
                .map(|i| new_correction(NewMask::Brush, format!("mask-{}", i + 1)))
                .collect(),
        }
    }

    #[test]
    fn every_new_mask_kind_makes_a_valid_correction_and_arms_the_right_tool() {
        for kind in NewMask::ALL {
            let c = new_correction(kind, "mask-1".into());
            assert_eq!(c.id, "mask-1");
            assert!(c.enabled && c.amount == 1.0);
            assert_eq!(c.mask.components.len(), 1);
            assert!(!c.name.is_empty());
            assert!(c.adjust.is_noop(), "a new mask does nothing until adjusted");
            // It survives sanitizing untouched.
            let p = MaskParams {
                corrections: vec![c.clone()],
            };
            assert_eq!(p.sanitized().corrections[0].mask, c.mask, "{kind:?}");
        }
        assert_eq!(NewMask::Brush.arm(), Arm::Brush);
        assert_eq!(NewMask::Linear.arm(), Arm::Linear);
        assert_eq!(NewMask::Radial.arm(), Arm::Radial);
        assert_eq!(NewMask::Color.arm(), Arm::Pick);
        assert_eq!(NewMask::Subject.arm(), Arm::None);
    }

    #[test]
    fn background_is_the_subject_recipe_inverted_so_they_share_one_bake() {
        let subject = new_correction(NewMask::Subject, "a".into());
        let background = new_correction(NewMask::Background, "b".into());
        let (s, b) = (&subject.mask.components[0], &background.mask.components[0]);
        assert_eq!(s.source, b.source, "the same recipe");
        assert!(!s.invert && b.invert);
        let neutral = blake3::hash(b"n");
        assert_eq!(
            ai_bake_key(&s.source, neutral),
            ai_bake_key(&b.source, neutral)
        );
        // Sky uses a different model entirely.
        let sky = new_correction(NewMask::Sky, "c".into());
        assert_ne!(
            ai_bake_key(&sky.mask.components[0].source, neutral),
            ai_bake_key(&s.source, neutral)
        );
    }

    #[test]
    fn ids_are_unique_and_reuse_a_freed_number() {
        let mut p = params(3);
        assert_eq!(next_id(&p), "mask-4");
        delete(&mut p, 0); // frees mask-1
        assert_eq!(next_id(&p), "mask-1");
    }

    #[test]
    fn duplicate_copies_after_the_original_with_a_fresh_id_and_can_invert() {
        let mut p = params(2);
        p.corrections[0].adjust = LocalAdjust {
            exposure: 1.0,
            ..LocalAdjust::default()
        };
        let i = duplicate(&mut p, 0, false).unwrap();
        assert_eq!(i, 1);
        assert_eq!(p.corrections.len(), 3);
        assert_ne!(p.corrections[1].id, p.corrections[0].id);
        assert_eq!(
            p.corrections[1].adjust.exposure, 1.0,
            "adjustments are copied"
        );
        assert!(p.corrections[1].name.ends_with("copy"));

        let j = duplicate(&mut p, 0, true).unwrap();
        assert!(p.corrections[j].mask.components[0].invert);
        assert!(
            !p.corrections[0].mask.components[0].invert,
            "the original is untouched"
        );
        // Ids stay unique across every duplicate.
        let ids: std::collections::HashSet<_> = p.corrections.iter().map(|c| &c.id).collect();
        assert_eq!(ids.len(), p.corrections.len());
        assert!(duplicate(&mut p, 99, false).is_none());
    }

    #[test]
    fn the_correction_and_component_limits_are_enforced() {
        let mut p = params(MAX_CORRECTIONS);
        assert!(add_correction(&mut p, new_correction(NewMask::Brush, "x".into())).is_none());
        assert!(duplicate(&mut p, 0, false).is_none());
        let mut c = new_correction(NewMask::Brush, "y".into());
        while add_component(&mut c, NewMask::Linear, Op::Add).is_some() {}
        assert_eq!(c.mask.components.len(), MAX_COMPONENTS);
    }

    #[test]
    fn delete_ignores_a_bad_index() {
        let mut p = params(2);
        assert!(!delete(&mut p, 5));
        assert!(delete(&mut p, 1));
        assert_eq!(p.corrections.len(), 1);
    }

    #[test]
    fn a_brush_stroke_starts_on_the_first_brush_component_or_makes_one() {
        let brush = BrushSettings::default();
        // A correction with no brush component gets one.
        let mut c = new_correction(NewMask::Radial, "r".into());
        let (comp, stroke) = begin_stroke(&mut c, &brush, false, [0.3, 0.4]).unwrap();
        assert_eq!((comp, stroke), (1, 0));
        assert_eq!(c.mask.components.len(), 2);
        // A second stroke reuses it.
        let (comp2, stroke2) = begin_stroke(&mut c, &brush, true, [0.6, 0.6]).unwrap();
        assert_eq!((comp2, stroke2), (1, 1));
        let MaskSource::Brush { strokes } = &c.mask.components[1].source else {
            panic!()
        };
        assert!(!strokes[0].erase && strokes[1].erase);
        assert_eq!(strokes[1].points, vec![[0.6, 0.6]]);
        assert_eq!(strokes[0].radius, brush.radius);
    }

    #[test]
    fn extending_a_stroke_skips_tiny_moves_and_stays_in_bounds() {
        let brush = BrushSettings::default();
        let mut c = new_correction(NewMask::Brush, "b".into());
        let (comp, stroke) = begin_stroke(&mut c, &brush, false, [0.5, 0.5]).unwrap();
        assert!(
            !extend_stroke(&mut c, comp, stroke, [0.5001, 0.5], 0.01),
            "too small a move"
        );
        assert!(extend_stroke(&mut c, comp, stroke, [0.52, 0.5], 0.01));
        assert!(
            !extend_stroke(&mut c, 9, 0, [0.6, 0.6], 0.0),
            "a bad component is a no-op"
        );
        assert!(
            !extend_stroke(&mut c, comp, 9, [0.6, 0.6], 0.0),
            "a bad stroke is a no-op"
        );
        let MaskSource::Brush { strokes } = &c.mask.components[0].source else {
            panic!()
        };
        assert_eq!(strokes[0].points.len(), 2);
    }

    #[test]
    fn brush_settings_resize_within_limits_and_keep_the_feather_inside_the_radius() {
        let mut b = BrushSettings::default();
        for _ in 0..100 {
            b.resize(1.12);
        }
        assert_eq!(b.radius, MAX_BRUSH_RADIUS);
        assert!(b.feather <= b.radius);
        for _ in 0..200 {
            b.resize(1.0 / 1.12);
        }
        assert_eq!(b.radius, MIN_BRUSH_RADIUS);
        assert!(b.feather <= b.radius);
        b.refeather(50.0);
        assert_eq!(b.feather, b.radius, "feather never exceeds the radius");
        b.refeather(0.0);
        assert_eq!(b.feather, 0.0);
    }

    #[test]
    fn gradients_are_set_in_place_or_created() {
        let mut c = new_correction(NewMask::Brush, "b".into());
        assert!(linear_ends(&c).is_none());
        assert!(set_linear(&mut c, [0.1, 0.1], [0.9, 0.2]));
        assert_eq!(linear_ends(&c), Some(([0.1, 0.1], [0.9, 0.2])));
        assert_eq!(c.mask.components.len(), 2);
        // A second call edits the same component rather than adding another.
        assert!(set_linear(&mut c, [0.0, 0.0], [1.0, 1.0]));
        assert_eq!(c.mask.components.len(), 2);
        assert_eq!(linear_ends(&c), Some(([0.0, 0.0], [1.0, 1.0])));

        assert!(radial_shape(&c).is_none());
        assert!(set_radial(&mut c, [0.4, 0.6], [0.2, 0.1]));
        let (center, radii, angle, feather) = radial_shape(&c).unwrap();
        assert_eq!((center, radii), ([0.4, 0.6], [0.2, 0.1]));
        // Moving the shape keeps the angle and feather the panel set.
        if let MaskSource::RadialGradient {
            angle_deg,
            feather: f,
            ..
        } = &mut c.mask.components[2].source
        {
            *angle_deg = 30.0;
            *f = 0.2;
        }
        assert!(set_radial(&mut c, [0.5, 0.5], [0.3, 0.3]));
        let (_, _, a2, f2) = radial_shape(&c).unwrap();
        assert_eq!((a2, f2), (30.0, 0.2));
        let _ = (angle, feather);
    }

    #[test]
    fn colour_samples_accumulate_up_to_the_limit() {
        let mut c = new_correction(NewMask::Color, "c".into());
        for i in 0..MAX_COLOR_SAMPLES {
            assert!(add_color_sample(&mut c, [50.0, i as f32, 0.0]));
        }
        assert!(!add_color_sample(&mut c, [50.0, 99.0, 0.0]), "at the limit");
        let MaskSource::ColorRange { samples, .. } = &c.mask.components[0].source else {
            panic!()
        };
        assert_eq!(samples.len(), MAX_COLOR_SAMPLES);
        // A correction with no colour component gets one on the first sample.
        let mut b = new_correction(NewMask::Brush, "b".into());
        assert!(add_color_sample(&mut b, [50.0, 0.0, 0.0]));
        assert_eq!(b.mask.components.len(), 2);
    }

    #[test]
    fn a_preview_extent_keeps_the_aspect_ratio() {
        assert_eq!(preview_extent((6000.0, 4000.0), 300), (300, 200));
        assert_eq!(preview_extent((4000.0, 6000.0), 300), (200, 300));
        assert_eq!(preview_extent((1.0, 1.0), 300), (300, 300));
    }

    fn frame(w: u32, h: u32) -> LinearFrame {
        LinearFrame {
            make: "T".into(),
            model: "T".into(),
            width: w,
            height: h,
            black: 0,
            maximum: 1000,
            cam_mul: [1.0; 4],
            pre_mul: [1.0; 4],
            cam_xyz: [
                1.0, 0.0, 0.0, // R
                0.0, 1.0, 0.0, // G
                0.0, 0.0, 1.0, // B
                0.0, 0.0, 0.0,
            ],
            cblack: [0; 4],
            // Left half red-ish, right half blue-ish.
            pixels: (0..w * h)
                .flat_map(|i| {
                    if i % w < w / 2 {
                        [700u16, 200, 100]
                    } else {
                        [100, 200, 700]
                    }
                })
                .collect(),
        }
    }

    #[test]
    fn a_thumbnail_downsamples_a_big_frame_and_samples_distinct_colours() {
        let t = Thumb::build(Arc::new(frame(600, 400)));
        assert_eq!((t.width, t.height), (THUMB_LONG_EDGE, 64));
        let left = t.sample_lab(0.2, 0.5);
        let right = t.sample_lab(0.8, 0.5);
        assert!(
            (left[1] - right[1]).abs() > 20.0 || (left[2] - right[2]).abs() > 20.0,
            "red-ish and blue-ish must differ in Lab: {left:?} vs {right:?}"
        );
        // A sample outside the frame clamps instead of panicking.
        let _ = t.sample_lab(-0.5, 1.5);
        let _ = t.sample_lab(2.0, -1.0);
    }

    #[test]
    fn a_tiny_frame_still_makes_a_thumbnail() {
        let t = Thumb::build(Arc::new(frame(2, 2)));
        assert_eq!((t.width, t.height), (2, 2));
    }
}
