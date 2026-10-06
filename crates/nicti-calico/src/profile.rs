//! Applying a DCP camera profile (#42/#38, ADR-0038 stage order): the per-frame solve that turns
//! a [`DcpProfile`] plus the frame's white balance into (a) one folded camera -> linear ProPhoto
//! matrix and (b) the blended HueSatMap / LookTable and exposure the GPU live suffix applies after
//! it, plus the CPU reference (`apply_cpu`) that the GPU kernel is tested against.
//!
//! Stage order (matches `spikes/calico/src/pipeline.rs`, the reference this was promoted from):
//! camera RGB -> [`ProfileSolution::camera_to_working`] (WB folded in for the ForwardMatrix
//! branch, CCT-interpolated) -> HueSatMap -> baseline exposure -> LookTable -> the rest of the
//! live suffix (user exposure, tone, ...).

use crate::cct::{interpolation_weight, solve_camera_to_xyz, CameraToXyz, Illuminant};
use crate::dcp::{DcpProfile, TableEncoding};
use crate::huesatmap::{hsv_to_rgb, rgb_to_hsv, HueSatMap};
use crate::math::{mat_mul, mat_vec_mul, Mat3, Vec3};
use crate::space::working_from_xyz_d50;

/// sRGB OETF without clamping above 1: ProPhoto-space values routinely exceed 1.0 for saturated
/// colors at this stage, and clamping per channel would crush highlights and skew hue.
pub fn srgb_oetf_unclamped(linear: f64) -> f64 {
    let c = linear.max(0.0);
    if c <= 0.003_130_8 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

/// Inverse of [`srgb_oetf_unclamped`].
pub fn srgb_eotf(encoded: f64) -> f64 {
    let c = encoded.max(0.0);
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn table_encode(encoding: TableEncoding, c: f64) -> f64 {
    match encoding {
        TableEncoding::Linear => c.max(0.0),
        TableEncoding::Srgb => srgb_oetf_unclamped(c),
    }
}

fn table_decode(encoding: TableEncoding, c: f64) -> f64 {
    match encoding {
        TableEncoding::Linear => c,
        TableEncoding::Srgb => srgb_eotf(c),
    }
}

/// Applies one HueSatMap/LookTable to linear ProPhoto `rgb`, per Adobe's reference implementation
/// (`dng_reference.cpp`'s `RefBaselineHueSatMap`): hue and saturation come from the **unencoded**
/// linear RGB; only the **value** coordinate goes through the table's `encoding` (both for the
/// lookup and for the returned scale, then decoded back).
pub fn apply_hue_sat(rgb: Vec3, map: &HueSatMap, encoding: TableEncoding) -> Vec3 {
    let hsv = rgb_to_hsv(rgb);
    let val_encoded = table_encode(encoding, hsv[2]);
    let adj = map.sample(hsv[0], hsv[1], val_encoded);
    hsv_to_rgb([
        hsv[0] + adj[0],
        (hsv[1] * adj[1]).clamp(0.0, 1.0),
        table_decode(encoding, val_encoded * adj[2]),
    ])
}

/// Blends two same-dimension tables by `weight_of_first` (the illuminant-1 interpolation weight),
/// componentwise. Trilinear sampling is linear in the table, so blending the tables is exactly
/// blending their sampled outputs -- which is what lets the GPU bind one texture per weight.
/// `None` when the dimensions differ.
pub fn blend_maps(a: &HueSatMap, b: &HueSatMap, weight_of_first: f64) -> Option<HueSatMap> {
    if a.hue_divisions != b.hue_divisions
        || a.sat_divisions != b.sat_divisions
        || a.val_divisions != b.val_divisions
        || a.data.len() != b.data.len()
    {
        return None;
    }
    let w = weight_of_first as f32;
    Some(HueSatMap {
        hue_divisions: a.hue_divisions,
        sat_divisions: a.sat_divisions,
        val_divisions: a.val_divisions,
        data: a
            .data
            .iter()
            .zip(&b.data)
            .map(|(x, y)| {
                [
                    x[0] * w + y[0] * (1.0 - w),
                    x[1] * w + y[1] * (1.0 - w),
                    x[2] * w + y[2] * (1.0 - w),
                ]
            })
            .collect(),
    })
}

/// A cheap content fingerprint (FNV-1a over the f32 bits) so callers can tell whether a blended
/// table changed without comparing it element by element.
pub fn fingerprint(map: &HueSatMap) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |v: u32| {
        for byte in v.to_le_bytes() {
            h ^= u64::from(byte);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    feed(map.hue_divisions as u32);
    feed(map.sat_divisions as u32);
    feed(map.val_divisions as u32);
    for e in &map.data {
        for c in e {
            feed(c.to_bits());
        }
    }
    h
}

/// Everything the live suffix needs from a profile for one frame at one white balance.
#[derive(Debug, Clone)]
pub struct ProfileSolution {
    /// Camera RGB (black-subtracted, un-white-balanced) -> linear ProPhoto (D50). For the
    /// ForwardMatrix branch the white-balance gains are already folded in.
    pub camera_to_working: [[f32; 3]; 3],
    /// The solved illuminant CCT (K).
    pub cct: f64,
    /// Weight of illuminant 1 at that CCT.
    pub weight: f64,
    /// The illuminant-blended HueSatMap, if the profile has one.
    pub hue_sat_map: Option<HueSatMap>,
    pub hue_sat_encoding: TableEncoding,
    pub look_table: Option<HueSatMap>,
    pub look_encoding: TableEncoding,
    /// An Adobe Raw "Look" `.xmp` profile's LookTable, layered after the DCP's own (#321).
    pub look_profile: Option<HueSatMap>,
    pub look_profile_encoding: TableEncoding,
    /// `2^BaselineExposureOffset`.
    pub baseline_exposure_multiplier: f32,
    /// The profile's own `ProfileToneCurve` (or the ACR default when it has none), baked.
    pub tone_lut: std::sync::Arc<crate::tonecurve::ToneCurveLut>,
    /// `DefaultBlackRender`; parsed and carried, see ADR-0042.
    pub black_render: crate::dcp::BlackRender,
}

impl DcpProfile {
    /// Solves this profile for a frame whose white-balance gains (`[r/g, 1, b/g]`, what
    /// Tapetum's `wb_gains_with_params` returns) are `wb_gains`.
    pub fn solve(&self, wb_gains: [f64; 3]) -> ProfileSolution {
        // AsShotNeutral is proportional to 1/gain (a neutral surface's raw signal is smaller in
        // the channels that need more boost).
        let neutral = wb_gains.map(|g| 1.0 / g.max(1e-6));
        let illum1 = Illuminant {
            cct: self.illuminant1_cct,
            color_matrix: self.color_matrix1,
            forward_matrix: self.forward_matrix1,
        };
        let illum2 = Illuminant {
            cct: self.illuminant2_cct,
            color_matrix: self.color_matrix2,
            forward_matrix: self.forward_matrix2,
        };
        let (cct, camera_to_xyz) = solve_camera_to_xyz(neutral, &illum1, &illum2);
        let weight = interpolation_weight(cct, self.illuminant1_cct, self.illuminant2_cct);

        let xyz_to_working = working_from_xyz_d50();
        let camera_to_working: Mat3 = match camera_to_xyz {
            // ForwardMatrix expects white-balanced input: fold the gains in as a diagonal.
            CameraToXyz::WhiteBalanced(m) => {
                let mut wb = [[0.0; 3]; 3];
                for (i, g) in wb_gains.iter().enumerate() {
                    wb[i][i] = *g;
                }
                mat_mul(&xyz_to_working, &mat_mul(&m, &wb))
            }
            // The ColorMatrix fallback expects raw input; its Bradford step does the WB. Its
            // absolute scale drifts with the solved illuminant (~0.35 EV between A and D65 on a
            // real profile), unlike the ForwardMatrix branch, which maps a neutral to Y =
            // (green channel). Normalize the same way: scale so the camera neutral (green = 1)
            // lands at Y = 1.
            CameraToXyz::Raw(m) => {
                let white = mat_vec_mul(&m, neutral.map(|n| n / neutral[1].max(1e-12)));
                let scale = if white[1].is_finite() && white[1] > 1e-6 {
                    1.0 / white[1]
                } else {
                    1.0
                };
                let mut scaled = m;
                for row in &mut scaled {
                    for v in row.iter_mut() {
                        *v *= scale;
                    }
                }
                mat_mul(&xyz_to_working, &scaled)
            }
        };

        let hue_sat_map = match (&self.hue_sat_map1, &self.hue_sat_map2) {
            (Some(a), Some(b)) => blend_maps(a, b, weight).or_else(|| Some(a.clone())),
            (Some(a), None) => Some(a.clone()),
            (None, Some(b)) => Some(b.clone()),
            (None, None) => None,
        };

        ProfileSolution {
            camera_to_working: camera_to_working.map(|r| r.map(|v| v as f32)),
            cct,
            weight,
            hue_sat_map,
            hue_sat_encoding: self.hue_sat_map_encoding,
            look_table: self.look_table.clone(),
            look_encoding: self.look_table_encoding,
            look_profile: None,
            look_profile_encoding: TableEncoding::Linear,
            baseline_exposure_multiplier: 2f64.powf(self.baseline_exposure_offset) as f32,
            tone_lut: std::sync::Arc::new(crate::tonecurve::ToneCurveLut::from_points_or_default(
                self.tone_curve_points.as_deref(),
            )),
            black_render: self.default_black_render,
        }
    }
}

impl ProfileSolution {
    /// Layers an Adobe Raw "Look" profile's table after the DCP's own LookTable.
    pub fn with_look(mut self, look: &crate::xmp_profile::LookProfile) -> Self {
        self.look_profile = Some(look.look_table.clone());
        self.look_profile_encoding = look.encoding;
        self
    }

    /// CPU reference for the shader's profile stages: camera RGB -> linear ProPhoto -> HueSatMap
    /// -> baseline exposure -> LookTable. The GPU parity test measures against this.
    pub fn apply_cpu(&self, camera_rgb: [f32; 3]) -> [f32; 3] {
        let m = self.camera_to_working.map(|r| r.map(f64::from));
        let mut rgb = mat_vec_mul(&m, camera_rgb.map(f64::from));
        if let Some(map) = &self.hue_sat_map {
            rgb = apply_hue_sat(rgb, map, self.hue_sat_encoding);
        }
        let exposure = f64::from(self.baseline_exposure_multiplier);
        rgb = rgb.map(|c| c * exposure);
        if let Some(look) = &self.look_table {
            rgb = apply_hue_sat(rgb, look, self.look_encoding);
        }
        if let Some(look) = &self.look_profile {
            rgb = apply_hue_sat(rgb, look, self.look_profile_encoding);
        }
        rgb.map(|c| c as f32)
    }

    /// [`Self::apply_cpu`] followed by the profile's hue-preserving tone curve: the full CPU
    /// reference for the shader's profile block.
    pub fn apply_cpu_toned(&self, camera_rgb: [f32; 3]) -> [f32; 3] {
        self.tone_lut.apply_rgb(self.apply_cpu(camera_rgb))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::math::IDENTITY;

    fn hue_only_map() -> HueSatMap {
        // 4 hue divisions, 1 sat, 1 val: bin 1 (90 deg) gets a distinctive +15 deg shift, every
        // other bin is a no-op. One val division, so value can't be a source of difference.
        let mut data = vec![[0.0f32, 1.0, 1.0]; 4];
        data[1] = [15.0, 1.0, 1.0];
        HueSatMap {
            hue_divisions: 4,
            sat_divisions: 1,
            val_divisions: 1,
            data,
        }
    }

    #[test]
    fn encoding_choice_does_not_affect_hue_or_saturation() {
        let map = hue_only_map();
        let rgb: Vec3 = [0.3, 0.6, 0.1];
        let lin = apply_hue_sat(rgb, &map, TableEncoding::Linear);
        let srgb = apply_hue_sat(rgb, &map, TableEncoding::Srgb);
        for (a, b) in lin.iter().zip(srgb) {
            assert!((a - b).abs() < 1e-9, "encoding leaked into hue/sat");
        }
        assert!(
            (lin[0] - rgb[0]).abs() > 1e-6 || (lin[1] - rgb[1]).abs() > 1e-6,
            "the hue shift should have changed the output"
        );
    }

    #[test]
    fn encoding_choice_does_affect_value_scaling() {
        let map = HueSatMap {
            hue_divisions: 1,
            sat_divisions: 1,
            val_divisions: 2,
            data: vec![[0.0, 1.0, 1.0], [0.0, 1.0, 2.0]],
        };
        let rgb: Vec3 = [0.5, 0.5, 0.5];
        let lin = apply_hue_sat(rgb, &map, TableEncoding::Linear);
        let srgb = apply_hue_sat(rgb, &map, TableEncoding::Srgb);
        assert!((lin[0] - srgb[0]).abs() > 1e-6);
    }

    #[test]
    fn blending_two_tables_equals_blending_their_samples() {
        let a = hue_only_map();
        let mut b = hue_only_map();
        b.data[1] = [45.0, 0.5, 1.0];
        let w = 0.3;
        let blended = blend_maps(&a, &b, w).unwrap();
        for hue in [10.0, 90.0, 200.0] {
            let (sa, sb) = (a.sample(hue, 0.5, 0.5), b.sample(hue, 0.5, 0.5));
            let sm = blended.sample(hue, 0.5, 0.5);
            for c in 0..3 {
                assert!((sm[c] - (sa[c] * w + sb[c] * (1.0 - w))).abs() < 1e-5);
            }
        }
        let mut mismatched = hue_only_map();
        mismatched.hue_divisions = 5;
        assert!(blend_maps(&a, &mismatched, 0.5).is_none());
    }

    #[test]
    fn fingerprint_tracks_content() {
        let a = hue_only_map();
        let mut b = hue_only_map();
        assert_eq!(fingerprint(&a), fingerprint(&b));
        b.data[2][0] = 1.0;
        assert_ne!(fingerprint(&a), fingerprint(&b));
    }

    fn synthetic_profile() -> DcpProfile {
        // Identity ColorMatrix at both illuminants, ForwardMatrix = diag(D50) so a white-balanced
        // (1,1,1) maps to XYZ D50 white.
        let d50 = [0.9642, 1.0, 0.8249];
        let fm = [[d50[0], 0.0, 0.0], [0.0, d50[1], 0.0], [0.0, 0.0, d50[2]]];
        DcpProfile {
            name: "synthetic".into(),
            unique_camera_model: "TEST".into(),
            illuminant1_cct: 2856.0,
            illuminant2_cct: 6504.0,
            color_matrix1: IDENTITY,
            color_matrix2: IDENTITY,
            forward_matrix1: Some(fm),
            forward_matrix2: Some(fm),
            hue_sat_map1: None,
            hue_sat_map2: None,
            look_table: None,
            tone_curve_points: None,
            baseline_exposure_offset: 0.0,
            hue_sat_map_encoding: TableEncoding::Linear,
            look_table_encoding: TableEncoding::Linear,
            default_black_render: crate::dcp::BlackRender::Auto,
        }
    }

    #[test]
    fn a_neutral_camera_signal_maps_to_neutral_working_white() {
        // Camera neutral under gains [2, 1, 1.5]: raw signal proportional to 1/gain.
        let gains = [2.0, 1.0, 1.5];
        let sol = synthetic_profile().solve(gains);
        let raw = [1.0 / 2.0, 1.0, 1.0 / 1.5];
        let out = sol.apply_cpu(raw);
        for c in out {
            assert!(
                (c - 1.0).abs() < 2e-3,
                "expected neutral white, got {out:?}"
            );
        }
    }

    #[test]
    fn both_matrix_branches_render_a_neutral_at_the_same_brightness() {
        // A ColorMatrix-only profile (no ForwardMatrix) must not drift in brightness with the
        // solved illuminant the way the raw Bradford-adapted matrix does.
        let gains = [1.7, 1.0, 1.4];
        let raw = [1.0 / 1.7, 1.0, 1.0 / 1.4]; // a neutral patch, green = 1
        let forward = synthetic_profile().solve(gains).apply_cpu(raw);
        let mut cm_only = synthetic_profile();
        cm_only.forward_matrix1 = None;
        cm_only.forward_matrix2 = None;
        let no_forward = cm_only.solve(gains).apply_cpu(raw);
        for c in 0..3 {
            assert!(
                (forward[c] - no_forward[c]).abs() < 0.05,
                "forward {forward:?} vs color-matrix-only {no_forward:?}"
            );
        }
        // And across illuminants: warmer gains must not change a neutral's luminance either.
        for g in [[1.2, 1.0, 2.2], [2.4, 1.0, 1.1]] {
            let n = g.map(|x| (1.0 / x) as f32);
            let out = cm_only.solve(g).apply_cpu(n);
            let luma = 0.2126 * out[0] + 0.7152 * out[1] + 0.0722 * out[2];
            assert!((luma - 1.0).abs() < 0.08, "gains {g:?}: luma {luma}");
        }
    }

    #[test]
    fn a_single_illuminant_forward_matrix_is_used_not_discarded() {
        let mut p = synthetic_profile();
        p.forward_matrix2 = None; // ForwardMatrix1 only
        let gains = [1.5, 1.0, 1.3];
        let sol = p.solve(gains);
        let out = sol.apply_cpu(gains.map(|g| (1.0 / g) as f32));
        for c in out {
            assert!((c - 1.0).abs() < 2e-3, "{out:?}");
        }
    }

    #[test]
    fn baseline_exposure_scales_and_no_tables_is_just_the_matrix() {
        let mut p = synthetic_profile();
        p.baseline_exposure_offset = -1.0;
        let sol = p.solve([1.0, 1.0, 1.0]);
        assert!((sol.baseline_exposure_multiplier - 0.5).abs() < 1e-6);
        let base = synthetic_profile()
            .solve([1.0, 1.0, 1.0])
            .apply_cpu([0.4; 3]);
        let dimmed = sol.apply_cpu([0.4; 3]);
        for (a, b) in base.iter().zip(dimmed) {
            assert!((a * 0.5 - b).abs() < 1e-5);
        }
    }

    #[test]
    fn tables_are_applied_after_the_matrix_and_before_the_look() {
        let mut p = synthetic_profile();
        // A value-scale-2 HueSatMap doubles brightness; a value-scale-0.5 LookTable halves it
        // back. Their order relative to baseline exposure (x2 here) is observable:
        // ((v*2 [hsm]) * 2 [baseline]) * 0.5 [look] = 2v.
        let scale = |s: f32| HueSatMap {
            hue_divisions: 1,
            sat_divisions: 1,
            val_divisions: 1,
            data: vec![[0.0, 1.0, s]],
        };
        p.hue_sat_map1 = Some(scale(2.0));
        p.hue_sat_map2 = Some(scale(2.0));
        p.look_table = Some(scale(0.5));
        p.baseline_exposure_offset = 1.0;
        let base = synthetic_profile().solve([1.0; 3]).apply_cpu([0.2; 3]);
        let out = p.solve([1.0; 3]).apply_cpu([0.2; 3]);
        for (b, o) in base.iter().zip(out) {
            assert!((b * 2.0 - o).abs() < 2e-3, "base {base:?} out {out:?}");
        }
    }
}
