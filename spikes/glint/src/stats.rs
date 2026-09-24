//! p50/p95/max summary, per `docs/benchmarks.md`'s "1 warm-up discarded, 5 measured runs, report
//! p50/p95/max" rule. Deliberately duplicated (not depended on) from `bench/whisker/src/stats.rs`
//! — spikes don't depend on each other or on `bench/*`, per `CLAUDE.md`'s package-map note.

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Stats {
    pub n: usize,
    pub p50: f64,
    pub p95: f64,
    pub max: f64,
}

/// Nearest-rank percentile over `values`, in the input unit (callers use milliseconds).
/// Returns `None` for an empty input rather than fabricating a zero.
pub fn summarize(values: &[f64]) -> Option<Stats> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).expect("NaN in latency samples"));
    let percentile = |q: f64| -> f64 {
        let idx = (q * (sorted.len() as f64 - 1.0)).round() as usize;
        sorted[idx.min(sorted.len() - 1)]
    };
    Some(Stats {
        n: sorted.len(),
        p50: percentile(0.50),
        p95: percentile(0.95),
        max: *sorted.last().expect("checked non-empty above"),
    })
}

/// Discards the configured number of warm-up samples (from the front) before summarizing,
/// matching `docs/benchmarks.md`'s "1 warm-up run discarded, then N measured runs" rule.
pub fn summarize_after_warmup(values: &[f64], warmup: usize) -> Option<Stats> {
    summarize(values.get(warmup..).unwrap_or(&[]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_none() {
        assert!(summarize(&[]).is_none());
    }

    #[test]
    fn single_value() {
        let s = summarize(&[42.0]).unwrap();
        assert_eq!(s.n, 1);
        assert_eq!(s.p50, 42.0);
        assert_eq!(s.p95, 42.0);
        assert_eq!(s.max, 42.0);
    }

    #[test]
    fn known_distribution() {
        let values: Vec<f64> = (1..=10).map(|n| n as f64).collect();
        let s = summarize(&values).unwrap();
        assert_eq!(s.n, 10);
        assert_eq!(s.p50, 6.0);
        assert_eq!(s.p95, 10.0);
        assert_eq!(s.max, 10.0);
    }

    #[test]
    fn warmup_discarded_from_front() {
        let values = vec![1000.0, 1.0, 2.0, 3.0, 4.0, 5.0];
        let s = summarize_after_warmup(&values, 1).unwrap();
        assert_eq!(s.n, 5);
        assert_eq!(s.max, 5.0);
    }

    #[test]
    fn warmup_larger_than_input_is_none() {
        assert!(summarize_after_warmup(&[1.0, 2.0], 5).is_none());
    }
}
