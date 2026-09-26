//! DNG `ProfileHueSatMapData`/`ProfileLookTableData`: a 3D LUT over (hue, saturation, value) that
//! nudges hue/saturation/value, applied in the working-space HSV representation per the DNG spec
//! (section 6.3.7's "look" and "hue/sat map" tables share this exact table format and application
//! rule -- calico's `HueSatMap` type serves both `dcp.rs` tags).
//!
//! Table layout matches the DNG SDK's actual storage order (`dng_hue_sat_map::SetDivisions`:
//! `fHueStep = satDivisions`, `fValStep = hueDivisions * fHueStep`) -- **value outermost, hue in
//! the middle, saturation innermost** (saturation varies fastest), not the hue-major order this
//! module's doc originally (and wrongly) claimed. `hue_divisions * sat_divisions * val_divisions`
//! entries, each `[hue_shift_deg, sat_scale, val_scale]`. The hue axis wraps (entry
//! `hue_divisions` is entry `0` again, no duplicate stored); the sat/val axes clamp at the ends.

#[derive(Debug, Clone)]
pub struct HueSatMap {
    pub hue_divisions: usize,
    pub sat_divisions: usize,
    pub val_divisions: usize,
    /// `[hue_shift_deg, sat_scale, val_scale]` per entry, length `hue*sat*val`.
    pub data: Vec<[f32; 3]>,
}

impl HueSatMap {
    fn index(&self, h: usize, s: usize, v: usize) -> [f32; 3] {
        // DNG SDK order: value outermost, hue middle, saturation innermost.
        let idx = (v * self.hue_divisions + h) * self.sat_divisions + s;
        self.data[idx]
    }

    /// Trilinear sample at (hue_deg in [0,360), sat in [0,1], val in [0,1]). Returns
    /// `[hue_shift_deg, sat_scale, val_scale]`.
    pub fn sample(&self, hue_deg: f64, sat: f64, val: f64) -> [f64; 3] {
        let hue_deg = hue_deg.rem_euclid(360.0);
        let h_step = 360.0 / self.hue_divisions as f64;
        let h_pos = hue_deg / h_step;
        let h0 = h_pos.floor() as usize % self.hue_divisions;
        let h1 = (h0 + 1) % self.hue_divisions;
        let h_frac = h_pos - h_pos.floor();

        let (s0, s1, s_frac) = axis_index(sat, self.sat_divisions);
        let (v0, v1, v_frac) = axis_index(val, self.val_divisions);

        let mut out = [0.0f64; 3];
        for (channel, out_val) in out.iter_mut().enumerate() {
            let c000 = self.index(h0, s0, v0)[channel] as f64;
            let c100 = self.index(h1, s0, v0)[channel] as f64;
            let c010 = self.index(h0, s1, v0)[channel] as f64;
            let c110 = self.index(h1, s1, v0)[channel] as f64;
            let c001 = self.index(h0, s0, v1)[channel] as f64;
            let c101 = self.index(h1, s0, v1)[channel] as f64;
            let c011 = self.index(h0, s1, v1)[channel] as f64;
            let c111 = self.index(h1, s1, v1)[channel] as f64;

            // Hue is the axis that wraps, so its shift channel (channel 0) needs shortest-path
            // interpolation across the 360/0 seam (e.g. between +179 deg and -179 deg entries,
            // naive linear interpolation would swing through 0 instead of the 2 deg short way).
            let (c00, c10, c01, c11) = if channel == 0 {
                (
                    lerp_angle(c000, c100, h_frac),
                    lerp_angle(c010, c110, h_frac),
                    lerp_angle(c001, c101, h_frac),
                    lerp_angle(c011, c111, h_frac),
                )
            } else {
                (
                    lerp(c000, c100, h_frac),
                    lerp(c010, c110, h_frac),
                    lerp(c001, c101, h_frac),
                    lerp(c011, c111, h_frac),
                )
            };
            let c0 = lerp(c00, c01, s_frac);
            let c1 = lerp(c10, c11, s_frac);
            *out_val = lerp(c0, c1, v_frac);
        }
        out
    }
}

impl HueSatMap {
    /// Same trilinear sample as [`HueSatMap::sample`], but *without* shortest-path angle
    /// interpolation on the hue-shift channel -- i.e. exactly what a hardware trilinear texture
    /// sampler does (see `gpu.rs`'s module doc). Exists so `tests/gpu_parity.rs` can compare the
    /// GPU kernel against a CPU function with the *same* (documented) approximation, rather than
    /// against `sample`'s more correct wraparound handling, which the GPU path doesn't do.
    pub fn sample_gpu_style(&self, hue_deg: f64, sat: f64, val: f64) -> [f64; 3] {
        let hue_deg = hue_deg.rem_euclid(360.0);
        let h_step = 360.0 / self.hue_divisions as f64;
        let h_pos = hue_deg / h_step;
        let h0 = h_pos.floor() as usize % self.hue_divisions;
        let h1 = (h0 + 1) % self.hue_divisions;
        let h_frac = h_pos - h_pos.floor();

        let (s0, s1, s_frac) = axis_index(sat, self.sat_divisions);
        let (v0, v1, v_frac) = axis_index(val, self.val_divisions);

        let mut out = [0.0f64; 3];
        for (channel, out_val) in out.iter_mut().enumerate() {
            let c000 = self.index(h0, s0, v0)[channel] as f64;
            let c100 = self.index(h1, s0, v0)[channel] as f64;
            let c010 = self.index(h0, s1, v0)[channel] as f64;
            let c110 = self.index(h1, s1, v0)[channel] as f64;
            let c001 = self.index(h0, s0, v1)[channel] as f64;
            let c101 = self.index(h1, s0, v1)[channel] as f64;
            let c011 = self.index(h0, s1, v1)[channel] as f64;
            let c111 = self.index(h1, s1, v1)[channel] as f64;

            let c00 = lerp(c000, c100, h_frac);
            let c10 = lerp(c010, c110, h_frac);
            let c01 = lerp(c001, c101, h_frac);
            let c11 = lerp(c011, c111, h_frac);
            let c0 = lerp(c00, c01, s_frac);
            let c1 = lerp(c10, c11, s_frac);
            *out_val = lerp(c0, c1, v_frac);
        }
        out
    }
}

fn axis_index(pos: f64, divisions: usize) -> (usize, usize, f64) {
    if divisions <= 1 {
        return (0, 0, 0.0);
    }
    let scaled = pos.clamp(0.0, 1.0) * (divisions - 1) as f64;
    let i0 = scaled.floor() as usize;
    let i0 = i0.min(divisions - 2);
    (i0, i0 + 1, scaled - i0 as f64)
}

fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

/// Linear interpolation between two hue-shift angles (degrees) via the shorter arc.
fn lerp_angle(a: f64, b: f64, t: f64) -> f64 {
    let mut diff = (b - a) % 360.0;
    if diff > 180.0 {
        diff -= 360.0;
    } else if diff < -180.0 {
        diff += 360.0;
    }
    a + diff * t
}

/// RGB (each channel in [0,1]) -> HSV, hue in degrees [0,360).
pub fn rgb_to_hsv(rgb: [f64; 3]) -> [f64; 3] {
    let (r, g, b) = (rgb[0], rgb[1], rgb[2]);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;
    let v = max;
    let s = if max <= 0.0 { 0.0 } else { delta / max };
    let h = if delta.abs() < 1e-12 {
        0.0
    } else if max == r {
        60.0 * (((g - b) / delta).rem_euclid(6.0))
    } else if max == g {
        60.0 * ((b - r) / delta + 2.0)
    } else {
        60.0 * ((r - g) / delta + 4.0)
    };
    [h.rem_euclid(360.0), s, v]
}

pub fn hsv_to_rgb(hsv: [f64; 3]) -> [f64; 3] {
    let (h, s, v) = (hsv[0].rem_euclid(360.0), hsv[1].clamp(0.0, 1.0), hsv[2]);
    let c = v * s;
    let hp = h / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r1, g1, b1) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    [r1 + m, g1 + m, b1 + m]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity_map(hue_div: usize, sat_div: usize, val_div: usize) -> HueSatMap {
        HueSatMap {
            hue_divisions: hue_div,
            sat_divisions: sat_div,
            val_divisions: val_div,
            data: vec![[0.0, 1.0, 1.0]; hue_div * sat_div * val_div],
        }
    }

    #[test]
    fn identity_map_is_a_no_op() {
        let map = identity_map(6, 4, 2);
        for hue in [0.0, 45.0, 180.0, 359.0] {
            for sat in [0.0, 0.3, 1.0] {
                for val in [0.0, 0.6, 1.0] {
                    let out = map.sample(hue, sat, val);
                    assert!((out[0]).abs() < 1e-9);
                    assert!((out[1] - 1.0).abs() < 1e-9);
                    assert!((out[2] - 1.0).abs() < 1e-9);
                }
            }
        }
    }

    #[test]
    fn hue_wraps_across_0_360_seam() {
        let mut map = identity_map(4, 1, 1); // 90 deg steps: 0, 90, 180, 270
        map.data[0] = [10.0, 1.0, 1.0]; // hue bin 0 (at 0 deg): +10
        map.data[3] = [-10.0, 1.0, 1.0]; // hue bin 3 (at 270 deg): -10
                                         // Halfway between bin 3 (270 deg) and bin 0 (360/0 deg, wrapped) is 315 deg.
        let out = map.sample(315.0, 0.0, 1.0);
        // Shortest arc from -10 to +10 through 0, not the long way through +/-180.
        assert!((out[0]).abs() < 1e-6, "expected ~0, got {}", out[0]);
    }

    #[test]
    fn rgb_hsv_round_trip() {
        for rgb in [
            [1.0, 0.0, 0.0],
            [0.2, 0.7, 0.9],
            [0.5, 0.5, 0.5],
            [0.0, 0.0, 0.0],
        ] {
            let hsv = rgb_to_hsv(rgb);
            let back = hsv_to_rgb(hsv);
            for (b, r) in back.iter().zip(rgb.iter()) {
                assert!(
                    (b - r).abs() < 1e-9,
                    "rgb {rgb:?} -> hsv {hsv:?} -> {back:?}"
                );
            }
        }
    }
}
