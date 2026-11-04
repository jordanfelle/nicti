//! `prowl bench-gate` end to end (#538): exit code, output, and what happens to the history.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn report(dir: &Path, name: &str, cpu_p50_ms: f64) -> std::path::PathBuf {
    let path = dir.join(format!("{name}-report.json"));
    fs::write(
        &path,
        format!(
            r#"{{"name":"{name}","unix_time":9,"hardware":{{}},"stats":{{"p50_ms":5.0,"cpu_p50_ms":{cpu_p50_ms}}}}}"#
        ),
    )
    .unwrap();
    path
}

fn seed_history(path: &Path, bench: &str, values: &[f64]) {
    let lines: Vec<String> = values
        .iter()
        .map(|v| format!(r#"{{"bench":"{bench}","cpu_ms":{v},"unix_time":1}}"#))
        .collect();
    fs::write(path, lines.join("\n") + "\n").unwrap();
}

fn gate(history: &Path, report: &Path, extra: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_prowl"))
        .arg("bench-gate")
        .arg("--history")
        .arg(history)
        .arg("--report")
        .arg(report)
        .args(extra)
        .output()
        .unwrap()
}

#[test]
fn a_regression_exits_nonzero_and_never_becomes_the_baseline() {
    let dir = tempfile::tempdir().unwrap();
    let history = dir.path().join("history.jsonl");
    seed_history(&history, "decode", &[100.0, 100.0, 100.0]);
    let before = fs::read_to_string(&history).unwrap();

    let out = gate(
        &history,
        &report(dir.path(), "decode", 130.0),
        &["--append"],
    );
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("REGRESSION decode"), "{stdout}");
    assert!(stdout.contains("+30.0%"), "{stdout}");
    assert_eq!(
        fs::read_to_string(&history).unwrap(),
        before,
        "a regressed run must not be appended even with --append"
    );
}

#[test]
fn a_passing_run_exits_zero_and_is_appended_only_when_asked() {
    let dir = tempfile::tempdir().unwrap();
    let history = dir.path().join("history.jsonl");
    seed_history(&history, "decode", &[100.0, 100.0, 100.0]);
    let r = report(dir.path(), "decode", 110.0);

    let out = gate(&history, &r, &[]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(fs::read_to_string(&history).unwrap().lines().count(), 3);

    let out = gate(&history, &r, &["--append", "--commit", "abc123"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let text = fs::read_to_string(&history).unwrap();
    assert_eq!(text.lines().count(), 4);
    assert!(text.lines().last().unwrap().contains("abc123"), "{text}");
}

#[test]
fn a_custom_threshold_and_a_wall_time_only_report_are_handled() {
    let dir = tempfile::tempdir().unwrap();
    let history = dir.path().join("history.jsonl");
    seed_history(&history, "decode", &[100.0, 100.0, 100.0]);
    let r = report(dir.path(), "decode", 110.0);
    let out = gate(&history, &r, &["--threshold", "0.05"]);
    assert_eq!(out.status.code(), Some(1), "10 % is over a 5 % threshold");

    let wall_only = dir.path().join("wall.json");
    fs::write(&wall_only, r#"{"name":"decode","stats":{"p50_ms":1.0}}"#).unwrap();
    let out = gate(&history, &wall_only, &[]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "a broken gate is not a regression"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("never wall time"), "{err}");
}

#[test]
fn errors_exit_two_and_a_zero_min_history_does_not_panic() {
    let dir = tempfile::tempdir().unwrap();
    let history = dir.path().join("history.jsonl");
    let r = report(dir.path(), "decode", 500.0);

    // --min-history 0 with no history: not gated, exit 0 (used to panic with exit 101).
    let out = gate(&history, &r, &["--min-history", "0", "--window", "0"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");

    // A corrupt history is an error (2), distinct from a regression (1).
    fs::write(&history, "not json\n").unwrap();
    let out = gate(&history, &r, &[]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");

    // Two reports for one benchmark are refused.
    let out = Command::new(env!("CARGO_BIN_EXE_prowl"))
        .arg("bench-gate")
        .arg("--history")
        .arg(dir.path().join("none.jsonl"))
        .arg("--report")
        .arg(&r)
        .arg("--report")
        .arg(&r)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("second report"));
}
