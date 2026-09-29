//! Display/proof transform: linear ProPhoto working space -> the monitor's encoded RGB,
//! optionally simulating a proof space in between (ADR-0042).
//!
//! Display-only by design — nothing here touches Tapetum's render graph or cache keys, so a
//! monitor change or a proofing toggle costs zero bake work.
//!
//! Two independent stages, both evaluated per pixel by the display shader:
//!
//! 1. **Proof** (optional, built-in spaces only): working -> proof-space linear RGB, clamp to
//!    [0, 1] (relative-colorimetric clipping, which is exactly what a matrix/TRC profile does),
//!    and back to working. Pure 3x3 math, so it is exact and the out-of-gamut flag is exact —
//!    an earlier 3D-LUT version clipped nodes along the gamut boundary and smeared the error up
//!    to ~9/255 into in-gamut colors (adversarial review of #42).
//! 2. **Display**: either an exact matrix + transfer function ([`DisplayKind::Space`] — the
//!    default, the sRGB fallback, and any monitor profile equivalent to a built-in space), or a
//!    [`LUT_SIZE`]³ 3D LUT baked by `moxcms` for a genuinely different monitor profile
//!    ([`DisplayKind::Lut`]), indexed by the working space under a gamma-1.8 shaper
//!    ([`crate::space::PROPHOTO_GAMMA`], matching `moxcms`'s ProPhoto profile).

use crate::icc::{color_profile, working_profile};
use crate::math::mat_vec_mul;
use crate::space::{OutputSpace, PROPHOTO_GAMMA};
use moxcms::{CmsError, Layout, RenderingIntent, TransformOptions};

/// Re-exported so callers can build a [`DisplayProfile::Icc`] without their own `moxcms` edge.
pub use moxcms::ColorProfile;
use std::sync::Arc;

/// Edge length of the baked monitor LUT.
pub const LUT_SIZE: usize = 33;

/// Tolerance on the proof gamut test: a linear channel this far outside [0, 1] is float noise
/// (white is 1.0000001), not an out-of-gamut color.
pub const GAMUT_EPS: f64 = 0.002;

/// Where the pixels end up.
#[derive(Clone)]
pub enum DisplayProfile {
    /// One of the built-in spaces (the fallback when no monitor profile is available: sRGB).
    Space(OutputSpace),
    /// A real display ICC profile (e.g. read from the OS).
    Icc(Arc<ColorProfile>),
}

/// A baked 3D LUT. `rgba` is `size³ * 4` f32, red fastest, then green, then blue (matches a wgpu
/// 3D texture with width=R, height=G, depth=B): display-encoded RGB, alpha unused (1.0).
#[derive(Debug, Clone)]
pub struct Lut3d {
    pub size: usize,
    pub rgba: Vec<f32>,
}

#[derive(Debug, Clone)]
pub enum DisplayKind {
    /// Exact matrix + transfer function for a built-in space.
    Space(OutputSpace),
    /// Baked LUT for a monitor profile that is not a built-in space.
    Lut(Lut3d),
}

#[derive(Debug, Clone)]
pub struct DisplayTransform {
    /// Soft-proof through this space first, if set.
    pub proof: Option<OutputSpace>,
    pub kind: DisplayKind,
}

impl DisplayTransform {
    /// No proofing, exact `space` output.
    pub fn exact(space: OutputSpace) -> Self {
        Self {
            proof: None,
            kind: DisplayKind::Space(space),
        }
    }

    /// Builds the transform for `display`, optionally soft-proofing through `proof`.
    pub fn build(display: &DisplayProfile, proof: Option<OutputSpace>) -> Result<Self, CmsError> {
        let kind = match display {
            DisplayProfile::Space(s) => DisplayKind::Space(*s),
            // A monitor profile that is just one of the built-in spaces (Windows' stock
            // "sRGB IEC61966-2.1" is the usual case) takes the exact path.
            DisplayProfile::Icc(p) => match equivalent_space(p) {
                Some(s) => DisplayKind::Space(s),
                None => DisplayKind::Lut(bake_lut(p)?),
            },
        };
        Ok(Self { proof, kind })
    }

    /// CPU reference for the display shader: linear ProPhoto -> display-encoded RGB plus the
    /// out-of-proof-gamut flag. The GPU parity test and the unit tests measure against this.
    pub fn apply(&self, working_linear: [f32; 3]) -> ([f32; 3], bool) {
        let mut w = working_linear;
        let mut flagged = false;
        if let Some(p) = self.proof {
            let v = mat_vec_mul(&p.from_working(), w.map(f64::from));
            flagged = v
                .iter()
                .any(|c| !(-GAMUT_EPS..=1.0 + GAMUT_EPS).contains(c));
            let back = mat_vec_mul(&p.to_working(), v.map(|c| c.clamp(0.0, 1.0)));
            w = back.map(|c| c as f32);
        }
        let out = match &self.kind {
            DisplayKind::Space(space) => {
                let v = mat_vec_mul(&space.from_working(), w.map(f64::from));
                v.map(|c| space.encode(c as f32))
            }
            DisplayKind::Lut(lut) => lut.sample(w),
        };
        (out, flagged)
    }
}

/// Bakes `working -> monitor` (relative colorimetric). moxcms clamps f32 output to [0, 1].
fn bake_lut(display_profile: &ColorProfile) -> Result<Lut3d, CmsError> {
    let n = LUT_SIZE;
    let mut grid = Vec::with_capacity(n * n * n * 3);
    for b in 0..n {
        for g in 0..n {
            for r in 0..n {
                let s = (n - 1) as f32;
                grid.extend_from_slice(&[r as f32 / s, g as f32 / s, b as f32 / s]);
            }
        }
    }
    let mut out = vec![0.0f32; grid.len()];
    working_profile()
        .create_transform_f32(
            Layout::Rgb,
            display_profile,
            Layout::Rgb,
            TransformOptions {
                rendering_intent: RenderingIntent::RelativeColorimetric,
                ..TransformOptions::default()
            },
        )?
        .transform(&grid, &mut out)?;
    let mut rgba = Vec::with_capacity(n * n * n * 4);
    for px in out.as_chunks::<3>().0 {
        rgba.extend_from_slice(&[
            px[0].clamp(0.0, 1.0),
            px[1].clamp(0.0, 1.0),
            px[2].clamp(0.0, 1.0),
            1.0,
        ]);
    }
    Ok(Lut3d { size: n, rgba })
}

/// The built-in space `profile` is indistinguishable from (max encoded difference < 0.004 over a
/// probe grid of working-space colors), if any.
fn equivalent_space(profile: &ColorProfile) -> Option<OutputSpace> {
    let levels = [0.05f32, 0.3, 0.6, 0.85];
    let mut probe = Vec::new();
    for &r in &levels {
        for &g in &levels {
            for &b in &levels {
                probe.extend_from_slice(&[r, g, b]);
            }
        }
    }
    let opts = TransformOptions {
        rendering_intent: RenderingIntent::RelativeColorimetric,
        ..TransformOptions::default()
    };
    let run = |dst: &ColorProfile| -> Option<Vec<f32>> {
        let mut out = vec![0.0f32; probe.len()];
        working_profile()
            .create_transform_f32(Layout::Rgb, dst, Layout::Rgb, opts)
            .ok()?
            .transform(&probe, &mut out)
            .ok()?;
        Some(out)
    };
    let theirs = run(profile)?;
    OutputSpace::ALL.into_iter().find(|&s| {
        run(&color_profile(s))
            .is_some_and(|ours| ours.iter().zip(&theirs).all(|(a, b)| (a - b).abs() < 0.004))
    })
}

impl Lut3d {
    /// Trilinear sample, matching the shader: shaper (gamma 1/1.8 on the clamped input), then
    /// texel-centered trilinear filtering with clamp-to-edge addressing.
    pub fn sample(&self, working_linear: [f32; 3]) -> [f32; 3] {
        let n = self.size;
        let coord = working_linear.map(|c| {
            f64::from(c).clamp(0.0, 1.0).powf(1.0 / PROPHOTO_GAMMA) as f32 * (n - 1) as f32
        });
        let base = coord.map(|c| (c.floor() as usize).min(n - 2));
        let frac = [
            coord[0] - base[0] as f32,
            coord[1] - base[1] as f32,
            coord[2] - base[2] as f32,
        ];
        let mut acc = [0.0f32; 3];
        for corner in 0..8usize {
            let (dr, dg, db) = (corner & 1, (corner >> 1) & 1, (corner >> 2) & 1);
            let w = (if dr == 1 { frac[0] } else { 1.0 - frac[0] })
                * (if dg == 1 { frac[1] } else { 1.0 - frac[1] })
                * (if db == 1 { frac[2] } else { 1.0 - frac[2] });
            let idx = (((base[2] + db) * n + (base[1] + dg)) * n + (base[0] + dr)) * 4;
            for (a, v) in acc.iter_mut().zip(&self.rgba[idx..idx + 3]) {
                *a += w * v;
            }
        }
        acc
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::math::mat_invert;

    fn linear_working_from_srgb_encoded(rgb: [f32; 3]) -> [f32; 3] {
        // sRGB-encoded -> linear sRGB -> linear ProPhoto, via the inverse of `from_working`.
        let lin = rgb.map(|c| OutputSpace::Srgb.decode(c));
        let inv = mat_invert(&OutputSpace::Srgb.from_working());
        mat_vec_mul(&inv, lin.map(f64::from)).map(|c| c as f32)
    }

    #[test]
    fn a_builtin_display_without_proof_is_exact() {
        let t = DisplayTransform::build(&DisplayProfile::Space(OutputSpace::Srgb), None).unwrap();
        assert!(t.proof.is_none());
        assert!(matches!(t.kind, DisplayKind::Space(OutputSpace::Srgb)));
    }

    #[test]
    fn exact_round_trips_known_colors() {
        let t = DisplayTransform::exact(OutputSpace::Srgb);
        for src in [[0.2f32, 0.5, 0.8], [0.153, 0.687, 0.66], [0.9, 0.3, 0.1]] {
            let (out, flagged) = t.apply(linear_working_from_srgb_encoded(src));
            assert!(!flagged);
            for (o, s) in out.iter().zip(src) {
                assert!((o - s).abs() < 1e-3, "{out:?} vs {src:?}");
            }
        }
    }

    #[test]
    fn a_monitor_profile_equal_to_a_builtin_space_takes_the_exact_path() {
        for space in OutputSpace::ALL {
            let t =
                DisplayTransform::build(&DisplayProfile::Icc(Arc::new(color_profile(space))), None)
                    .unwrap();
            assert!(
                matches!(t.kind, DisplayKind::Space(s) if s == space),
                "{space:?} should snap to the exact path"
            );
        }
    }

    #[test]
    fn a_different_monitor_profile_bakes_a_lut() {
        let t = DisplayTransform::build(
            &DisplayProfile::Icc(Arc::new(ColorProfile::new_bt2020())),
            None,
        )
        .unwrap();
        assert!(matches!(t.kind, DisplayKind::Lut(_)));
    }

    #[test]
    fn lut_tracks_moxcms_for_a_non_builtin_monitor() {
        // The LUT must agree with a direct moxcms evaluation for an in-gamut color of the
        // monitor (BT.2020 here); interpolation error stays within a couple of 8-bit steps.
        let display = ColorProfile::new_bt2020();
        let t =
            DisplayTransform::build(&DisplayProfile::Icc(Arc::new(display.clone())), None).unwrap();
        let src = [0.25f32, 0.4, 0.55]; // ProPhoto-encoded (gamma 1.8), well inside BT.2020
        let mut want = [0.0f32; 3];
        working_profile()
            .create_transform_f32(
                Layout::Rgb,
                &display,
                Layout::Rgb,
                TransformOptions::default(),
            )
            .unwrap()
            .transform(&src, &mut want)
            .unwrap();
        let lin = src.map(|c| f64::from(c).powf(PROPHOTO_GAMMA) as f32);
        let (got, _) = t.apply(lin);
        for (g, w) in got.iter().zip(want) {
            assert!((g - w).abs() < 0.01, "lut {got:?} vs moxcms {want:?}");
        }
    }

    fn proofed(display: OutputSpace, proof: OutputSpace) -> DisplayTransform {
        DisplayTransform::build(&DisplayProfile::Space(display), Some(proof)).unwrap()
    }

    #[test]
    fn proofing_a_space_through_itself_is_exact_and_never_flags_in_gamut_colors() {
        // The review's counter-example: a plain teal well inside sRGB used to come out 12/255
        // off through the clipped-node LUT. Analytic proofing is exact.
        let t = proofed(OutputSpace::Srgb, OutputSpace::Srgb);
        let exact = DisplayTransform::exact(OutputSpace::Srgb);
        let mut worst = 0.0f32;
        for r in 0..=20 {
            for g in 0..=20 {
                for b in 0..=20 {
                    let src = [r, g, b].map(|v| v as f32 / 20.0);
                    let w = linear_working_from_srgb_encoded(src);
                    let (a, flagged) = t.apply(w);
                    let (d, _) = exact.apply(w);
                    assert!(!flagged, "{src:?} is inside sRGB but was flagged");
                    for (x, y) in a.iter().zip(d) {
                        worst = worst.max((x - y).abs());
                    }
                }
            }
        }
        assert!(worst < 1e-3, "worst error {} /255", worst * 255.0);
    }

    #[test]
    fn proof_flags_a_saturated_color_but_not_mid_grey() {
        // Saturated Display P3 green is outside sRGB; mid-grey is inside every space.
        let t = proofed(OutputSpace::DisplayP3, OutputSpace::Srgb);
        let p3_green = mat_invert(&OutputSpace::DisplayP3.from_working());
        let w = mat_vec_mul(&p3_green, [0.0, 1.0, 0.0]).map(|c| c as f32);
        assert!(
            t.apply(w).1,
            "saturated P3 green should be out of sRGB gamut"
        );
        let grey = linear_working_from_srgb_encoded([0.5, 0.5, 0.5]);
        assert!(!t.apply(grey).1, "mid grey must not be flagged");
    }

    #[test]
    fn gamut_flag_is_exact_at_the_boundary() {
        let t = proofed(OutputSpace::Srgb, OutputSpace::Srgb);
        let inside = linear_working_from_srgb_encoded([1.0, 0.5, 0.5]);
        assert!(!t.apply(inside).1);
        let m_inv = mat_invert(&OutputSpace::Srgb.from_working());
        let outside = mat_vec_mul(&m_inv, [1.01, 0.2, 0.2]).map(|c| c as f32);
        assert!(t.apply(outside).1);
    }

    #[test]
    fn proofing_clips_wide_colors_to_the_proof_gamut() {
        // P3 green proofed to sRGB comes out as sRGB's clipped green, not P3's.
        let t = proofed(OutputSpace::Srgb, OutputSpace::Srgb);
        let p3_green = mat_invert(&OutputSpace::DisplayP3.from_working());
        let w = mat_vec_mul(&p3_green, [0.0, 1.0, 0.0]).map(|c| c as f32);
        let (out, flagged) = t.apply(w);
        assert!(flagged);
        assert!(
            out[1] > 0.99 && out[0] < 0.01,
            "expected clipped green, got {out:?}"
        );
    }

    #[test]
    fn proofing_leaves_neutrals_unchanged() {
        let t = proofed(OutputSpace::DisplayP3, OutputSpace::AdobeRgb);
        let plain = DisplayTransform::exact(OutputSpace::DisplayP3);
        let grey = linear_working_from_srgb_encoded([0.5, 0.5, 0.5]);
        let (a, _) = t.apply(grey);
        let (b, _) = plain.apply(grey);
        for (x, y) in a.iter().zip(b) {
            assert!(
                (x - y).abs() < 2e-3,
                "grey should be unaffected: {a:?} vs {b:?}"
            );
        }
    }
}
