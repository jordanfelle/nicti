//! Bench regression gate (#538, item 2 of #433): compares a benchmark's **CPU time** (not wall
//! time, which moves with machine load) against its recorded history and flags a regression of
//! more than 20 %.
//!
//! History is a JSON-lines file (`history.jsonl`), one [`Entry`] per line, appended in run order.
//! The current run is read from the structured JSON a [`crate::perf::RunReport`] writes
//! (`stats.cpu_p50_ms`), never parsed out of text. A benchmark is judged against the **median of
//! its last [`Policy::window`] entries**, which a single noisy run can't move, and only once it
//! has [`Policy::min_history`] of them.

use std::fs;
use std::path::Path;

use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};

/// One recorded run of one benchmark.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub bench: String,
    /// Median process CPU time per run, milliseconds.
    pub cpu_ms: f64,
    pub unix_time: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

/// How strict the gate is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Policy {
    /// Regression when `current > baseline * (1 + threshold)`. Default 0.20.
    pub threshold: f64,
    /// How many of a benchmark's latest history entries make its baseline. Default 5.
    pub window: usize,
    /// Fewer history entries than this and the benchmark is not gated yet. Default 3.
    pub min_history: usize,
    /// A baseline below this many milliseconds is too small to gate (CPU time ticks are 10 ms on
    /// Linux): skipped rather than judged on noise. Default 20.
    pub noise_floor_ms: f64,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            threshold: 0.20,
            window: 5,
            min_history: 3,
            noise_floor_ms: 20.0,
        }
    }
}

/// The gate's judgement of one benchmark.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Ok {
        bench: String,
        baseline_ms: f64,
        current_ms: f64,
    },
    Regression {
        bench: String,
        baseline_ms: f64,
        current_ms: f64,
    },
    /// Not enough history yet (or the baseline is under the noise floor): recorded, not gated.
    NotGated { bench: String, reason: String },
}

impl Verdict {
    pub fn is_regression(&self) -> bool {
        matches!(self, Verdict::Regression { .. })
    }

    /// One line, e.g. `REGRESSION decode: 130.0 ms vs baseline 100.0 ms (+30.0%)`.
    pub fn line(&self) -> String {
        match self {
            Verdict::Ok {
                bench,
                baseline_ms,
                current_ms,
            } => format!(
                "ok         {bench}: {current_ms:.1} ms vs baseline {baseline_ms:.1} ms ({:+.1}%)",
                pct(*baseline_ms, *current_ms)
            ),
            Verdict::Regression {
                bench,
                baseline_ms,
                current_ms,
            } => format!(
                "REGRESSION {bench}: {current_ms:.1} ms vs baseline {baseline_ms:.1} ms ({:+.1}%)",
                pct(*baseline_ms, *current_ms)
            ),
            Verdict::NotGated { bench, reason } => format!("not gated  {bench}: {reason}"),
        }
    }
}

fn pct(baseline: f64, current: f64) -> f64 {
    (current / baseline - 1.0) * 100.0
}

/// Reads `history.jsonl`. A missing file is an empty history (the first run); a line that does not
/// parse is an error naming it -- a corrupt history must not quietly shrink the baseline.
pub fn read_history(path: &Path) -> anyhow::Result<Vec<Entry>> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    parse_history(&text).with_context(|| format!("in {}", path.display()))
}

fn parse_history(text: &str) -> anyhow::Result<Vec<Entry>> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let entry: Entry = serde_json::from_str(line)
            .with_context(|| format!("history line {} is not a valid entry", i + 1))?;
        if !entry.cpu_ms.is_finite() || entry.cpu_ms < 0.0 {
            bail!(
                "history line {} has an invalid cpu_ms {}",
                i + 1,
                entry.cpu_ms
            );
        }
        out.push(entry);
    }
    Ok(out)
}

/// Appends `entries` to the history file, creating it if needed.
pub fn append_history(path: &Path, entries: &[Entry]) -> anyhow::Result<()> {
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    // Never glue a new line onto an unterminated last line.
    if file.metadata()?.len() > 0 && !fs::read(path)?.ends_with(b"\n") {
        file.write_all(b"\n")?;
    }
    for e in entries {
        writeln!(file, "{}", serde_json::to_string(e)?)?;
    }
    Ok(())
}

/// The fields of a `RunReport` JSON the gate reads; everything else is ignored.
#[derive(Deserialize)]
struct ReportIn {
    name: String,
    #[serde(default)]
    unix_time: u64,
    stats: StatsIn,
}

#[derive(Deserialize)]
struct StatsIn {
    #[serde(default)]
    cpu_p50_ms: Option<f64>,
}

/// The [`Entry`] a run report describes. A report with no CPU time is refused: comparing its wall
/// time instead would make the gate depend on how busy the machine was.
pub fn entry_from_report(json: &str, commit: Option<&str>) -> anyhow::Result<Entry> {
    let r: ReportIn = serde_json::from_str(json).context("not a RunReport JSON")?;
    let Some(cpu_ms) = r.stats.cpu_p50_ms else {
        bail!(
            "report `{}` has no stats.cpu_p50_ms (this platform did not record CPU time); the gate \
             compares CPU time, never wall time",
            r.name
        );
    };
    if !cpu_ms.is_finite() || cpu_ms < 0.0 {
        bail!("report `{}` has an invalid cpu_p50_ms {cpu_ms}", r.name);
    }
    Ok(Entry {
        bench: r.name,
        cpu_ms,
        unix_time: r.unix_time,
        commit: commit.map(str::to_string),
    })
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

/// Judges each of `current` against `history` under `policy`.
pub fn evaluate(history: &[Entry], current: &[Entry], policy: &Policy) -> Vec<Verdict> {
    current
        .iter()
        .map(|cur| {
            let past: Vec<f64> = history
                .iter()
                .filter(|h| h.bench == cur.bench)
                .map(|h| h.cpu_ms)
                .collect();
            if past.len() < policy.min_history {
                return Verdict::NotGated {
                    bench: cur.bench.clone(),
                    reason: format!(
                        "{} of {} history entries needed",
                        past.len(),
                        policy.min_history
                    ),
                };
            }
            let recent = past[past.len().saturating_sub(policy.window)..].to_vec();
            let baseline_ms = median(recent);
            if baseline_ms < policy.noise_floor_ms {
                return Verdict::NotGated {
                    bench: cur.bench.clone(),
                    reason: format!(
                        "baseline {baseline_ms:.1} ms is under the {} ms noise floor",
                        policy.noise_floor_ms
                    ),
                };
            }
            if cur.cpu_ms > baseline_ms * (1.0 + policy.threshold) {
                Verdict::Regression {
                    bench: cur.bench.clone(),
                    baseline_ms,
                    current_ms: cur.cpu_ms,
                }
            } else {
                Verdict::Ok {
                    bench: cur.bench.clone(),
                    baseline_ms,
                    current_ms: cur.cpu_ms,
                }
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(bench: &str, cpu_ms: f64) -> Entry {
        Entry {
            bench: bench.into(),
            cpu_ms,
            unix_time: 1,
            commit: None,
        }
    }

    fn hist(bench: &str, values: &[f64]) -> Vec<Entry> {
        values.iter().map(|v| e(bench, *v)).collect()
    }

    #[test]
    fn more_than_twenty_percent_over_the_median_baseline_is_a_regression() {
        let h = hist("decode", &[100.0, 100.0, 100.0, 100.0, 100.0]);
        let p = Policy::default();
        let v = evaluate(&h, &[e("decode", 121.0)], &p);
        assert!(v[0].is_regression(), "{:?}", v[0]);
        let v = evaluate(&h, &[e("decode", 119.0)], &p);
        assert!(!v[0].is_regression(), "{:?}", v[0]);
        assert!(matches!(v[0], Verdict::Ok { .. }));
        // Exactly +20 % is not over the line.
        let v = evaluate(&h, &[e("decode", 120.0)], &p);
        assert!(!v[0].is_regression());
        // Faster is never a regression.
        assert!(!evaluate(&h, &[e("decode", 10.0)], &p)[0].is_regression());
    }

    #[test]
    fn one_noisy_history_run_does_not_move_the_baseline() {
        // A single slow outlier among five: the median ignores it.
        let h = hist("b", &[100.0, 100.0, 400.0, 100.0, 100.0]);
        let v = evaluate(&h, &[e("b", 115.0)], &Policy::default());
        assert!(matches!(v[0], Verdict::Ok { baseline_ms, .. } if baseline_ms == 100.0));
    }

    #[test]
    fn only_the_latest_window_counts_so_an_old_slow_era_is_forgotten() {
        let mut h = hist("b", &[500.0, 500.0, 500.0, 500.0]);
        h.extend(hist("b", &[100.0; 5]));
        let v = evaluate(&h, &[e("b", 130.0)], &Policy::default());
        assert!(v[0].is_regression(), "baseline should be 100, not 500");
    }

    #[test]
    fn benchmarks_are_judged_only_against_their_own_history() {
        let mut h = hist("a", &[100.0; 3]);
        h.extend(hist("b", &[1000.0; 3]));
        let v = evaluate(&h, &[e("a", 105.0), e("b", 1300.0)], &Policy::default());
        assert!(!v[0].is_regression());
        assert!(v[1].is_regression());
    }

    #[test]
    fn too_little_history_or_a_tiny_baseline_is_not_gated() {
        let p = Policy::default();
        let v = evaluate(&hist("b", &[100.0, 100.0]), &[e("b", 9999.0)], &p);
        assert!(matches!(v[0], Verdict::NotGated { .. }), "{:?}", v[0]);
        let v = evaluate(&[], &[e("new", 5.0)], &p);
        assert!(matches!(v[0], Verdict::NotGated { .. }));
        let v = evaluate(&hist("tiny", &[3.0; 5]), &[e("tiny", 30.0)], &p);
        assert!(
            matches!(&v[0], Verdict::NotGated { reason, .. } if reason.contains("noise floor")),
            "{:?}",
            v[0]
        );
    }

    #[test]
    fn a_report_must_carry_cpu_time_and_wall_time_is_never_substituted() {
        let ok = r#"{"name":"decode","unix_time":7,"hardware":{},"stats":{"p50_ms":9.0,"cpu_p50_ms":42.5}}"#;
        let entry = entry_from_report(ok, Some("abc")).unwrap();
        assert_eq!(
            entry,
            Entry {
                bench: "decode".into(),
                cpu_ms: 42.5,
                unix_time: 7,
                commit: Some("abc".into())
            }
        );
        let no_cpu = r#"{"name":"decode","stats":{"p50_ms":9.0}}"#;
        let err = entry_from_report(no_cpu, None).unwrap_err().to_string();
        assert!(err.contains("never wall time"), "{err}");
        assert!(entry_from_report("not json", None).is_err());
        let bad = r#"{"name":"d","stats":{"cpu_p50_ms":-1.0}}"#;
        assert!(entry_from_report(bad, None).is_err());
    }

    #[test]
    fn history_round_trips_and_a_corrupt_line_is_an_error_not_a_shorter_baseline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        assert!(
            read_history(&path).unwrap().is_empty(),
            "missing = first run"
        );
        append_history(&path, &[e("a", 1.5), e("b", 2.5)]).unwrap();
        append_history(&path, &[e("a", 3.5)]).unwrap();
        let back = read_history(&path).unwrap();
        assert_eq!(back.len(), 3);
        assert_eq!(back[2].cpu_ms, 3.5);

        // An unterminated last line is not glued onto by the next append.
        fs::write(&path, r#"{"bench":"a","cpu_ms":1.0,"unix_time":1}"#).unwrap();
        append_history(&path, &[e("a", 2.0)]).unwrap();
        assert_eq!(read_history(&path).unwrap().len(), 2);

        fs::write(
            &path,
            "{\"bench\":\"a\",\"cpu_ms\":1.0,\"unix_time\":1}\nnot json\n",
        )
        .unwrap();
        let err = format!("{:#}", read_history(&path).unwrap_err());
        assert!(err.contains("line 2"), "{err}");
        fs::write(&path, r#"{"bench":"a","cpu_ms":-4.0,"unix_time":1}"#).unwrap();
        assert!(read_history(&path).is_err(), "negative cpu_ms is corrupt");
    }
}
