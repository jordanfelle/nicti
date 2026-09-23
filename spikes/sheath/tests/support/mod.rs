//! Shared helper for tests that need the `dewclaw` fixture built as a cdylib. Not itself a
//! test target (Cargo only auto-discovers direct `tests/*.rs` files, not files in
//! subdirectories), following the standard "tests/support/mod.rs" pattern.

use std::env::consts::{DLL_PREFIX, DLL_SUFFIX};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// A private target dir per test *binary* (each `tests/*.rs` file compiles to its own
/// process), so builds across different test files never race on the same output path,
/// without needing a cross-process lock.
fn target_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before UNIX epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "nicti-sheath-dewclaw-{}-{nanos}",
            std::process::id()
        ))
    })
}

/// Builds the `dewclaw` fixture crate with the given cargo features and returns the path
/// to the resulting cdylib. Callers within the same test binary that need distinct
/// feature sets must call this sequentially (not from concurrently-running `#[test]` fns),
/// since a later call's build overwrites the same private target dir's artifact.
pub fn build_dewclaw(features: &[&str]) -> PathBuf {
    let sheath_manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let dewclaw_manifest = sheath_manifest_dir
        .parent()
        .expect("spikes/sheath has a parent dir (spikes/)")
        .join("dewclaw")
        .join("Cargo.toml");

    let mut cmd = Command::new(env!("CARGO"));
    cmd.args(["build", "--quiet", "--manifest-path"])
        .arg(&dewclaw_manifest)
        .arg("--target-dir")
        .arg(target_dir());
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

    target_dir()
        .join("debug")
        .join(format!("{DLL_PREFIX}dewclaw{DLL_SUFFIX}"))
}
