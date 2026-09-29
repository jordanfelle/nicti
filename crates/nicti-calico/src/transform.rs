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
use crate::math::{mat_invert, mat_mul, mat_vec_mul};
use crate::space::{OutputSpace, PROPHOTO_GAMMA};
use moxcms::{CmsError, Layout, RenderingIntent, TransformOptions};

/// Re-exported so callers can build a [`DisplayProfile::Icc`] without their own `moxcms` edge.
pub use moxcms::{ColorProfile, ToneReprCurve};
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

/// Encode-table length for [`MatrixTrc`]; indexed by `sqrt(linear)` so the steep near-black part
/// of a gamma curve gets dense sampling.
pub const TRC_TABLE_SIZE: usize = 1024;

/// A matrix/TRC monitor profile evaluated analytically: linear working -> monitor linear RGB by
/// one 3x3, clip to [0, 1], then a per-channel encode table. Exact where a 3D LUT is not (a LUT
/// indexed by the wide ProPhoto cube clips nodes along a narrower monitor's gamut boundary and
/// smears that error into in-gamut colors -- measured 14-20/255 for a P3 gamma-2.2 monitor).
#[derive(Debug, Clone)]
pub struct MatrixTrc {
    /// Linear ProPhoto (D50) -> the monitor's linear RGB.
    pub from_working: [[f32; 3]; 3],
    /// `encode[c][i]` = encoded value of channel `c` at linear `L = (i / (N-1))²`.
    pub encode: [Vec<f32>; 3],
}

#[derive(Debug, Clone)]
pub enum DisplayKind {
    /// Exact matrix + transfer function for a built-in space.
    Space(OutputSpace),
    /// Analytic matrix + per-channel curve extracted from a matrix/TRC monitor profile.
    MatrixTrc(MatrixTrc),
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
            DisplayProfile::Icc(p) => match (equivalent_space(p), matrix_trc(p)) {
                (Some(s), _) => DisplayKind::Space(s),
                (None, Some(m)) => DisplayKind::MatrixTrc(m),
                // Not a matrix/TRC profile (LUT-based): the 3D LUT is the only option.
                (None, None) => DisplayKind::Lut(bake_lut(p)?),
            },
        };
        Ok(Self { proof, kind })
    }

    /// Forces the baked-LUT display path for `profile` (what [`Self::build`] falls back to for a
    /// profile that is neither a built-in space nor matrix/TRC). Exposed so that path stays
    /// testable end to end.
    pub fn with_lut(profile: &ColorProfile, proof: Option<OutputSpace>) -> Result<Self, CmsError> {
        Ok(Self {
            proof,
            kind: DisplayKind::Lut(bake_lut(profile)?),
        })
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
            DisplayKind::MatrixTrc(m) => m.apply(w),
            DisplayKind::Lut(lut) => lut.sample(w),
        };
        (out, flagged)
    }
}

/// Decodes a profile TRC (encoded -> linear) per the ICC `curv`/`para` semantics.
fn decode_fn(curve: &ToneReprCurve) -> Option<Box<dyn Fn(f64) -> f64>> {
    match curve {
        ToneReprCurve::Lut(t) if t.is_empty() => Some(Box::new(|x| x)),
        // A single-entry `curv` is a u8Fixed8 gamma.
        ToneReprCurve::Lut(t) if t.len() == 1 => {
            let g = f64::from(t[0]) / 256.0;
            Some(Box::new(move |x| x.powf(g)))
        }
        ToneReprCurve::Lut(t) => {
            let t: Vec<f64> = t.iter().map(|&v| f64::from(v) / 65535.0).collect();
            Some(Box::new(move |x| {
                let pos = x.clamp(0.0, 1.0) * (t.len() - 1) as f64;
                let i = (pos.floor() as usize).min(t.len() - 2);
                t[i] + (t[i + 1] - t[i]) * (pos - i as f64)
            }))
        }
        ToneReprCurve::Parametric(p) => {
            if p.is_empty() {
                return None;
            }
            let c = moxcms::ParametricCurve::new(p)?;
            Some(Box::new(move |x| f64::from(c.eval(x as f32))))
        }
    }
}

/// Extracts a [`MatrixTrc`] from a matrix/TRC profile (all three colorants and TRCs present and
/// monotonic), else `None`.
fn matrix_trc(profile: &ColorProfile) -> Option<MatrixTrc> {
    // ICC says a B2A/A2B LUT wins over matrix/TRC tags (and moxcms follows that), so a profile
    // that carries one is LUT-based no matter what else it has.
    let has_lut = [
        &profile.lut_a_to_b_perceptual,
        &profile.lut_a_to_b_colorimetric,
        &profile.lut_a_to_b_saturation,
        &profile.lut_b_to_a_perceptual,
        &profile.lut_b_to_a_colorimetric,
        &profile.lut_b_to_a_saturation,
    ]
    .iter()
    .any(|l| l.is_some());
    if has_lut
        || profile.color_space != moxcms::DataColorSpace::Rgb
        || profile.pcs != moxcms::DataColorSpace::Xyz
    {
        return None;
    }
    let (r, g, b) = (
        profile.red_colorant,
        profile.green_colorant,
        profile.blue_colorant,
    );
    let decoders = [
        decode_fn(profile.red_trc.as_ref()?)?,
        decode_fn(profile.green_trc.as_ref()?)?,
        decode_fn(profile.blue_trc.as_ref()?)?,
    ];
    // Colorants are XYZ under the D50 PCS: columns of the profile's RGB -> XYZ(D50) matrix.
    let to_xyz = [[r.x, g.x, b.x], [r.y, g.y, b.y], [r.z, g.z, b.z]];
    if to_xyz.iter().flatten().any(|v| !v.is_finite()) {
        return None;
    }
    let det = to_xyz[0][0] * (to_xyz[1][1] * to_xyz[2][2] - to_xyz[1][2] * to_xyz[2][1])
        - to_xyz[0][1] * (to_xyz[1][0] * to_xyz[2][2] - to_xyz[1][2] * to_xyz[2][0])
        + to_xyz[0][2] * (to_xyz[1][0] * to_xyz[2][1] - to_xyz[1][1] * to_xyz[2][0]);
    if det.abs() < 1e-9 {
        return None;
    }
    let from_working = mat_mul(&mat_invert(&to_xyz), &crate::space::working_to_xyz_d50())
        .map(|row| row.map(|v| v as f32));

    let mut encode: [Vec<f32>; 3] = Default::default();
    for (c, decode) in decoders.iter().enumerate() {
        // Monotonic non-decreasing check, then invert by bisection: encode(L) = e with decode(e)=L.
        let mut prev = decode(0.0);
        if !prev.is_finite() {
            return None;
        }
        for i in 1..=64 {
            let d = decode(f64::from(i) / 64.0);
            // Non-finite (e.g. a negative base under a fractional exponent) or decreasing: not a
            // usable TRC.
            if !d.is_finite() || d < prev - 1e-6 {
                return None;
            }
            prev = d;
        }
        let (lo_l, hi_l) = (decode(0.0), decode(1.0));
        encode[c] = (0..TRC_TABLE_SIZE)
            .map(|i| {
                let u = i as f64 / (TRC_TABLE_SIZE - 1) as f64;
                let l = (u * u).max(lo_l).min(hi_l);
                let (mut lo, mut hi) = (0.0f64, 1.0f64);
                for _ in 0..40 {
                    let mid = 0.5 * (lo + hi);
                    if decode(mid) < l {
                        lo = mid;
                    } else {
                        hi = mid;
                    }
                }
                (0.5 * (lo + hi)) as f32
            })
            .collect();
    }
    Some(MatrixTrc {
        from_working,
        encode,
    })
}

impl MatrixTrc {
    /// CPU reference for the shader's mode 2: matrix, clip, sqrt-indexed encode table.
    pub fn apply(&self, working_linear: [f32; 3]) -> [f32; 3] {
        let m = self.from_working.map(|r| r.map(f64::from));
        let v = mat_vec_mul(&m, working_linear.map(f64::from));
        let mut out = [0.0f32; 3];
        for c in 0..3 {
            let u = v[c].clamp(0.0, 1.0).sqrt() * (TRC_TABLE_SIZE - 1) as f64;
            let i = (u.floor() as usize).min(TRC_TABLE_SIZE - 2);
            let t = &self.encode[c];
            out[c] = t[i] + (t[i + 1] - t[i]) * (u - i as f64) as f32;
        }
        out
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
                allow_use_cicp_transfer: false,
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
        // Judge the profile by its own ICC tags, the same model `matrix_trc` uses; a stale CICP
        // tag must not make moxcms substitute an sRGB curve for the tagged TRC.
        allow_use_cicp_transfer: false,
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

    fn p3_gamma22() -> ColorProfile {
        let mut p = ColorProfile::new_display_p3();
        // Drop the stale CICP tag, or moxcms would use its sRGB transfer instead of our curve.
        p.cicp = None;
        let curve = ToneReprCurve::Parametric(vec![2.2]);
        p.red_trc = Some(curve.clone());
        p.green_trc = Some(curve.clone());
        p.blue_trc = Some(curve);
        p
    }

    #[test]
    fn a_matrix_trc_monitor_that_is_not_builtin_uses_the_analytic_path() {
        for profile in [p3_gamma22(), ColorProfile::new_bt2020()] {
            let t = DisplayTransform::build(&DisplayProfile::Icc(Arc::new(profile)), None).unwrap();
            assert!(matches!(t.kind, DisplayKind::MatrixTrc(_)));
        }
    }

    #[test]
    fn matrix_trc_tracks_moxcms_over_the_whole_working_cube() {
        // The review's counter-example: a P3 gamma-2.2 monitor. Compare against a direct moxcms
        // transform on a dense grid of ProPhoto-encoded colors, *including* the many that fall
        // outside the monitor's gamut (both clip), so boundary behaviour is covered.
        for display in [p3_gamma22(), ColorProfile::new_bt2020()] {
            let t = DisplayTransform::build(&DisplayProfile::Icc(Arc::new(display.clone())), None)
                .unwrap();
            let mut src = Vec::new();
            let n = 21;
            for r in 0..n {
                for g in 0..n {
                    for b in 0..n {
                        src.extend([r, g, b].map(|v| v as f32 / (n - 1) as f32));
                    }
                }
            }
            let mut want = vec![0.0f32; src.len()];
            working_profile()
                .create_transform_f32(
                    Layout::Rgb,
                    &display,
                    Layout::Rgb,
                    TransformOptions {
                        rendering_intent: RenderingIntent::RelativeColorimetric,
                        allow_use_cicp_transfer: false,
                        ..TransformOptions::default()
                    },
                )
                .unwrap()
                .transform(&src, &mut want)
                .unwrap();
            let mut worst = 0.0f32;
            for (s, w) in src.as_chunks::<3>().0.iter().zip(want.as_chunks::<3>().0) {
                let lin = s.map(|c| f64::from(c).powf(PROPHOTO_GAMMA) as f32);
                let (got, _) = t.apply(lin);
                for (g, w) in got.iter().zip(w) {
                    worst = worst.max((g - w).abs());
                }
            }
            eprintln!("matrix/TRC worst error: {} /255", worst * 255.0);
            assert!(worst < 2.5 / 255.0, "worst {} /255", worst * 255.0);
        }
    }

    #[test]
    fn a_lut_based_monitor_profile_falls_back_to_the_3d_lut() {
        // Strip the TRCs so it no longer looks like a matrix/TRC profile.
        let mut p = ColorProfile::new_display_p3();
        p.red_trc = None;
        assert!(matrix_trc(&p).is_none());
    }

    fn p3_with_gammas(gammas: [f32; 3]) -> ColorProfile {
        let mut p = ColorProfile::new_display_p3();
        p.cicp = None;
        p.red_trc = Some(ToneReprCurve::Parametric(vec![gammas[0]]));
        p.green_trc = Some(ToneReprCurve::Parametric(vec![gammas[1]]));
        p.blue_trc = Some(ToneReprCurve::Parametric(vec![gammas[2]]));
        p
    }

    #[test]
    fn matrix_trc_applies_each_channels_own_curve() {
        // Different gamma per channel, so a channel mix-up in the tables (or the shader that
        // mirrors this) cannot pass. Compared against exact math, not moxcms (which quantises
        // near black).
        let gammas = [1.8f32, 2.2, 2.6];
        let t =
            DisplayTransform::build(&DisplayProfile::Icc(Arc::new(p3_with_gammas(gammas))), None)
                .unwrap();
        let DisplayKind::MatrixTrc(m) = &t.kind else {
            panic!("expected the analytic path");
        };
        for w in [[0.3f32, 0.4, 0.2], [0.05, 0.05, 0.05], [0.6, 0.1, 0.4]] {
            let (got, _) = t.apply(w);
            let lin = mat_vec_mul(&m.from_working.map(|r| r.map(f64::from)), w.map(f64::from));
            for c in 0..3 {
                let want = lin[c].clamp(0.0, 1.0).powf(1.0 / f64::from(gammas[c])) as f32;
                assert!(
                    (got[c] - want).abs() < 1.5 / 255.0,
                    "channel {c} for {w:?}: got {got:?}, want gamma {} -> {want}",
                    gammas[c]
                );
            }
        }
    }

    #[test]
    fn a_stale_cicp_tag_does_not_override_the_tagged_curve() {
        // P3 with a 2.2 TRC but the built-in P3's sRGB-transfer CICP still attached: the ICC
        // tags are the model, so this must not snap to the sRGB-curve Display P3.
        let mut p = p3_gamma22();
        p.cicp = ColorProfile::new_display_p3().cicp;
        assert!(p.cicp.is_some());
        let t = DisplayTransform::build(&DisplayProfile::Icc(Arc::new(p)), None).unwrap();
        assert!(
            matches!(t.kind, DisplayKind::MatrixTrc(_)),
            "snapped to {:?}",
            match t.kind {
                DisplayKind::Space(s) => format!("{s:?}"),
                _ => "other".into(),
            }
        );
    }

    #[test]
    fn a_malformed_curve_degrades_instead_of_panicking() {
        // A `para` type-1 curve with a negative `a` decodes to NaN at 1.0; an empty parametric
        // vec must not index out of bounds. Neither may take the app down.
        for curve in [
            ToneReprCurve::Parametric(vec![2.2, -1.0, 0.5]),
            ToneReprCurve::Parametric(vec![]),
            ToneReprCurve::Parametric(vec![f32::NAN]),
        ] {
            let mut p = p3_gamma22();
            p.red_trc = Some(curve.clone());
            assert!(matrix_trc(&p).is_none());
            // (`build` itself is not called for the empty vec: a parsed profile can't produce
            // one, and moxcms' own LUT fallback indexes it. `ColorManagement` also wraps `build`
            // in `catch_unwind` as a last-resort net for a crashing profile.)
            if !matches!(&curve, ToneReprCurve::Parametric(v) if v.is_empty()) {
                let _ = DisplayTransform::build(&DisplayProfile::Icc(Arc::new(p)), None);
            }
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
