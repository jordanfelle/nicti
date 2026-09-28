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
        .or_else(local_git_sha)
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=NICTI_GIT_SHA={sha}");
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");
    watch_git_head();
}

fn local_git_sha() -> Option<String> {
    std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
}

/// Tells Cargo to rerun this script when the checked-out commit changes locally (CodeRabbit
/// review, PR #283) -- without this, emitting `rerun-if-env-changed` above already opts out of
/// Cargo's default "rerun on any source change" behavior, so a local incremental `cargo build`
/// after committing new work would keep embedding whatever `NICTI_GIT_SHA` this crate happened to
/// build with first, silently going stale for the Edge channel's own identity check.
///
/// Resolves `.git` as either a real repository directory or a linked worktree's `.git` gitlink
/// file (`gitdir: <path>`, what `git worktree add` creates) -- this repo's own workflow uses
/// worktrees for every feature branch (see `CLAUDE.md`'s Development workflow section), so a
/// build from one of those must watch *that* worktree's own HEAD, not assume `.git` is always a
/// directory. Best-effort only: if the git dir can't be resolved at all, this simply emits
/// nothing extra -- Cargo still rebuilds this crate on its own source changes regardless, only
/// the embedded SHA itself would go stale in that (already-`GITHUB_SHA`-absent, already
/// dev-only) case, same as the fallback's own "unknown" case already accepts.
fn watch_git_head() {
    let Some(git_dir) = find_git_dir() else {
        return;
    };
    let head = git_dir.join("HEAD");
    if !head.is_file() {
        return;
    }
    println!("cargo:rerun-if-changed={}", head.display());
    // HEAD is normally a symbolic ref ("ref: refs/heads/main") -- checking out a new commit on
    // the same branch updates the ref file it points to, not HEAD itself, so watch that too. For
    // a linked worktree, that ref lives in the *main* repository's shared refs (git's own
    // `commondir` mechanism), not under this worktree's own per-worktree git dir -- resolve that
    // first, or a same-branch commit here (this repo's own workflow: a worktree per feature
    // branch, see `CLAUDE.md`) would keep watching a ref path that never actually changes.
    if let Ok(contents) = std::fs::read_to_string(&head) {
        if let Some(rel_ref) = contents.strip_prefix("ref:").map(str::trim) {
            let common_dir = resolve_common_dir(&git_dir);
            let ref_path = common_dir.join(rel_ref);
            if ref_path.is_file() {
                println!("cargo:rerun-if-changed={}", ref_path.display());
            } else {
                // The branch's ref can be packed (`packed-refs`) rather than a loose file --
                // common right after a fresh clone or a `git pack-refs` (CodeRabbit review, PR
                // #283). Cargo's own FAQ warns that watching a path that doesn't exist *and never
                // gets created* makes it rerun the build script on every single build, so this
                // must never point at `packed-refs` itself (a commit on a packed branch always
                // writes a fresh *loose* ref, never edits the packed entry in place -- watching
                // the packed file would never see that commit at all). Watch the containing
                // `refs` directory instead: creating that loose ref file is itself a change
                // Cargo's mtime-based watch already detects.
                let refs_dir = common_dir.join("refs");
                if refs_dir.is_dir() {
                    println!("cargo:rerun-if-changed={}", refs_dir.display());
                }
            }
        }
    }
}

/// The actual shared refs directory for `git_dir` -- for a linked worktree, `git_dir` is
/// `<main-repo>/.git/worktrees/<name>` and carries its own `commondir` file (a path back to
/// `<main-repo>/.git`, git's own documented mechanism for locating shared state); for a plain
/// repository (no `commondir` file), `git_dir` already *is* that shared directory.
fn resolve_common_dir(git_dir: &std::path::Path) -> std::path::PathBuf {
    match std::fs::read_to_string(git_dir.join("commondir")) {
        Ok(contents) => git_dir.join(contents.trim()),
        Err(_) => git_dir.to_path_buf(),
    }
}

fn find_git_dir() -> Option<std::path::PathBuf> {
    let manifest_dir = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").ok()?);
    let mut dir = manifest_dir.as_path();
    loop {
        let candidate = dir.join(".git");
        if candidate.is_dir() {
            return Some(candidate);
        }
        if candidate.is_file() {
            let contents = std::fs::read_to_string(&candidate).ok()?;
            let gitdir = std::path::PathBuf::from(contents.strip_prefix("gitdir:")?.trim());
            // With `worktree.useRelativePaths`, the gitlink's own path is relative -- to the
            // worktree root (`candidate`'s own parent), not whatever directory this build script
            // happens to run from (CodeRabbit review, PR #283). An unresolved relative path here
            // would make every `cargo:rerun-if-changed` built from it point at a nonexistent
            // path forever, which -- per Cargo's own FAQ -- reruns this build script on every
            // single build instead of the intended "only when the real commit changes."
            return Some(if gitdir.is_relative() {
                candidate.parent().unwrap_or(dir).join(gitdir)
            } else {
                gitdir
            });
        }
        dir = dir.parent()?;
    }
}
