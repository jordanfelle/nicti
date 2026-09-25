//! The benchmark protocol from `docs/benchmarks.md`: 1 discarded warm-up run, 5 measured runs,
//! p50/p95/max reported. A run only executes once a [`crate::manifest::VerifyReport`] for the
//! files it touches has passed -- see [`Protocol::run`].

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::manifest::VerifyReport;

#[derive(Debug, Clone, Copy)]
pub struct Protocol {
    pub warmup: usize,
    pub measured: usize,
}

impl Default for Protocol {
    fn default() -> Self {
        // docs/benchmarks.md's Methodology section: "1 warm-up run discarded, then 5 measured
        // runs."
        Protocol {
            warmup: 1,
            measured: 5,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Stats {
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub max_ms: f64,
    pub samples_ms: Vec<f64>,
}

impl Stats {
    fn from_samples(mut samples: Vec<Duration>) -> Self {
        samples.sort();
        let ms: Vec<f64> = samples.iter().map(|d| d.as_secs_f64() * 1000.0).collect();
        Stats {
            p50_ms: percentile(&ms, 0.50),
            p95_ms: percentile(&ms, 0.95),
            max_ms: ms.last().copied().unwrap_or(0.0),
            samples_ms: ms,
        }
    }
}

/// Nearest-rank percentile over an already-sorted slice.
fn percentile(sorted_ms: &[f64], p: f64) -> f64 {
    if sorted_ms.is_empty() {
        return 0.0;
    }
    let rank = ((p * sorted_ms.len() as f64).ceil() as usize).clamp(1, sorted_ms.len());
    sorted_ms[rank - 1]
}

#[derive(Debug, Clone, Serialize)]
pub struct HardwareIdentity {
    pub os: String,
    pub arch: String,
    pub cpu_count: usize,
    pub hostname: String,
}

impl HardwareIdentity {
    /// Best-effort identity from what `std` can see without a native-query dependency. GPU/driver
    /// version aren't captured here -- docs/benchmarks.md expects those recorded by hand
    /// alongside the reference machine's run, same as today's manual bench/ scripts.
    pub fn capture() -> Self {
        let hostname = env::var("COMPUTERNAME")
            .or_else(|_| env::var("HOSTNAME"))
            .unwrap_or_else(|_| "unknown".to_string());
        HardwareIdentity {
            os: env::consts::OS.to_string(),
            arch: env::consts::ARCH.to_string(),
            cpu_count: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(0),
            hostname,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RunReport {
    pub name: String,
    pub hardware: HardwareIdentity,
    pub protocol: Protocol,
    pub stats: Stats,
    pub unix_time: u64,
}

impl serde::Serialize for Protocol {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = s.serialize_struct("Protocol", 2)?;
        st.serialize_field("warmup", &self.warmup)?;
        st.serialize_field("measured", &self.measured)?;
        st.end()
    }
}

impl Protocol {
    /// Runs `f` under this protocol and returns the resulting stats. Doesn't itself check a
    /// [`VerifyReport`] -- use [`Protocol::run_verified`] when the run touches ref-10k files.
    pub fn run<F: FnMut()>(&self, mut f: F) -> Stats {
        for _ in 0..self.warmup {
            f();
        }
        let mut samples = Vec::with_capacity(self.measured);
        for _ in 0..self.measured {
            let start = Instant::now();
            f();
            samples.push(start.elapsed());
        }
        Stats::from_samples(samples)
    }

    /// Refuses to run `f` unless `verify_report` is clean -- the enforcement point for
    /// docs/benchmarks.md's "verify before trusting a run" rule.
    pub fn run_verified<F: FnMut()>(
        &self,
        verify_report: &VerifyReport,
        f: F,
    ) -> anyhow::Result<Stats> {
        if !verify_report.is_clean() {
            anyhow::bail!(
                "refusing to benchmark against an unverified ref-10k copy: {} missing, {} mismatched, {} unknown",
                verify_report.missing.len(),
                verify_report.mismatched.len(),
                verify_report.unknown.len(),
            );
        }
        Ok(self.run(f))
    }
}

/// Writes `report` as JSON under `bench-results/<name>-<unix_time>.json` (gitignored, per
/// docs/benchmarks.md's Result format).
pub fn write_report(
    bench_results_dir: impl AsRef<Path>,
    report: &RunReport,
) -> anyhow::Result<PathBuf> {
    let dir = bench_results_dir.as_ref();
    fs::create_dir_all(dir)?;
    let path = dir.join(format!("{}-{}.json", report.name, report.unix_time));
    let json = serde_json::to_string_pretty(report)?;
    fs::write(&path, json)?;
    Ok(path)
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn run_executes_warmup_plus_measured() {
        let calls = AtomicUsize::new(0);
        let protocol = Protocol {
            warmup: 1,
            measured: 5,
        };
        let stats = protocol.run(|| {
            calls.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(calls.load(Ordering::SeqCst), 6);
        assert_eq!(stats.samples_ms.len(), 5);
    }

    #[test]
    fn percentiles_on_known_samples() {
        // Five samples: 10, 20, 30, 40, 50ms. p50 -> 3rd (30), p95 -> 5th (50), max -> 50.
        let samples: Vec<Duration> = [10, 20, 30, 40, 50]
            .iter()
            .map(|ms| Duration::from_millis(*ms))
            .collect();
        let stats = Stats::from_samples(samples);
        assert!((stats.p50_ms - 30.0).abs() < 0.01);
        assert!((stats.p95_ms - 50.0).abs() < 0.01);
        assert!((stats.max_ms - 50.0).abs() < 0.01);
    }

    #[test]
    fn run_verified_refuses_dirty_report() {
        let protocol = Protocol::default();
        let dirty = VerifyReport {
            ok: 0,
            missing: vec!["x.nef".into()],
            mismatched: vec![],
            unknown: vec![],
        };
        let result = protocol.run_verified(&dirty, || {});
        assert!(result.is_err());
    }

    #[test]
    fn run_verified_runs_on_clean_report() {
        let protocol = Protocol {
            warmup: 0,
            measured: 2,
        };
        let clean = VerifyReport {
            ok: 10,
            missing: vec![],
            mismatched: vec![],
            unknown: vec![],
        };
        let result = protocol.run_verified(&clean, || {});
        assert!(result.is_ok());
    }

    #[test]
    fn write_report_roundtrips_to_disk() {
        let dir = tempfile::tempdir().unwrap();
        let report = RunReport {
            name: "test-run".into(),
            hardware: HardwareIdentity::capture(),
            protocol: Protocol::default(),
            stats: Stats::from_samples(vec![Duration::from_millis(5)]),
            unix_time: 1234567890,
        };
        let path = write_report(dir.path(), &report).unwrap();
        assert!(path.exists());
        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains("test-run"));
    }
}
