//! Hand-rolled p50/p95/max timing stats, following `spikes/glint/src/stats.rs`'s pattern
//! (spikes never depend on each other, so this is copied rather than shared).

use std::time::Duration;

#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct Percentiles {
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub max_ms: f64,
    pub n: usize,
}

/// Computes p50/p95/max over a set of measured runs. `samples` should already exclude the
/// discarded warm-up run, per `docs/benchmarks.md`'s methodology.
pub fn percentiles(samples: &[Duration]) -> Percentiles {
    assert!(
        !samples.is_empty(),
        "percentiles() needs at least one sample"
    );
    let mut ms: Vec<f64> = samples.iter().map(|d| d.as_secs_f64() * 1000.0).collect();
    ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p = |q: f64| -> f64 {
        let idx = ((ms.len() as f64 - 1.0) * q).round() as usize;
        ms[idx]
    };
    Percentiles {
        p50_ms: p(0.50),
        p95_ms: p(0.95),
        max_ms: *ms.last().unwrap(),
        n: ms.len(),
    }
}
