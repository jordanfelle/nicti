//! `ProfileToneCurve`: a monotonic curve through a small set of (x, y) control points in [0, 1],
//! sampled per-channel. Adobe's own spline construction is undisclosed; this uses the
//! Fritsch-Carlson monotonic cubic Hermite spline (a standard, published method that guarantees
//! no ringing/overshoot between control points, which is the property a tone curve actually
//! needs) as a documented stand-in -- see ADR-0038's Candidates section.

#[derive(Debug, Clone)]
pub struct ToneCurve {
    xs: Vec<f64>,
    ys: Vec<f64>,
    /// Precomputed tangents (Fritsch-Carlson), one per control point.
    tangents: Vec<f64>,
}

impl ToneCurve {
    /// `points` must be sorted by `x` and have at least 2 entries; `x`/`y` should be in [0, 1].
    pub fn new(points: &[(f64, f64)]) -> Self {
        assert!(
            points.len() >= 2,
            "a tone curve needs at least 2 control points"
        );
        let xs: Vec<f64> = points.iter().map(|p| p.0).collect();
        let ys: Vec<f64> = points.iter().map(|p| p.1).collect();
        for w in xs.windows(2) {
            assert!(
                w[1] > w[0],
                "control points must be strictly increasing in x"
            );
        }

        let n = xs.len();
        let mut secants = vec![0.0; n - 1];
        for i in 0..n - 1 {
            secants[i] = (ys[i + 1] - ys[i]) / (xs[i + 1] - xs[i]);
        }

        let mut tangents = vec![0.0; n];
        tangents[0] = secants[0];
        tangents[n - 1] = secants[n - 2];
        for i in 1..n - 1 {
            if secants[i - 1] * secants[i] <= 0.0 {
                tangents[i] = 0.0;
            } else {
                tangents[i] = (secants[i - 1] + secants[i]) / 2.0;
            }
        }
        // Fritsch-Carlson limiter: clamp tangents so the interpolant stays monotonic on each
        // interval where the secant itself is monotonic (flat secants force both endpoint
        // tangents to 0, handled above via the sign-change check).
        for i in 0..n - 1 {
            if secants[i] == 0.0 {
                tangents[i] = 0.0;
                tangents[i + 1] = 0.0;
                continue;
            }
            let a = tangents[i] / secants[i];
            let b = tangents[i + 1] / secants[i];
            let dist = (a * a + b * b).sqrt();
            if dist > 3.0 {
                let t = 3.0 / dist;
                tangents[i] = t * a * secants[i];
                tangents[i + 1] = t * b * secants[i];
            }
        }

        ToneCurve { xs, ys, tangents }
    }

    /// Adobe's commonly-reproduced "medium contrast" default curve (used by ACR/LRC's Adobe
    /// Standard-family profiles when a DCP has no `ProfileToneCurve` of its own) -- these exact
    /// control points are widely cited in open-source raw-processing discussions, not sourced
    /// from an Adobe primary document; treat as an approximation, per ADR-0038.
    pub fn acr_default() -> Self {
        ToneCurve::new(&[
            (0.0, 0.0),
            (32.0 / 255.0, 22.0 / 255.0),
            (64.0 / 255.0, 56.0 / 255.0),
            (128.0 / 255.0, 128.0 / 255.0),
            (192.0 / 255.0, 196.0 / 255.0),
            (1.0, 1.0),
        ])
    }

    pub fn identity() -> Self {
        ToneCurve::new(&[(0.0, 0.0), (1.0, 1.0)])
    }

    pub fn eval(&self, x: f64) -> f64 {
        let x = x.clamp(self.xs[0], *self.xs.last().unwrap());
        let i = match self
            .xs
            .binary_search_by(|probe| probe.partial_cmp(&x).unwrap())
        {
            Ok(i) => i.min(self.xs.len() - 2),
            Err(i) => (i - 1).min(self.xs.len() - 2),
        };
        let (x0, x1) = (self.xs[i], self.xs[i + 1]);
        let (y0, y1) = (self.ys[i], self.ys[i + 1]);
        let (m0, m1) = (self.tangents[i], self.tangents[i + 1]);
        let h = x1 - x0;
        let t = (x - x0) / h;
        let t2 = t * t;
        let t3 = t2 * t;
        let h00 = 2.0 * t3 - 3.0 * t2 + 1.0;
        let h10 = t3 - 2.0 * t2 + t;
        let h01 = -2.0 * t3 + 3.0 * t2;
        let h11 = t3 - t2;
        h00 * y0 + h10 * h * m0 + h01 * y1 + h11 * h * m1
    }
}

/// Entries in a [`ToneCurveLut`].
pub const TONE_LUT_LEN: usize = 1024;

/// A [`ToneCurve`] baked to a dense table over linear [0, 1], for the GPU and the CPU reference.
/// Entry `i` is the curve at `(i / (LEN - 1))^2`: indexing in sqrt space gives the shadows, where
/// a tone curve bends hardest, far more resolution than a linear table of the same size.
#[derive(Debug, Clone, PartialEq)]
pub struct ToneCurveLut {
    samples: Vec<f32>,
}

impl ToneCurveLut {
    pub fn from_curve(curve: &ToneCurve) -> Self {
        let samples = (0..TONE_LUT_LEN)
            .map(|i| {
                let u = i as f64 / (TONE_LUT_LEN - 1) as f64;
                curve.eval(u * u).clamp(0.0, 1.0) as f32
            })
            .collect();
        ToneCurveLut { samples }
    }

    /// The Adobe default curve (see [`ToneCurve::acr_default`]), for profiles without their own.
    pub fn acr_default() -> Self {
        Self::from_curve(&ToneCurve::acr_default())
    }

    /// A profile's own `ProfileToneCurve` control points (already validated strictly increasing
    /// by the DCP parser), or the ACR default when it has none.
    pub fn from_points_or_default(points: Option<&[(f64, f64)]>) -> Self {
        match points {
            // Finite, in-range points only: a NaN or infinite value would bake NaN into the table
            // and blank every photo using the profile.
            Some(p)
                if p.len() >= 2
                    && p.iter()
                        .all(|&(x, y)| (0.0..=1.0).contains(&x) && (0.0..=1.0).contains(&y))
                    && p.windows(2).all(|w| w[1].0 > w[0].0) =>
            {
                Self::from_curve(&ToneCurve::new(p))
            }
            _ => Self::acr_default(),
        }
    }

    pub fn samples(&self) -> &[f32] {
        &self.samples
    }

    /// FNV-1a over the sample bits, so a re-upload can be skipped when the curve is unchanged.
    pub fn fingerprint(&self) -> u64 {
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        for v in &self.samples {
            h ^= u64::from(v.to_bits());
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h
    }

    /// Linear interpolation in sqrt space; inputs are clamped to [0, 1].
    pub fn eval(&self, x: f32) -> f32 {
        let u = x.clamp(0.0, 1.0).sqrt() * (TONE_LUT_LEN - 1) as f32;
        let i = (u.floor() as usize).min(TONE_LUT_LEN - 2);
        let t = u - i as f32;
        self.samples[i] * (1.0 - t) + self.samples[i + 1] * t
    }

    /// Hue-preserving RGB tone, after Adobe's reference renderer: the largest and smallest
    /// channels go through the curve and the middle one is interpolated between them, so the
    /// curve changes contrast without shifting hue the way a per-channel curve does.
    pub fn apply_rgb(&self, rgb: [f32; 3]) -> [f32; 3] {
        let c = rgb.map(|v| v.clamp(0.0, 1.0));
        let (mut hi, mut lo) = (0, 0);
        for i in 1..3 {
            if c[i] > c[hi] {
                hi = i;
            }
            if c[i] < c[lo] {
                lo = i;
            }
        }
        if hi == lo {
            let y = self.eval(c[0]);
            return [y; 3];
        }
        let mid = 3 - hi - lo;
        let (yh, yl) = (self.eval(c[hi]), self.eval(c[lo]));
        let t = (c[mid] - c[lo]) / (c[hi] - c[lo]);
        let mut out = [0.0; 3];
        out[hi] = yh;
        out[lo] = yl;
        out[mid] = yl + (yh - yl) * t;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_curve_is_a_no_op() {
        let curve = ToneCurve::identity();
        for x in [0.0, 0.1, 0.5, 0.9, 1.0] {
            assert!((curve.eval(x) - x).abs() < 1e-9, "x={x}");
        }
    }

    #[test]
    fn curve_passes_through_control_points() {
        let curve = ToneCurve::acr_default();
        assert!((curve.eval(0.0)).abs() < 1e-9);
        assert!((curve.eval(1.0) - 1.0).abs() < 1e-9);
        assert!((curve.eval(128.0 / 255.0) - 128.0 / 255.0).abs() < 1e-6);
    }

    #[test]
    fn curve_is_monotonic() {
        let curve = ToneCurve::acr_default();
        let mut prev = curve.eval(0.0);
        let mut x = 0.0;
        while x <= 1.0 {
            let y = curve.eval(x);
            assert!(
                y >= prev - 1e-9,
                "curve not monotonic at x={x}: {y} < {prev}"
            );
            prev = y;
            x += 0.001;
        }
    }

    #[test]
    fn lut_matches_the_curve_and_is_monotone() {
        let curve = ToneCurve::acr_default();
        let lut = ToneCurveLut::from_curve(&curve);
        let mut prev = -1.0;
        for i in 0..=100 {
            let x = i as f32 / 100.0;
            let y = lut.eval(x);
            assert!(y >= prev - 1e-6, "not monotone at {x}");
            prev = y;
            assert!(
                (f64::from(y) - curve.eval(f64::from(x))).abs() < 2e-3,
                "x={x}"
            );
        }
    }

    #[test]
    fn rgb_tone_keeps_channel_order_and_greys_stay_grey() {
        let lut = ToneCurveLut::acr_default();
        let g = lut.apply_rgb([0.3, 0.3, 0.3]);
        assert!(g.iter().all(|v| (v - g[0]).abs() < 1e-6));
        let out = lut.apply_rgb([0.8, 0.2, 0.5]);
        assert!(out[0] > out[2] && out[2] > out[1]);
    }

    #[test]
    fn missing_or_bad_points_fall_back_to_the_acr_default() {
        assert_eq!(
            ToneCurveLut::from_points_or_default(None),
            ToneCurveLut::acr_default()
        );
        let bad = [(0.5, 0.5), (0.5, 0.6)];
        let nan = [(0.0, 0.0), (0.5, f64::NAN), (1.0, 1.0)];
        let inf = [(0.0, 0.0), (f64::INFINITY, 1.0)];
        for pts in [&nan[..], &inf[..]] {
            assert_eq!(
                ToneCurveLut::from_points_or_default(Some(pts)),
                ToneCurveLut::acr_default()
            );
        }
        assert_eq!(
            ToneCurveLut::from_points_or_default(Some(&bad)),
            ToneCurveLut::acr_default()
        );
    }
}
