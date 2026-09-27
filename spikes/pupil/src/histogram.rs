//! A luminance histogram + nearest-rank percentile lookup over `0.0..=1.0` samples. No image
//! histogram/percentile utility existed anywhere in this repo before #99.

#[derive(Clone)]
pub struct Histogram {
    /// Ascending-sorted luminance samples, `0.0..=1.0`.
    sorted: Vec<f32>,
}

impl Histogram {
    pub fn from_samples(mut samples: Vec<f32>) -> Self {
        samples.sort_by(|a, b| a.total_cmp(b));
        Self { sorted: samples }
    }

    pub fn len(&self) -> usize {
        self.sorted.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sorted.is_empty()
    }

    /// Nearest-rank percentile, `p` in `0.0..=100.0`. Clamps `p` into range and clamps the rank
    /// into the sample count, so `percentile(0.0)`/`percentile(100.0)` are always the min/max.
    pub fn percentile(&self, p: f64) -> f32 {
        assert!(!self.sorted.is_empty(), "percentile of an empty histogram");
        let p = p.clamp(0.0, 100.0);
        let rank = ((p / 100.0) * (self.sorted.len() - 1) as f64).round() as usize;
        self.sorted[rank.min(self.sorted.len() - 1)]
    }

    pub fn mean(&self) -> f32 {
        assert!(!self.sorted.is_empty(), "mean of an empty histogram");
        self.sorted.iter().sum::<f32>() / self.sorted.len() as f32
    }

    /// Fraction of samples at or below `threshold` -- used to estimate shadow/black clipping
    /// mass. A sample exactly at `threshold` counts here, not in `fraction_above` -- the two are
    /// defined to always sum to `1.0`, never double-counting a boundary value.
    pub fn fraction_below(&self, threshold: f32) -> f64 {
        assert!(
            !self.sorted.is_empty(),
            "fraction_below of an empty histogram"
        );
        let count = self.sorted.partition_point(|&v| v <= threshold);
        count as f64 / self.sorted.len() as f64
    }

    /// Fraction of samples strictly above `threshold` -- used to estimate highlight clipping
    /// mass. `1.0 - fraction_below(threshold)`, so a sample exactly at `threshold` is never
    /// counted by both this and `fraction_below`.
    pub fn fraction_above(&self, threshold: f32) -> f64 {
        1.0 - self.fraction_below(threshold)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_endpoints_are_min_and_max() {
        let h = Histogram::from_samples(vec![0.9, 0.1, 0.5, 0.3, 0.7]);
        assert_eq!(h.percentile(0.0), 0.1);
        assert_eq!(h.percentile(100.0), 0.9);
    }

    #[test]
    fn percentile_median_of_uniform_run() {
        let h = Histogram::from_samples((0..=100).map(|i| i as f32 / 100.0).collect());
        assert!((h.percentile(50.0) - 0.5).abs() < 0.01);
    }

    #[test]
    fn mean_of_uniform_run() {
        let h = Histogram::from_samples((0..=100).map(|i| i as f32 / 100.0).collect());
        assert!((h.mean() - 0.5).abs() < 0.01);
    }

    #[test]
    fn fraction_below_counts_inclusively() {
        let h = Histogram::from_samples(vec![0.0, 0.0, 1.0]);
        assert!((h.fraction_below(0.0) - 2.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn fraction_above_excludes_the_threshold_value_itself() {
        // A sample sits exactly at 1.0 -- it must count toward `fraction_below`, not
        // `fraction_above`, or the two would sum to more than 1.0.
        let h = Histogram::from_samples(vec![0.0, 1.0, 1.0]);
        assert_eq!(h.fraction_above(1.0), 0.0);
        assert!((h.fraction_below(1.0) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn fraction_below_and_above_never_double_count_a_boundary_value() {
        // A sample sits exactly at the threshold (0.5) -- the old implementation counted it in
        // both directions, summing to 2.0 instead of 1.0.
        let h = Histogram::from_samples(vec![0.1, 0.5, 0.9]);
        assert!((h.fraction_below(0.5) + h.fraction_above(0.5) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn fraction_below_and_above_are_consistent_off_threshold() {
        let h = Histogram::from_samples(vec![0.1, 0.4, 0.6, 0.9]);
        // No sample sits exactly at 0.5, so the two fractions must sum to 1.
        assert!((h.fraction_below(0.5) + h.fraction_above(0.5) - 1.0).abs() < 1e-9);
    }

    #[test]
    #[should_panic]
    fn percentile_of_empty_panics() {
        Histogram::from_samples(vec![]).percentile(50.0);
    }
}
