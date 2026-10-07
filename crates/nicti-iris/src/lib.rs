//! Lens-correction extension point (ADR-0019 §7/§8) and, since #428, the data model and the
//! pure-CPU math the Tapetum lens stage consumes.
//!
//! `LensCorrection` settles identity/versioning via `Module` and now has one method,
//! [`LensCorrection::model`], returning a [`LensModel`]: plain data (per-plane warp, radial
//! vignette, optical centre) that a GPU kernel can upload without knowing where it came from.
//! The first provider is [`dng::DngEmbedded`] (OpcodeList3 from a DNG). A lensfun- or NEF-backed
//! provider is #410's scope and plugs in behind the same trait.
//!
//! [`lateral_ca`] holds the automatic lateral-chromatic-aberration estimator, the no-profile
//! fallback.

pub mod dng;
pub mod lateral_ca;

use nicti_claw::{Module, Registry};

/// DNG `WarpRectilinear`: per colour plane `[kr0, kr1, kr2, kr3, kt0, kt1]` (radial polynomial
/// then tangential terms) plus the optical centre, normalised 0..1 over the image.
#[derive(Clone, Debug, PartialEq)]
pub struct Warp {
    /// One entry per plane (1..=4). A single entry applies to every channel; otherwise plane 0/1/2
    /// are R/G/B.
    pub planes: Vec<[f64; 6]>,
    pub center: [f64; 2],
}

/// DNG `FixVignetteRadial`: gain `1 + k0 r² + k1 r⁴ + k2 r⁶ + k3 r⁸ + k4 r¹⁰`, `r` normalised so
/// the farthest image corner is 1.
#[derive(Clone, Debug, PartialEq)]
pub struct Vignette {
    pub k: [f64; 5],
    pub center: [f64; 2],
}

/// Everything a lens profile says about one image. Empty (`Default`) means "no correction".
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LensModel {
    pub warp: Option<Warp>,
    pub vignette: Option<Vignette>,
}

impl Warp {
    /// The coefficients for colour channel `ch` (0=R, 1=G, 2=B); a one-plane warp serves all.
    pub fn plane(&self, ch: usize) -> &[f64; 6] {
        self.planes.get(ch).unwrap_or(&self.planes[0])
    }

    /// True when the planes differ in a way that matters, i.e. the profile already corrects lateral
    /// CA. Compared with a tolerance: a profile whose planes differ by float noise (1e-12) has no
    /// per-channel correction, and treating it as having one would disable auto-CA for nothing.
    pub fn corrects_lateral_ca(&self) -> bool {
        const TOLERANCE: f64 = 1.0e-6;
        self.planes.iter().skip(1).any(|p| {
            p.iter()
                .zip(&self.planes[0])
                .any(|(a, b)| (a - b).abs() > TOLERANCE)
        })
    }

    /// Map a corrected-image point to the source point it samples. `dx`/`dy` are the offset from
    /// the optical centre in pixels, `m` the centre-to-farthest-corner distance in pixels.
    /// Returns the *source* offset from the centre in pixels.
    pub fn source_offset(&self, ch: usize, dx: f64, dy: f64, m: f64) -> (f64, f64) {
        let [kr0, kr1, kr2, kr3, kt0, kt1] = *self.plane(ch);
        let (x, y) = (dx / m, dy / m);
        let r2 = x * x + y * y;
        let f = kr0 + r2 * (kr1 + r2 * (kr2 + r2 * kr3));
        let sx = f * x + 2.0 * kt0 * x * y + kt1 * (r2 + 2.0 * x * x);
        let sy = f * y + kt0 * (r2 + 2.0 * y * y) + 2.0 * kt1 * x * y;
        (sx * m, sy * m)
    }
}

impl Vignette {
    /// Multiplicative gain at squared normalised radius `r2`.
    pub fn gain(&self, r2: f64) -> f64 {
        let [k0, k1, k2, k3, k4] = self.k;
        1.0 + r2 * (k0 + r2 * (k1 + r2 * (k2 + r2 * (k3 + r2 * k4))))
    }
}

/// Distance from `(cx, cy)` to the farthest corner of a `w`×`h` image, in pixels. The DNG
/// normalisation radius `m`; never zero.
pub fn farthest_corner(w: f64, h: f64, cx: f64, cy: f64) -> f64 {
    let dx = cx.max(w - cx);
    let dy = cy.max(h - cy);
    dx.hypot(dy).max(1.0)
}

/// What a provider is given to identify a correction. Grows with new sources (#410).
#[derive(Clone, Copy, Debug, Default)]
pub struct LensSource<'a> {
    pub make: &'a str,
    pub model: &'a str,
    /// The raw DNG `OpcodeList3` blob, when the file is a DNG that carries one.
    pub dng_opcode_list3: Option<&'a [u8]>,
}

/// A lens-correction data provider.
pub trait LensCorrection: Module {
    /// The correction for this image, or `None` when the provider has nothing for it.
    fn model(&self, source: &LensSource<'_>) -> Option<LensModel>;
}

/// Registry of lens-correction modules, keyed by namespaced id.
pub type LensRegistry = Registry<dyn LensCorrection>;

#[cfg(test)]
mod tests {
    use super::*;
    use nicti_claw::Descriptor;
    use serde_json::Value;
    use std::sync::Arc;

    struct Dummy;

    impl Module for Dummy {
        fn id(&self) -> &str {
            "nicti.lens.dummy"
        }

        fn schema_version(&self) -> u32 {
            1
        }

        fn migrate_params(&self, _from_version: u32, params: Value) -> Option<Value> {
            Some(params)
        }
    }

    impl LensCorrection for Dummy {
        fn model(&self, _source: &LensSource<'_>) -> Option<LensModel> {
            None
        }
    }

    fn make_dummy() -> Arc<dyn LensCorrection> {
        Arc::new(Dummy)
    }

    #[test]
    fn dummy_registers_and_resolves_as_trait_object() {
        let mut registry: LensRegistry = Registry::new();
        registry
            .register(
                Descriptor {
                    id: "nicti.lens.dummy",
                    schema_version: 1,
                },
                make_dummy,
            )
            .expect("registration should succeed");

        let resolved = registry
            .get("nicti.lens.dummy")
            .expect("dummy lens module is registered");
        assert_eq!(resolved.id(), "nicti.lens.dummy");
        assert!(resolved.model(&LensSource::default()).is_none());
    }

    #[test]
    fn identity_warp_maps_every_point_to_itself() {
        let w = Warp {
            planes: vec![[1.0, 0.0, 0.0, 0.0, 0.0, 0.0]],
            center: [0.5, 0.5],
        };
        for (dx, dy) in [(0.0, 0.0), (120.0, -40.0), (-300.0, 250.0)] {
            let (sx, sy) = w.source_offset(1, dx, dy, 500.0);
            assert!((sx - dx).abs() < 1e-9 && (sy - dy).abs() < 1e-9);
        }
    }

    #[test]
    fn radial_term_scales_with_radius_and_tangential_is_zero_on_axis() {
        let w = Warp {
            planes: vec![[1.0, 0.1, 0.0, 0.0, 0.0, 0.0]],
            center: [0.5, 0.5],
        };
        // On the x axis at r = 1 (the corner distance): f = 1.1.
        let (sx, sy) = w.source_offset(0, 500.0, 0.0, 500.0);
        assert!((sx - 550.0).abs() < 1e-9 && sy.abs() < 1e-9);
        // The centre never moves.
        assert_eq!(w.source_offset(0, 0.0, 0.0, 500.0), (0.0, 0.0));
    }

    #[test]
    fn per_plane_warp_reports_lateral_ca_only_when_planes_differ() {
        let same = Warp {
            planes: vec![[1.0; 6]; 3],
            center: [0.5; 2],
        };
        assert!(!same.corrects_lateral_ca());
        let mut diff = same.clone();
        diff.planes[0][0] = 1.001;
        assert!(diff.corrects_lateral_ca());
        // A single-plane warp serves all channels.
        let one = Warp {
            planes: vec![[2.0, 0.0, 0.0, 0.0, 0.0, 0.0]],
            center: [0.5; 2],
        };
        assert_eq!(one.plane(2), &[2.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        assert!(!one.corrects_lateral_ca());
    }

    #[test]
    fn planes_that_differ_only_by_float_noise_do_not_count_as_ca_correction() {
        let mut w = Warp {
            planes: vec![[1.0, 0.01, 0.0, 0.0, 0.0, 0.0]; 3],
            center: [0.5; 2],
        };
        w.planes[2][0] += 1.0e-12;
        assert!(!w.corrects_lateral_ca());
        w.planes[2][0] += 1.0e-3;
        assert!(w.corrects_lateral_ca());
    }

    /// Hand-computed from the DNG specification's equations, not from this crate's code: x = 0.5,
    /// y = 0.25 (m = 1), kr = [1, 0.1, 0, 0], kt = [0.01, 0.02]:
    ///   r2 = 0.3125, f = 1 + 0.1 * 0.3125 = 1.03125
    ///   x' = f*x + 2*kt0*x*y + kt1*(r2 + 2*x^2) = 0.515625 + 0.0025 + 0.01625 = 0.534375
    ///   y' = f*y + kt0*(r2 + 2*y^2) + 2*kt1*x*y = 0.2578125 + 0.004375 + 0.005 = 0.2671875
    #[test]
    fn the_warp_matches_a_hand_computed_specification_vector() {
        let w = Warp {
            planes: vec![[1.0, 0.1, 0.0, 0.0, 0.01, 0.02]],
            center: [0.5; 2],
        };
        let (sx, sy) = w.source_offset(1, 0.5, 0.25, 1.0);
        assert!((sx - 0.534375).abs() < 1e-12, "{sx}");
        assert!((sy - 0.2671875).abs() < 1e-12, "{sy}");
        // And the scale: the same point at m = 100 px gives 100x the offset.
        let (sx, sy) = w.source_offset(1, 50.0, 25.0, 100.0);
        assert!((sx - 53.4375).abs() < 1e-9 && (sy - 26.71875).abs() < 1e-9);
    }

    #[test]
    fn vignette_gain_is_one_at_centre_and_follows_the_polynomial() {
        let v = Vignette {
            k: [0.5, -0.1, 0.0, 0.0, 0.0],
            center: [0.5; 2],
        };
        assert_eq!(v.gain(0.0), 1.0);
        assert!((v.gain(1.0) - 1.4).abs() < 1e-12);
    }

    #[test]
    fn farthest_corner_is_the_dng_normalisation_radius() {
        assert!((farthest_corner(600.0, 800.0, 300.0, 400.0) - 500.0).abs() < 1e-9);
        // Off-centre: the far corner dominates.
        assert!((farthest_corner(100.0, 100.0, 0.0, 0.0) - 100.0f64.hypot(100.0)).abs() < 1e-9);
        assert!(farthest_corner(0.0, 0.0, 0.0, 0.0) >= 1.0);
    }
}
