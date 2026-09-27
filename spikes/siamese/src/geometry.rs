//! Vector local-adjustment mask geometry: linear gradient, radial gradient, and brush -- the
//! "design the brush & gradient local-adjustment mask model" half of #48. Per ADR-0021
//! (`docs/adr/0021-non-destructive-edit-model.md:117-123`), a geometry mask stores **vector
//! parameters**, never a derived pixel mask -- these types are exactly that parameter set, and
//! `rasterize()` is the (Tapetum-owned, at render time) function that turns them into a `Field`.
//! darktable's retouch module uses a similar destination-geometry-plus-feather shape (see
//! `docs/adr/0050-healing-and-removal.md`'s Prior art section) but freehand-only; this module
//! covers gradient + brush, matching LRC's own local-adjustment tool set.
//!
//! **Not covered here:** luminance/color-range masks (LRC's `RangeMaskMapInfo`) -- out of #48's
//! literal scope ("brush & gradient"), owned by #49 (see `docs/adr/0048-masking.md`'s
//! Consequences section for why this isn't an orphaned gap).

use serde::{Deserialize, Serialize};

use crate::image::Field;

/// A linear (graduated) gradient: full weight at `p0`, ramping linearly to zero at `p1`, constant
/// along the perpendicular direction -- matches LRC's Linear Gradient tool.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LinearGradient {
    pub p0: (f32, f32),
    pub p1: (f32, f32),
    #[serde(default)]
    pub invert: bool,
}

impl LinearGradient {
    fn weight(&self, x: f32, y: f32) -> f32 {
        let (x0, y0) = self.p0;
        let (x1, y1) = self.p1;
        let (dx, dy) = (x1 - x0, y1 - y0);
        let len_sq = dx * dx + dy * dy;
        let t = if len_sq <= f32::EPSILON {
            0.0
        } else {
            ((x - x0) * dx + (y - y0) * dy) / len_sq
        };
        let w = (1.0 - t.clamp(0.0, 1.0)).clamp(0.0, 1.0);
        if self.invert {
            1.0 - w
        } else {
            w
        }
    }
}

/// A radial (elliptical) gradient: full weight inside the ellipse defined by `center`/`radii`
/// (rotated by `angle` radians), feathered outward over `feather` (in the same units as `radii`)
/// -- matches LRC's Radial Gradient tool. `invert` matches LRC's "Invert Mask" checkbox and, per
/// `docs/adr/0021`/`compose.rs`'s design, is how the hero scenario's "Select Subject, Invert" case
/// is expressed for a geometry mask too (an AI mask's own inverse instead reuses its bake key --
/// see `compose.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RadialGradient {
    pub center: (f32, f32),
    pub radii: (f32, f32),
    pub angle: f32,
    pub feather: f32,
    #[serde(default)]
    pub invert: bool,
}

impl RadialGradient {
    fn weight(&self, x: f32, y: f32) -> f32 {
        let (cx, cy) = self.center;
        let (rx, ry) = self.radii;
        if rx <= 0.0 || ry <= 0.0 {
            return if self.invert { 1.0 } else { 0.0 };
        }
        let (dx, dy) = (x - cx, y - cy);
        let (cos_a, sin_a) = (self.angle.cos(), self.angle.sin());
        // Rotate into the ellipse's own axis-aligned frame before normalizing by its radii.
        let rot_x = dx * cos_a + dy * sin_a;
        let rot_y = -dx * sin_a + dy * cos_a;
        let normalized = ((rot_x / rx).powi(2) + (rot_y / ry).powi(2)).sqrt();
        // `feather` is specified in the same units as `radii` (pixels); convert it to the
        // normalized-ellipse units `normalized` is already in via the mean radius, since a single
        // feather width has to work along both axes of a (possibly very eccentric) ellipse.
        let mean_radius = (rx + ry) * 0.5;
        let feather_norm = if mean_radius > 0.0 {
            (self.feather / mean_radius).max(1e-6)
        } else {
            1e-6
        };
        let w = (1.0 - (normalized - 1.0) / feather_norm).clamp(0.0, 1.0);
        if self.invert {
            1.0 - w
        } else {
            w
        }
    }
}

/// One brush dab: a soft circle, feathered from `radius - feather` to `radius`, scaled by `flow`
/// (per-stroke opacity multiplier, LRC's "Flow" slider) and blended via `max` across dabs within a
/// stroke and across strokes (matches a real brush tool: overlapping dabs don't double-darken).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Dab {
    pub center: (f32, f32),
    pub radius: f32,
    pub feather: f32,
    pub flow: f32,
}

impl Dab {
    fn weight(&self, x: f32, y: f32) -> f32 {
        let (cx, cy) = self.center;
        let dist = ((x - cx).powi(2) + (y - cy).powi(2)).sqrt();
        if dist >= self.radius {
            return 0.0;
        }
        let inner = (self.radius - self.feather).max(0.0);
        let w = if dist <= inner {
            1.0
        } else {
            1.0 - (dist - inner) / (self.radius - inner).max(1e-6)
        };
        (w * self.flow).clamp(0.0, 1.0)
    }
}

/// A brush stroke: an ordered list of dabs (one per recorded input sample along the drag), plus
/// whether this stroke erases (subtracts) rather than adds -- LRC's "Erase" sub-tool within Brush.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Stroke {
    pub dabs: Vec<Dab>,
    #[serde(default)]
    pub erase: bool,
}

/// One piece of vector geometry a `compose::MaskComponent` can carry. `Brush` is a `Vec<Stroke>`
/// (not a single stroke) so a whole brush mask -- built from many add/erase strokes over an
/// editing session -- is one geometry value, matching how `compose::MaskComponent` treats "AI
/// recipe" and "geometry" as the two source kinds (ADR-0021's rule), not one component per stroke.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Geometry {
    LinearGradient(LinearGradient),
    RadialGradient(RadialGradient),
    Brush(Vec<Stroke>),
}

impl Geometry {
    /// Rasterizes this geometry to a `Field` at `width`x`height` -- the CPU reference every other
    /// implementation (the `gpu` module's WGSL twin) is checked against.
    pub fn rasterize(&self, width: usize, height: usize) -> Field {
        let mut field = Field::new(width, height, 0.0);
        match self {
            Geometry::LinearGradient(g) => {
                for y in 0..height {
                    for x in 0..width {
                        field.data[y * width + x] = g.weight(x as f32 + 0.5, y as f32 + 0.5);
                    }
                }
            }
            Geometry::RadialGradient(g) => {
                for y in 0..height {
                    for x in 0..width {
                        field.data[y * width + x] = g.weight(x as f32 + 0.5, y as f32 + 0.5);
                    }
                }
            }
            Geometry::Brush(strokes) => {
                for stroke in strokes {
                    for y in 0..height {
                        for x in 0..width {
                            let mut best = 0.0f32;
                            for dab in &stroke.dabs {
                                best = best.max(dab.weight(x as f32 + 0.5, y as f32 + 0.5));
                            }
                            let i = y * width + x;
                            if stroke.erase {
                                field.data[i] = (field.data[i] - best).max(0.0);
                            } else {
                                field.data[i] = field.data[i].max(best);
                            }
                        }
                    }
                }
            }
        }
        field
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linear_gradient_is_full_at_p0_and_zero_at_p1() {
        let g = LinearGradient {
            p0: (0.0, 50.0),
            p1: (100.0, 50.0),
            invert: false,
        };
        let field = Geometry::LinearGradient(g).rasterize(101, 101);
        // Pixel centers sit at integer+0.5, so pixel 0 (center 0.5) is a hair past p0=(0,50), not
        // exactly on it -- assert "close to 1.0", not bit-exact.
        assert!((field.get(0, 50) - 1.0).abs() < 1e-2);
        assert!(field.get(100, 50) < 1e-2);
        // Midpoint is roughly half-weighted.
        let mid = field.get(50, 50);
        assert!((mid - 0.5).abs() < 0.1);
    }

    #[test]
    fn linear_gradient_invert_flips_the_weight() {
        let g = LinearGradient {
            p0: (0.0, 0.0),
            p1: (10.0, 0.0),
            invert: false,
        };
        let inverted = LinearGradient { invert: true, ..g };
        let a = Geometry::LinearGradient(g).rasterize(11, 1);
        let b = Geometry::LinearGradient(inverted).rasterize(11, 1);
        for i in 0..a.data.len() {
            assert!((a.data[i] + b.data[i] - 1.0).abs() < 1e-4);
        }
    }

    #[test]
    fn radial_gradient_is_full_at_center_and_fades_outward() {
        let g = RadialGradient {
            center: (50.0, 50.0),
            radii: (20.0, 20.0),
            angle: 0.0,
            feather: 5.0,
            invert: false,
        };
        let field = Geometry::RadialGradient(g).rasterize(101, 101);
        assert!((field.get(50, 50) - 1.0).abs() < 1e-3);
        assert!(field.get(0, 0) < 1e-3);
    }

    #[test]
    fn brush_dab_blends_with_max_not_addition() {
        let stroke = Stroke {
            dabs: vec![
                Dab {
                    center: (10.0, 10.0),
                    radius: 8.0,
                    feather: 2.0,
                    flow: 1.0,
                },
                Dab {
                    center: (12.0, 10.0),
                    radius: 8.0,
                    feather: 2.0,
                    flow: 1.0,
                },
            ],
            erase: false,
        };
        let geometry = Geometry::Brush(vec![stroke]);
        let field = geometry.rasterize(20, 20);
        // The overlap region between both dabs must not exceed 1.0 (would, under naive addition).
        assert!(field.get(11, 10) <= 1.0 + 1e-5);
    }

    #[test]
    fn erase_stroke_subtracts_from_a_prior_add_stroke() {
        let add = Stroke {
            dabs: vec![Dab {
                center: (10.0, 10.0),
                radius: 8.0,
                feather: 0.0,
                flow: 1.0,
            }],
            erase: false,
        };
        let erase = Stroke {
            dabs: vec![Dab {
                center: (10.0, 10.0),
                radius: 8.0,
                feather: 0.0,
                flow: 1.0,
            }],
            erase: true,
        };
        let geometry = Geometry::Brush(vec![add, erase]);
        let field = geometry.rasterize(20, 20);
        assert!(field.get(10, 10) < 1e-5);
    }

    #[test]
    fn geometry_round_trips_through_json() {
        let g = Geometry::RadialGradient(RadialGradient {
            center: (1.0, 2.0),
            radii: (3.0, 4.0),
            angle: 0.5,
            feather: 1.5,
            invert: true,
        });
        let json = serde_json::to_string(&g).unwrap();
        let back: Geometry = serde_json::from_str(&json).unwrap();
        assert_eq!(g, back);
    }
}
