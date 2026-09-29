//! Display/proof transform: linear ProPhoto working space -> the monitor's encoded RGB,
//! optionally simulating a proof space in between (ADR-0042).
//!
//! Display-only by design — nothing here touches Tapetum's render graph or cache keys, so a
//! monitor change or a proofing toggle costs zero bake work.
//!
//! Two shapes:
//! - [`DisplayTransform::Direct`]: the monitor *is* one of the built-in spaces and there is no
//!   proof. The shader uses an exact matrix + transfer function, no LUT (the common case).
//! - [`DisplayTransform::Lut`]: anything else (a real monitor ICC, or soft-proofing). A
//!   [`LUT_SIZE`]³ 3D LUT, indexed by the working space encoded with a gamma-1.8 shaper
//!   ([`crate::space::PROPHOTO_GAMMA`], matching `moxcms`'s ProPhoto profile), whose RGBA texels
//!   hold display-encoded RGB and an out-of-proof-gamut flag in alpha.

use crate::icc::{color_profile, working_profile};
use crate::math::mat_vec_mul;
use crate::space::{OutputSpace, PROPHOTO_GAMMA};
use moxcms::{CmsError, ColorProfile, Layout, TransformOptions};
use std::sync::Arc;

pub use moxcms::RenderingIntent;

/// Edge length of the baked LUT.
pub const LUT_SIZE: usize = 33;

/// Where the pixels end up.
#[derive(Clone)]
pub enum DisplayProfile {
    /// One of the built-in spaces (the fallback when no monitor profile is available: sRGB).
    Space(OutputSpace),
    /// A real display ICC profile (e.g. read from the OS).
    Icc(Arc<ColorProfile>),
}

/// Soft-proofing: simulate how the image would look in `space`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProofSettings {
    pub space: OutputSpace,
    pub intent: RenderingIntent,
}

/// A baked 3D LUT. `rgba` is `size³ * 4` f32, red fastest, then green, then blue (matches a wgpu
/// 3D texture with width=R, height=G, depth=B). Alpha is 1.0 where the working-space color falls
/// outside the proof space's gamut, else 0.0.
#[derive(Debug, Clone)]
pub struct Lut3d {
    pub size: usize,
    pub rgba: Vec<f32>,
}

#[derive(Debug, Clone)]
pub enum DisplayTransform {
    Direct(OutputSpace),
    Lut(Lut3d),
}

impl DisplayTransform {
    /// Builds the transform for `display`, optionally soft-proofing through `proof`.
    pub fn build(display: &DisplayProfile, proof: Option<ProofSettings>) -> Result<Self, CmsError> {
        if let (DisplayProfile::Space(space), None) = (display, proof) {
            return Ok(Self::Direct(*space));
        }
        let display_profile = match display {
            DisplayProfile::Space(s) => color_profile(*s),
            DisplayProfile::Icc(p) => (**p).clone(),
        };
        let working = working_profile();
        let f32_opts = |intent| TransformOptions {
            rendering_intent: intent,
            ..TransformOptions::default()
        };

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
        let mut flags = vec![0.0f32; n * n * n];

        match proof {
            None => {
                working
                    .create_transform_f32(
                        Layout::Rgb,
                        &display_profile,
                        Layout::Rgb,
                        f32_opts(RenderingIntent::RelativeColorimetric),
                    )?
                    .transform(&grid, &mut out)?;
            }
            Some(p) => {
                let proof_profile = color_profile(p.space);
                let mut proofed = vec![0.0f32; grid.len()];
                working
                    .create_transform_f32(
                        Layout::Rgb,
                        &proof_profile,
                        Layout::Rgb,
                        f32_opts(p.intent),
                    )?
                    .transform(&grid, &mut proofed)?;
                // Gamut flag from the proof space's own matrix (exact for the built-in spaces):
                // the color is out of gamut if any linear channel leaves [0, 1].
                let m = p.space.from_working();
                for (i, flag) in flags.iter_mut().enumerate() {
                    let enc = &grid[i * 3..i * 3 + 3];
                    let lin = [
                        f64::from(enc[0]).powf(PROPHOTO_GAMMA),
                        f64::from(enc[1]).powf(PROPHOTO_GAMMA),
                        f64::from(enc[2]).powf(PROPHOTO_GAMMA),
                    ];
                    let v = mat_vec_mul(&m, lin);
                    *flag = f32::from(v.iter().any(|c| !(-0.002..=1.002).contains(c)));
                }
                for v in &mut proofed {
                    *v = v.clamp(0.0, 1.0);
                }
                proof_profile
                    .create_transform_f32(
                        Layout::Rgb,
                        &display_profile,
                        Layout::Rgb,
                        f32_opts(RenderingIntent::RelativeColorimetric),
                    )?
                    .transform(&proofed, &mut out)?;
            }
        }

        let mut rgba = Vec::with_capacity(n * n * n * 4);
        for (i, px) in out.as_chunks::<3>().0.iter().enumerate() {
            rgba.extend_from_slice(&[
                px[0].clamp(0.0, 1.0),
                px[1].clamp(0.0, 1.0),
                px[2].clamp(0.0, 1.0),
                flags[i],
            ]);
        }
        Ok(Self::Lut(Lut3d { size: n, rgba }))
    }

    /// CPU reference for the display shader: linear ProPhoto -> display-encoded RGB plus the
    /// out-of-gamut flag. The GPU parity test and the unit tests measure against this.
    pub fn apply(&self, working_linear: [f32; 3]) -> ([f32; 3], bool) {
        match self {
            Self::Direct(space) => {
                let m = space.from_working();
                let v = mat_vec_mul(&m, working_linear.map(f64::from));
                (v.map(|c| space.encode(c as f32)), false)
            }
            Self::Lut(lut) => lut.sample(working_linear),
        }
    }
}

impl Lut3d {
    /// Trilinear sample, matching the shader: shaper (gamma 1/1.8 on the clamped input), then
    /// texel-centered trilinear filtering with clamp-to-edge addressing.
    pub fn sample(&self, working_linear: [f32; 3]) -> ([f32; 3], bool) {
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
        let mut acc = [0.0f32; 4];
        for corner in 0..8usize {
            let (dr, dg, db) = (corner & 1, (corner >> 1) & 1, (corner >> 2) & 1);
            let w = (if dr == 1 { frac[0] } else { 1.0 - frac[0] })
                * (if dg == 1 { frac[1] } else { 1.0 - frac[1] })
                * (if db == 1 { frac[2] } else { 1.0 - frac[2] });
            let idx = (((base[2] + db) * n + (base[1] + dg)) * n + (base[0] + dr)) * 4;
            for (a, v) in acc.iter_mut().zip(&self.rgba[idx..idx + 4]) {
                *a += w * v;
            }
        }
        ([acc[0], acc[1], acc[2]], acc[3] > 0.5)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linear_working_from_srgb_encoded(rgb: [f32; 3]) -> [f32; 3] {
        // sRGB-encoded -> linear sRGB -> linear ProPhoto, via the inverse of `from_working`.
        let lin = rgb.map(|c| OutputSpace::Srgb.decode(c));
        let inv = crate::math::mat_invert(&OutputSpace::Srgb.from_working());
        mat_vec_mul(&inv, lin.map(f64::from)).map(|c| c as f32)
    }

    #[test]
    fn direct_when_display_is_a_builtin_space_and_no_proof() {
        let t = DisplayTransform::build(&DisplayProfile::Space(OutputSpace::Srgb), None).unwrap();
        assert!(matches!(t, DisplayTransform::Direct(OutputSpace::Srgb)));
    }

    #[test]
    fn direct_round_trips_a_known_color() {
        let t = DisplayTransform::Direct(OutputSpace::Srgb);
        let src = [0.2f32, 0.5, 0.8];
        let (out, flagged) = t.apply(linear_working_from_srgb_encoded(src));
        assert!(!flagged);
        for (o, s) in out.iter().zip(src) {
            assert!((o - s).abs() < 1e-3, "{out:?} vs {src:?}");
        }
    }

    #[test]
    fn lut_for_srgb_display_matches_the_direct_path() {
        // Building the sRGB display through the LUT machinery (forced via an Icc profile) must
        // agree with the exact matrix path to within ~4 8-bit steps. The worst case is a
        // saturated color near the sRGB gamut edge (0.9, 0.3, 0.1), where a node channel clips
        // at 0 and trilinear interpolation across that kink costs about 1%.
        let lut = DisplayTransform::build(
            &DisplayProfile::Icc(Arc::new(color_profile(OutputSpace::Srgb))),
            None,
        )
        .unwrap();
        assert!(matches!(lut, DisplayTransform::Lut(_)));
        let direct = DisplayTransform::Direct(OutputSpace::Srgb);
        for src in [
            [0.2f32, 0.5, 0.8],
            [0.9, 0.3, 0.1],
            [0.5, 0.5, 0.5],
            [0.05, 0.05, 0.05],
        ] {
            let w = linear_working_from_srgb_encoded(src);
            let (a, _) = lut.apply(w);
            let (b, _) = direct.apply(w);
            for (x, y) in a.iter().zip(b) {
                assert!(
                    (x - y).abs() < 0.015,
                    "lut {a:?} vs direct {b:?} for {src:?}"
                );
            }
        }
    }

    fn proof_to(space: OutputSpace) -> DisplayTransform {
        DisplayTransform::build(
            &DisplayProfile::Space(OutputSpace::DisplayP3),
            Some(ProofSettings {
                space,
                intent: RenderingIntent::RelativeColorimetric,
            }),
        )
        .unwrap()
    }

    #[test]
    fn proof_flags_a_saturated_color_but_not_mid_grey() {
        // Saturated Display P3 green is outside sRGB; mid-grey is inside every space.
        let t = proof_to(OutputSpace::Srgb);
        let p3_green = crate::math::mat_invert(&OutputSpace::DisplayP3.from_working());
        let w = mat_vec_mul(&p3_green, [0.0, 1.0, 0.0]).map(|c| c as f32);
        assert!(
            t.apply(w).1,
            "saturated P3 green should be out of sRGB gamut"
        );
        let grey = linear_working_from_srgb_encoded([0.5, 0.5, 0.5]);
        assert!(!t.apply(grey).1, "mid grey must not be flagged");
    }

    #[test]
    fn proofing_to_a_smaller_gamut_changes_wide_colors_only() {
        let t = proof_to(OutputSpace::Srgb);
        let grey = linear_working_from_srgb_encoded([0.5, 0.5, 0.5]);
        let plain =
            DisplayTransform::build(&DisplayProfile::Space(OutputSpace::DisplayP3), None).unwrap();
        let (a, _) = t.apply(grey);
        let (b, _) = plain.apply(grey);
        for (x, y) in a.iter().zip(b) {
            assert!(
                (x - y).abs() < 0.01,
                "grey should be unaffected: {a:?} vs {b:?}"
            );
        }
    }
}
