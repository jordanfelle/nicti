//! Embeds the commit SHA this crate was built from, for the edge channel's update check
//! (`check::edge_update_available`) -- edge builds don't bump `Cargo.toml`'s version per commit,
//! so the installer filename/semver can't tell one edge build from the next; the running
//! binary's own build commit compared against `edge`'s current `target_commitish` can.
//!
//! `GITHUB_SHA` (set automatically by every GitHub Actions run, `release.yml` included) is
//! checked first since it's exactly what CI built from with no extra process spawn; `git
//! rev-parse HEAD` is a fallback for a local `cargo build`. A build with neither available (e.g.
//! from a source tarball with no `.git`) embeds "unknown" rather than failing -- this only ever
//! feeds an opt-in Edge-channel UI nicety, never the stable channel or the minisign trust
//! boundary, so it must never block a build.

fn main() {
    let sha = std::env::var("GITHUB_SHA")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
        })
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=NICTI_GIT_SHA={sha}");
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");
}
