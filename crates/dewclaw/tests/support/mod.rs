//! Shared helper for tests that need this fixture crate rebuilt with a specific feature set.
//! Not itself a test target (Cargo only auto-discovers direct `tests/*.rs` files, not files in
//! subdirectories), following the standard "tests/support/mod.rs" pattern.

use std::env::consts::{DLL_PREFIX, DLL_SUFFIX};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// A private target dir per test *binary* (each `tests/*.rs` file compiles to its own
/// process), so builds across different test files never race on the same output path,
/// without needing a cross-process lock. Within one binary, [`build_dewclaw`] adds a
/// subdirectory per feature set (see there).
fn base_target_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before UNIX epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("nicti-dewclaw-{}-{nanos}", std::process::id()))
    })
}

/// The target dir for one feature set. Each set gets its own, so `#[test]` fns in the same binary
/// (which cargo runs concurrently) can each build their variant without overwriting another's
/// dylib: they used to share one dir, and under CI load `missing_export_is_rejected_cleanly` loaded
/// the `bad-abi` build (`AbiMismatch { found: 1001 }`) instead of its own `no-export` one.
/// Two calls with the *same* features share a dir, which cargo's own build lock serializes.
fn target_dir(features: &[&str]) -> PathBuf {
    let name = if features.is_empty() {
        "default".to_string()
    } else {
        features.join("+")
    };
    base_target_dir().join(name)
}

/// Rebuilds this fixture crate with the given cargo features and returns the path to the
/// resulting cdylib. Safe to call from concurrently-running `#[test]` fns: each distinct feature
/// set builds into its own directory (see [`target_dir`]).
pub fn build_dewclaw(features: &[&str]) -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");

    let mut cmd = Command::new(env!("CARGO"));
    cmd.args(["build", "--quiet", "--manifest-path"])
        .arg(&manifest)
        .arg("--target-dir")
        .arg(target_dir(features));
    if !features.is_empty() {
        cmd.arg("--features").arg(features.join(","));
    }
    let status = cmd
        .status()
        .expect("failed to spawn `cargo build` for the dewclaw fixture");
    assert!(
        status.success(),
        "cargo build for dewclaw (features: {features:?}) failed"
    );

    target_dir(features)
        .join("debug")
        .join(format!("{DLL_PREFIX}dewclaw{DLL_SUFFIX}"))
}
