//! `ProfileToneCurve`: a monotonic curve through a small set of (x, y) control points in [0, 1],
//! sampled per-channel. Adobe's own spline construction is undisclosed; this uses the
//! Fritsch-Carlson monotonic cubic Hermite spline (a standard, published method that guarantees
//! no ringing/overshoot between control points, which is the property a tone curve actually
//! needs) as a documented stand-in -- see ADR-0021's Candidates section.

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
    /// from an Adobe primary document; treat as an approximation, per ADR-0021.
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
}
