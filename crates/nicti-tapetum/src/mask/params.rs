//! The `nicti.masks` stage's typed params (#49, ADR-0048/0049): every local correction the user has
//! made, each a [`MaskGroup`] (where the correction applies) plus a [`LocalAdjust`] (what it does).
//!
//! The group/component names match LRC's own `MaskGroupBasedCorrections` shape (ADR-0061) so the
//! catalog importer (#62) maps across directly. Per ADR-0021 a mask stores *vector parameters or a
//! model recipe, never derived pixels*.
//!
//! Conventions, all deliberate:
//! - **Normalized coordinates.** Points are `x / width, y / height` of the *uncropped source frame*
//!   and lengths (radius, feather) are fractions of its *long edge*. A mask therefore means the
//!   same thing at any resolution and survives a crop change, a paste onto a differently sized
//!   image (#52) and an LRC import (#62). (Heal's spots use source pixels; masks deliberately don't.)
//! - **Every `Option` uses `skip_serializing_if`**, because `nicti_pawprint::hash_value` refuses JSON
//!   `null` (a NaN also serializes as `null`, hence [`MaskParams::sanitized`]).
//! - **Documents are untrusted** (a synced or imported edit): counts and magnitudes are clamped by
//!   `sanitized` before anything renders, never a panic and never an unbounded allocation.

use serde::{Deserialize, Serialize};

use crate::coat::MaskRecipe;

/// Most corrections one photo renders. The rest are ignored (and the UI refuses to add more).
pub const MAX_CORRECTIONS: usize = 16;
/// Most components one mask group renders.
pub const MAX_COMPONENTS: usize = 16;
/// Most brush points summed over the whole document (a point is one recorded input sample).
pub const MAX_BRUSH_POINTS: usize = 200_000;
/// Most strokes one brush component renders.
pub const MAX_STROKES: usize = 4096;
/// Most colour-range samples one component keeps.
pub const MAX_COLOR_SAMPLES: usize = 16;
/// Normalized coordinates are clamped to `-COORD_LIMIT..=COORD_LIMIT` -- a little past the frame so
/// a gradient handle can sit off-image, but bounded so a hostile value can't overflow the rasters.
pub const COORD_LIMIT: f32 = 4.0;
/// Longest allowed radius/feather, as a fraction of the long edge.
pub const LENGTH_LIMIT: f32 = 4.0;
/// Shortest radius (fraction of the long edge) a dab or ellipse may have; below this it is a no-op.
pub const MIN_RADIUS: f32 = 1e-5;
/// Exposure is clamped to +-`EXPOSURE_LIMIT` stops, like the global slider.
pub const EXPOSURE_LIMIT: f32 = 5.0;

/// How a component's weight combines with the running composite so far.
///
/// The fold math is *amended* from the research spike (ADR-0049): `Add` is a union (`max`), not
/// `min(a + w, 1)`, which double-counted two overlapping feathered edges into a hard seam.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    /// `max(acc, w)` -- union.
    #[default]
    Add,
    /// `acc * (1 - w)` -- cut the component out of what is selected so far.
    Subtract,
    /// `acc * w` -- keep only where both are selected.
    Intersect,
}

/// One brush stroke: a polyline of input samples with one radius/feather/flow for the whole stroke.
/// Dabs are *derived* from the polyline (see `raster::dabs_for_stroke`), which keeps a document
/// small -- a stored dab per sample would be 100 KB+ for one brush mask (ADR-0021 sizing).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Stroke {
    pub points: Vec<[f32; 2]>,
    /// Fraction of the long edge.
    pub radius: f32,
    /// Width of the soft edge, fraction of the long edge (clamped to the radius).
    pub feather: f32,
    /// Per-stroke opacity, 0..=1.
    pub flow: f32,
    /// True: this stroke removes from the mask rather than adding to it.
    pub erase: bool,
}

impl Default for Stroke {
    fn default() -> Self {
        Self {
            points: Vec::new(),
            radius: 0.02,
            feather: 0.01,
            flow: 1.0,
            erase: false,
        }
    }
}

/// Where one component's raw weight field comes from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MaskSource {
    /// A model's alpha, identified by recipe (`model_id`, pinned `model_version`, `params`).
    /// Subject, background (= subject with `invert`) and sky are all this.
    Ai(MaskRecipe),
    /// Full weight at `p0`, ramping linearly to zero at `p1`.
    LinearGradient { p0: [f32; 2], p1: [f32; 2] },
    /// Full weight inside the ellipse, feathered outward.
    RadialGradient {
        center: [f32; 2],
        /// Semi-axes, fraction of the long edge.
        radii: [f32; 2],
        angle_deg: f32,
        /// Soft edge width, fraction of the long edge.
        feather: f32,
    },
    /// Hand-painted strokes, applied in order.
    Brush { strokes: Vec<Stroke> },
    /// Selects by luminance: full weight in `lo..=hi`, ramping to zero over `smooth` either side.
    LuminanceRange { lo: f32, hi: f32, smooth: f32 },
    /// Selects pixels near any of the sampled colours (Lab, in the neutral render's space).
    ColorRange {
        samples: Vec<[f32; 3]>,
        /// Lab distance at which the weight reaches zero.
        tolerance: f32,
    },
}

impl MaskSource {
    /// The model recipe if this component needs an AI bake.
    pub fn recipe(&self) -> Option<&MaskRecipe> {
        match self {
            MaskSource::Ai(r) => Some(r),
            _ => None,
        }
    }
}

fn one() -> f32 {
    1.0
}

/// One component of a [`MaskGroup`]. `invert` flips the source's raw weight (`1 - w`) *before*
/// `opacity` and `op`; it is how "Select Background" is expressed for an AI source without a second
/// model run (`compose::ai_bake_key` ignores it).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MaskComponent {
    pub source: MaskSource,
    pub op: Op,
    pub invert: bool,
    #[serde(default = "one")]
    pub opacity: f32,
}

impl Default for MaskComponent {
    fn default() -> Self {
        Self {
            source: MaskSource::Brush {
                strokes: Vec::new(),
            },
            op: Op::Add,
            invert: false,
            opacity: 1.0,
        }
    }
}

/// An ordered composition of components, folded left to right (see `compose::fold`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MaskGroup {
    pub components: Vec<MaskComponent>,
}

/// Colour overlay applied inside a mask (LRC's local "Color" swatch).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TintColor {
    /// Hue, degrees 0..360.
    pub hue_deg: f32,
    /// 0..=1. 0 is no tint.
    pub saturation: f32,
}

/// The adjustments a correction applies *inside its mask*. Every field is a no-op at its default
/// (0). Ranges follow the global sliders in `coat.rs`: `exposure` in stops (-5..=5), every other
/// slider normalized -1..=1 (LRC's -100..=100). The effective value at a pixel is the global value
/// plus `mask weight * amount * delta`, which is also how LRC stacks local corrections.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LocalAdjust {
    pub exposure: f32,
    pub contrast: f32,
    pub highlights: f32,
    pub shadows: f32,
    pub whites: f32,
    pub blacks: f32,
    pub temp: f32,
    pub tint: f32,
    pub saturation: f32,
    pub hue: f32,
    pub clarity: f32,
    pub texture: f32,
    pub dehaze: f32,
    pub sharpness: f32,
    pub noise: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<TintColor>,
}

impl LocalAdjust {
    /// True when this adjustment changes nothing (the correction is then skipped entirely).
    pub fn is_noop(&self) -> bool {
        let tint_zero = self.color.map(|c| c.saturation == 0.0).unwrap_or(true);
        tint_zero
            && self.exposure == 0.0
            && self.contrast == 0.0
            && self.highlights == 0.0
            && self.shadows == 0.0
            && self.whites == 0.0
            && self.blacks == 0.0
            && self.temp == 0.0
            && self.tint == 0.0
            && self.saturation == 0.0
            && self.hue == 0.0
            && self.clarity == 0.0
            && self.texture == 0.0
            && self.dehaze == 0.0
            && self.sharpness == 0.0
            && self.noise == 0.0
    }

    /// True when any adjustment needs a spatial pass (a cached detail base) rather than being
    /// pointwise -- the cost gate in `engine`.
    pub fn needs_spatial(&self) -> bool {
        self.clarity != 0.0 || self.texture != 0.0 || self.dehaze != 0.0
    }
}

/// One local correction: a named mask plus its adjustments, scaled by `amount`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LocalCorrection {
    /// Stable identity (survives reordering and rename); the UI generates it.
    pub id: String,
    pub name: String,
    pub enabled: bool,
    /// LRC's per-mask Amount slider, 0..=1 here.
    #[serde(default = "one")]
    pub amount: f32,
    pub mask: MaskGroup,
    pub adjust: LocalAdjust,
}

impl Default for LocalCorrection {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            enabled: true,
            amount: 1.0,
            mask: MaskGroup::default(),
            adjust: LocalAdjust::default(),
        }
    }
}

/// The `nicti.masks` stage's whole params.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MaskParams {
    pub corrections: Vec<LocalCorrection>,
}

fn clean(v: f32, lo: f32, hi: f32) -> f32 {
    if v.is_finite() {
        v.clamp(lo, hi)
    } else {
        0.0
    }
}

fn clean_point(p: [f32; 2]) -> [f32; 2] {
    [
        clean(p[0], -COORD_LIMIT, COORD_LIMIT),
        clean(p[1], -COORD_LIMIT, COORD_LIMIT),
    ]
}

impl MaskSource {
    fn sanitize(&mut self, brush_budget: &mut usize) {
        match self {
            MaskSource::Ai(_) => {}
            MaskSource::LinearGradient { p0, p1 } => {
                *p0 = clean_point(*p0);
                *p1 = clean_point(*p1);
            }
            MaskSource::RadialGradient {
                center,
                radii,
                angle_deg,
                feather,
            } => {
                *center = clean_point(*center);
                *radii = [
                    clean(radii[0], 0.0, LENGTH_LIMIT),
                    clean(radii[1], 0.0, LENGTH_LIMIT),
                ];
                *angle_deg = clean(*angle_deg, -720.0, 720.0);
                *feather = clean(*feather, 0.0, LENGTH_LIMIT);
            }
            MaskSource::Brush { strokes } => {
                strokes.truncate(MAX_STROKES);
                for stroke in strokes.iter_mut() {
                    let room = (*brush_budget).min(stroke.points.len());
                    stroke.points.truncate(room);
                    *brush_budget -= room;
                    for p in &mut stroke.points {
                        *p = clean_point(*p);
                    }
                    stroke.radius = clean(stroke.radius, 0.0, LENGTH_LIMIT);
                    stroke.feather = clean(stroke.feather, 0.0, LENGTH_LIMIT);
                    stroke.flow = clean(stroke.flow, 0.0, 1.0);
                }
            }
            MaskSource::LuminanceRange { lo, hi, smooth } => {
                *lo = clean(*lo, 0.0, 1.0);
                *hi = clean(*hi, 0.0, 1.0);
                if *hi < *lo {
                    std::mem::swap(lo, hi);
                }
                *smooth = clean(*smooth, 0.0, 1.0);
            }
            MaskSource::ColorRange { samples, tolerance } => {
                samples.truncate(MAX_COLOR_SAMPLES);
                for s in samples.iter_mut() {
                    *s = [
                        clean(s[0], 0.0, 100.0),
                        clean(s[1], -200.0, 200.0),
                        clean(s[2], -200.0, 200.0),
                    ];
                }
                *tolerance = clean(*tolerance, 0.0, 200.0);
            }
        }
    }
}

impl LocalAdjust {
    fn sanitize(&mut self) {
        self.exposure = clean(self.exposure, -EXPOSURE_LIMIT, EXPOSURE_LIMIT);
        for v in [
            &mut self.contrast,
            &mut self.highlights,
            &mut self.shadows,
            &mut self.whites,
            &mut self.blacks,
            &mut self.temp,
            &mut self.tint,
            &mut self.saturation,
            &mut self.hue,
            &mut self.clarity,
            &mut self.texture,
            &mut self.dehaze,
            &mut self.sharpness,
            &mut self.noise,
        ] {
            *v = clean(*v, -1.0, 1.0);
        }
        if let Some(c) = &mut self.color {
            c.hue_deg = clean(c.hue_deg, 0.0, 360.0);
            c.saturation = clean(c.saturation, 0.0, 1.0);
        }
    }
}

impl MaskParams {
    /// A copy that is always safe to render: counts capped, every number finite and in range.
    /// Anything over a limit is dropped, not rejected -- a document from a newer build or another
    /// machine still opens, it just renders the part this build accepts.
    pub fn sanitized(&self) -> MaskParams {
        let mut out = self.clone();
        out.corrections.truncate(MAX_CORRECTIONS);
        let mut brush_budget = MAX_BRUSH_POINTS;
        for c in &mut out.corrections {
            c.amount = clean(c.amount, 0.0, 1.0);
            c.adjust.sanitize();
            c.mask.components.truncate(MAX_COMPONENTS);
            for comp in &mut c.mask.components {
                comp.opacity = clean(comp.opacity, 0.0, 1.0);
                comp.source.sanitize(&mut brush_budget);
            }
        }
        out
    }

    /// Corrections that actually change pixels: enabled, non-zero amount, a non-noop adjustment and
    /// at least one component. Order is preserved (LRC stacks in list order).
    pub fn active(&self) -> impl Iterator<Item = &LocalCorrection> {
        self.corrections.iter().filter(|c| {
            c.enabled && c.amount > 0.0 && !c.mask.components.is_empty() && !c.adjust.is_noop()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn brush(n: usize) -> MaskSource {
        MaskSource::Brush {
            strokes: vec![Stroke {
                points: vec![[0.5, 0.5]; n],
                ..Stroke::default()
            }],
        }
    }

    fn correction(source: MaskSource) -> LocalCorrection {
        LocalCorrection {
            id: "a".into(),
            name: "A".into(),
            mask: MaskGroup {
                components: vec![MaskComponent {
                    source,
                    ..MaskComponent::default()
                }],
            },
            adjust: LocalAdjust {
                exposure: 1.0,
                ..LocalAdjust::default()
            },
            ..LocalCorrection::default()
        }
    }

    #[test]
    fn a_full_document_round_trips_through_json_and_hashes_without_null() {
        let params = MaskParams {
            corrections: vec![
                correction(MaskSource::Ai(MaskRecipe {
                    model_id: "nicti.mask.birefnet".into(),
                    model_version: "1".into(),
                    params: json!({ "target": "subject" }),
                    seed: None,
                })),
                correction(MaskSource::LinearGradient {
                    p0: [0.0, 0.0],
                    p1: [0.0, 1.0],
                }),
                correction(MaskSource::RadialGradient {
                    center: [0.5, 0.5],
                    radii: [0.2, 0.1],
                    angle_deg: 30.0,
                    feather: 0.05,
                }),
                correction(brush(3)),
                correction(MaskSource::LuminanceRange {
                    lo: 0.2,
                    hi: 0.8,
                    smooth: 0.1,
                }),
                correction(MaskSource::ColorRange {
                    samples: vec![[50.0, 10.0, -10.0]],
                    tolerance: 20.0,
                }),
            ],
        };
        let json = serde_json::to_value(&params).unwrap();
        let back: MaskParams = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(params, back);
        nicti_pawprint::hash_value(&json).expect("no JSON null anywhere in a mask document");
    }

    #[test]
    fn an_empty_or_partial_entry_parses_to_defaults() {
        let p: MaskParams = serde_json::from_value(json!({})).unwrap();
        assert!(p.corrections.is_empty());
        let p: MaskParams = serde_json::from_value(json!({
            "corrections": [{ "id": "x", "mask": { "components": [{
                "source": { "kind": "linear_gradient", "p0": [0, 0], "p1": [1, 0] } }] } }]
        }))
        .unwrap();
        let c = &p.corrections[0];
        assert!(c.enabled, "enabled defaults to true");
        assert_eq!(c.amount, 1.0);
        assert_eq!(c.mask.components[0].opacity, 1.0);
        assert_eq!(c.mask.components[0].op, Op::Add);
    }

    #[test]
    fn sanitizing_caps_counts_and_scrubs_non_finite_numbers() {
        let mut params = MaskParams {
            corrections: (0..MAX_CORRECTIONS + 5)
                .map(|_| correction(brush(2)))
                .collect(),
        };
        params.corrections[0].adjust.exposure = f32::NAN;
        params.corrections[0].amount = f32::INFINITY;
        params.corrections[0].mask.components[0].opacity = 9.0;
        params.corrections[1].mask.components = (0..MAX_COMPONENTS + 3)
            .map(|_| MaskComponent::default())
            .collect();
        params.corrections[2].mask.components[0].source = MaskSource::LinearGradient {
            p0: [f32::NAN, 1e9],
            p1: [0.0, -1e9],
        };
        let s = params.sanitized();
        assert_eq!(s.corrections.len(), MAX_CORRECTIONS);
        assert_eq!(s.corrections[0].adjust.exposure, 0.0);
        assert_eq!(s.corrections[0].amount, 0.0);
        assert_eq!(s.corrections[0].mask.components[0].opacity, 1.0);
        assert_eq!(s.corrections[1].mask.components.len(), MAX_COMPONENTS);
        match &s.corrections[2].mask.components[0].source {
            MaskSource::LinearGradient { p0, p1 } => {
                assert_eq!(*p0, [0.0, COORD_LIMIT]);
                assert_eq!(*p1, [0.0, -COORD_LIMIT]);
            }
            other => panic!("{other:?}"),
        }
        nicti_pawprint::hash_value(&s).expect("a sanitized document always hashes");
    }

    #[test]
    fn the_brush_point_budget_is_shared_across_the_whole_document() {
        let params = MaskParams {
            corrections: vec![correction(brush(MAX_BRUSH_POINTS)), correction(brush(500))],
        };
        let s = params.sanitized();
        let points = |c: &LocalCorrection| match &c.mask.components[0].source {
            MaskSource::Brush { strokes } => strokes.iter().map(|s| s.points.len()).sum::<usize>(),
            _ => unreachable!(),
        };
        assert_eq!(points(&s.corrections[0]), MAX_BRUSH_POINTS);
        assert_eq!(points(&s.corrections[1]), 0, "the budget is spent");
    }

    #[test]
    fn a_range_with_reversed_bounds_is_swapped() {
        let mut p = MaskParams {
            corrections: vec![correction(MaskSource::LuminanceRange {
                lo: 0.9,
                hi: 0.1,
                smooth: 0.0,
            })],
        };
        p = p.sanitized();
        match &p.corrections[0].mask.components[0].source {
            MaskSource::LuminanceRange { lo, hi, .. } => assert!(lo <= hi),
            _ => unreachable!(),
        }
    }

    #[test]
    fn active_skips_disabled_empty_and_noop_corrections() {
        let mut disabled = correction(brush(1));
        disabled.enabled = false;
        let mut zero_amount = correction(brush(1));
        zero_amount.amount = 0.0;
        let mut empty = correction(brush(1));
        empty.mask.components.clear();
        let mut noop = correction(brush(1));
        noop.adjust = LocalAdjust::default();
        let live = correction(brush(1));
        let p = MaskParams {
            corrections: vec![disabled, zero_amount, empty, noop, live.clone()],
        };
        let active: Vec<_> = p.active().collect();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0], &live);
    }

    #[test]
    fn a_tint_with_zero_saturation_is_still_a_noop() {
        let a = LocalAdjust {
            color: Some(TintColor {
                hue_deg: 120.0,
                saturation: 0.0,
            }),
            ..LocalAdjust::default()
        };
        assert!(a.is_noop());
        assert!(!LocalAdjust {
            clarity: 0.1,
            ..LocalAdjust::default()
        }
        .is_noop());
        assert!(LocalAdjust {
            dehaze: 0.1,
            ..LocalAdjust::default()
        }
        .needs_spatial());
        assert!(!LocalAdjust {
            exposure: 1.0,
            ..LocalAdjust::default()
        }
        .needs_spatial());
    }
}
