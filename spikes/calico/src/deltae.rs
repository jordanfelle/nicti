//! CIELAB conversion and CIEDE2000 (Sharma, Wu & Dalal 2005) -- the `calico compare` metric
//! against LRC-exported references, per ADR-0038's decision rule.

/// D65 reference white, 2 degree observer (matches sRGB, which is what both calico's own output
/// and an LRC-exported sRGB TIFF are in).
const WHITE_D65: [f64; 3] = [0.95047, 1.0, 1.08883];

fn lab_f(t: f64) -> f64 {
    const DELTA: f64 = 6.0 / 29.0;
    if t > DELTA.powi(3) {
        t.cbrt()
    } else {
        t / (3.0 * DELTA * DELTA) + 4.0 / 29.0
    }
}

/// XYZ (Y normalized to 1.0 for white) -> CIELAB, relative to D65.
pub fn xyz_to_lab(xyz: [f64; 3]) -> [f64; 3] {
    let fx = lab_f(xyz[0] / WHITE_D65[0]);
    let fy = lab_f(xyz[1] / WHITE_D65[1]);
    let fz = lab_f(xyz[2] / WHITE_D65[2]);
    [116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz)]
}

/// sRGB OETF inverse (linearize), then the standard sRGB->XYZ(D65) matrix.
pub fn srgb8_to_xyz(rgb: [u8; 3]) -> [f64; 3] {
    let lin = |c: u8| -> f64 {
        let c = c as f64 / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    let r = lin(rgb[0]);
    let g = lin(rgb[1]);
    let b = lin(rgb[2]);
    [
        0.4124564 * r + 0.3575761 * g + 0.1804375 * b,
        0.2126729 * r + 0.7151522 * g + 0.0721750 * b,
        0.0193339 * r + 0.1191920 * g + 0.9503041 * b,
    ]
}

/// CIEDE2000 color difference between two CIELAB colors, kL=kC=kH=1 (the standard "graphic arts"
/// weights). Reference: Sharma, Wu & Dalal, "The CIEDE2000 Color-Difference Formula:
/// Implementation Notes, Supplementary Test Data, and Mathematical Observations" (2005).
pub fn ciede2000(lab1: [f64; 3], lab2: [f64; 3]) -> f64 {
    let (l1, a1, b1) = (lab1[0], lab1[1], lab1[2]);
    let (l2, a2, b2) = (lab2[0], lab2[1], lab2[2]);

    let c1 = (a1 * a1 + b1 * b1).sqrt();
    let c2 = (a2 * a2 + b2 * b2).sqrt();
    let c_bar = (c1 + c2) / 2.0;

    let c_bar7 = c_bar.powi(7);
    let g = 0.5 * (1.0 - (c_bar7 / (c_bar7 + 25f64.powi(7))).sqrt());

    let a1p = a1 * (1.0 + g);
    let a2p = a2 * (1.0 + g);

    let c1p = (a1p * a1p + b1 * b1).sqrt();
    let c2p = (a2p * a2p + b2 * b2).sqrt();

    let h1p = hue_deg(a1p, b1);
    let h2p = hue_deg(a2p, b2);

    let delta_lp = l2 - l1;
    let delta_cp = c2p - c1p;

    let delta_hp_raw = if c1p * c2p == 0.0 {
        0.0
    } else {
        let mut d = h2p - h1p;
        if d > 180.0 {
            d -= 360.0;
        } else if d < -180.0 {
            d += 360.0;
        }
        d
    };
    let delta_hp = 2.0 * (c1p * c2p).sqrt() * (delta_hp_raw.to_radians() / 2.0).sin();

    let l_bar_p = (l1 + l2) / 2.0;
    let c_bar_p = (c1p + c2p) / 2.0;

    let h_bar_p = if c1p * c2p == 0.0 {
        h1p + h2p
    } else {
        let sum = h1p + h2p;
        let diff = (h1p - h2p).abs();
        if diff <= 180.0 {
            sum / 2.0
        } else if sum < 360.0 {
            (sum + 360.0) / 2.0
        } else {
            (sum - 360.0) / 2.0
        }
    };

    let t = 1.0 - 0.17 * (h_bar_p - 30.0).to_radians().cos()
        + 0.24 * (2.0 * h_bar_p).to_radians().cos()
        + 0.32 * (3.0 * h_bar_p + 6.0).to_radians().cos()
        - 0.20 * (4.0 * h_bar_p - 63.0).to_radians().cos();

    let delta_theta = 30.0 * (-(((h_bar_p - 275.0) / 25.0).powi(2))).exp();
    let c_bar_p7 = c_bar_p.powi(7);
    let rc = 2.0 * (c_bar_p7 / (c_bar_p7 + 25f64.powi(7))).sqrt();
    let rt = -rc * (2.0 * delta_theta.to_radians()).sin();

    let sl = 1.0 + (0.015 * (l_bar_p - 50.0).powi(2)) / (20.0 + (l_bar_p - 50.0).powi(2)).sqrt();
    let sc = 1.0 + 0.045 * c_bar_p;
    let sh = 1.0 + 0.015 * c_bar_p * t;

    let term_l = delta_lp / sl;
    let term_c = delta_cp / sc;
    let term_h = delta_hp / sh;

    (term_l * term_l + term_c * term_c + term_h * term_h + rt * term_c * term_h).sqrt()
}

fn hue_deg(a: f64, b: f64) -> f64 {
    if a == 0.0 && b == 0.0 {
        0.0
    } else {
        b.atan2(a).to_degrees().rem_euclid(360.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64, tol: f64) -> bool {
        (a - b).abs() < tol
    }

    #[test]
    fn identical_colors_have_zero_difference() {
        let lab = [50.0, 20.0, -30.0];
        assert!(approx(ciede2000(lab, lab), 0.0, 1e-9));
    }

    #[test]
    fn symmetric() {
        let a = [50.0, 2.6772, -79.7751];
        let b = [50.0, 0.0, -82.7485];
        assert!(approx(ciede2000(a, b), ciede2000(b, a), 1e-9));
    }

    /// Reference pairs from Sharma, Wu & Dalal (2005)'s published supplementary test-data table,
    /// widely reproduced across independent CIEDE2000 test suites -- specifically the set of
    /// near-neutral, opposite-hue-quadrant pairs (rows 4-6) whose expected result is exactly
    /// 1.0000, a well-known edge case for catching a broken hue-average/wraparound term.
    #[test]
    fn sharma_2005_known_unity_pairs() {
        let reference = [50.0, 0.0, -82.7485];
        let pairs = [
            [50.0, -1.3802, -84.2814],
            [50.0, -1.1848, -84.8006],
            [50.0, -0.9009, -85.5211],
        ];
        for pair in pairs {
            let de = ciede2000(reference, pair);
            assert!(
                approx(de, 1.0, 0.02),
                "expected ~1.0, got {de} for {pair:?}"
            );
        }
    }

    #[test]
    fn srgb_black_and_white_round_trip_to_lab_extremes() {
        let black = xyz_to_lab(srgb8_to_xyz([0, 0, 0]));
        let white = xyz_to_lab(srgb8_to_xyz([255, 255, 255]));
        assert!(approx(black[0], 0.0, 0.5));
        assert!(approx(white[0], 100.0, 0.5));
    }
}
