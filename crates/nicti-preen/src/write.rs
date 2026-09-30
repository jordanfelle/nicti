//! Collision-safe file output (#57).
//!
//! `spikes/scent`'s `atomic_write` used one shared temp name per target, which two parallel
//! writers to the same name would collide on, and its `rename` silently overwrites. This writer
//! fixes both:
//!
//! - **Claiming a name** (`UniqueSuffix`/`Skip`) uses `OpenOptions::create_new`, which is atomic
//!   (works across threads *and* processes, and on FAT32/exFAT where hard links don't). The claim
//!   leaves a 0-byte placeholder.
//! - **The data** goes to a unique temp file (`.{name}.{pid}-{n}.nicti-tmp`, `n` from a
//!   process-wide counter) in the same directory, is `sync_all`'d, then renamed over the
//!   placeholder (or over the existing file for `Overwrite`).
//! - On any error the temp file and any placeholder *we* created are removed.
//!
//! A crash between claiming and renaming can leave a 0-byte placeholder or a `.nicti-tmp` file;
//! that's documented rather than swept, because destinations aren't tracked anywhere.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use crate::spec::CollisionPolicy;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Suffix bumps tried before giving up on a contended name.
const MAX_CLAIM_TRIES: u32 = 10_000;
/// Backoffs between rename retries (Windows sharing / antivirus holds).
const RENAME_BACKOFF_MS: [u64; 3] = [50, 100, 200];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOutcome {
    Written(PathBuf),
    /// `Skip` policy and the file exists.
    Skipped(PathBuf),
}

#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    #[error("creating folder {path}: {source}")]
    CreateDir { path: PathBuf, source: io::Error },
    #[error("writing {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("no free file name near {0} after {MAX_CLAIM_TRIES} tries")]
    Exhausted(PathBuf),
}

/// `name.ext` -> `name-{n}.ext`.
fn with_suffix(path: &Path, n: u32) -> PathBuf {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = match path.extension() {
        Some(ext) => format!("{stem}-{n}.{}", ext.to_string_lossy()),
        None => format!("{stem}-{n}"),
    };
    path.with_file_name(name)
}

fn temp_path_for(target: &Path) -> PathBuf {
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = target
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    target.with_file_name(format!(".{name}.{}-{n}.nicti-tmp", std::process::id()))
}

/// Removes a file on drop unless disarmed.
struct Cleanup(Option<PathBuf>);
impl Cleanup {
    fn disarm(&mut self) {
        self.0 = None;
    }
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Some(p) = self.0.take() {
            let _ = fs::remove_file(p);
        }
    }
}

fn rename_with_retry(from: &Path, to: &Path) -> io::Result<()> {
    let mut last = None;
    for delay in std::iter::once(0).chain(RENAME_BACKOFF_MS) {
        if delay > 0 {
            thread::sleep(Duration::from_millis(delay));
        }
        match fs::rename(from, to) {
            Ok(()) => return Ok(()),
            // Sharing violations / access denied are transient on Windows (AV, a viewer holding
            // the file); anything else won't get better by waiting.
            Err(e) if matches!(e.kind(), io::ErrorKind::PermissionDenied) => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last.expect("loop ran at least once"))
}

fn write_temp(target: &Path, bytes: &[u8]) -> io::Result<(PathBuf, Cleanup)> {
    let temp = temp_path_for(target);
    let mut guard = Cleanup(Some(temp.clone()));
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    drop(f);
    // Keep the guard armed: the caller disarms it after the rename succeeds.
    let _ = &mut guard;
    Ok((temp, guard))
}

/// Writes `bytes` to `path` under `policy`. May write to a suffixed sibling name instead of
/// `path` itself under `UniqueSuffix` (the returned path says which).
pub fn write_output(
    path: &Path,
    bytes: &[u8],
    policy: CollisionPolicy,
) -> Result<WriteOutcome, WriteError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|source| WriteError::CreateDir {
                path: parent.to_path_buf(),
                source,
            })?;
        }
    }
    let io_err = |p: &Path| {
        let p = p.to_path_buf();
        move |source| WriteError::Io { path: p, source }
    };

    match policy {
        CollisionPolicy::Overwrite => {
            let (temp, mut guard) = write_temp(path, bytes).map_err(io_err(path))?;
            rename_with_retry(&temp, path).map_err(io_err(path))?;
            guard.disarm();
            Ok(WriteOutcome::Written(path.to_path_buf()))
        }
        CollisionPolicy::UniqueSuffix | CollisionPolicy::Skip => {
            let mut n = 1u32;
            loop {
                if n > MAX_CLAIM_TRIES {
                    return Err(WriteError::Exhausted(path.to_path_buf()));
                }
                let candidate = if n == 1 {
                    path.to_path_buf()
                } else {
                    with_suffix(path, n)
                };
                match OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&candidate)
                {
                    Ok(placeholder) => {
                        drop(placeholder);
                        // We own the placeholder now; remove it if anything below fails.
                        let mut placeholder_guard = Cleanup(Some(candidate.clone()));
                        let (temp, mut temp_guard) =
                            write_temp(&candidate, bytes).map_err(io_err(&candidate))?;
                        rename_with_retry(&temp, &candidate).map_err(io_err(&candidate))?;
                        temp_guard.disarm();
                        placeholder_guard.disarm();
                        return Ok(WriteOutcome::Written(candidate));
                    }
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                        if policy == CollisionPolicy::Skip {
                            return Ok(WriteOutcome::Skipped(candidate));
                        }
                        n += 1;
                    }
                    Err(e) => {
                        return Err(WriteError::Io {
                            path: candidate,
                            source: e,
                        })
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn files(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn writes_a_new_file_creating_parent_folders_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b/x.jpg");
        let out = write_output(&path, b"hello", CollisionPolicy::UniqueSuffix).unwrap();
        assert_eq!(out, WriteOutcome::Written(path.clone()));
        assert_eq!(fs::read(&path).unwrap(), b"hello");
        assert_eq!(files(path.parent().unwrap()), ["x.jpg"]);
    }

    #[test]
    fn unique_suffix_never_touches_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.jpg");
        fs::write(&path, b"original").unwrap();
        let out = write_output(&path, b"new", CollisionPolicy::UniqueSuffix).unwrap();
        assert_eq!(out, WriteOutcome::Written(dir.path().join("x-2.jpg")));
        assert_eq!(fs::read(&path).unwrap(), b"original");
        assert_eq!(fs::read(dir.path().join("x-2.jpg")).unwrap(), b"new");
    }

    #[test]
    fn skip_leaves_an_existing_file_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.jpg");
        fs::write(&path, b"original").unwrap();
        let out = write_output(&path, b"new", CollisionPolicy::Skip).unwrap();
        assert_eq!(out, WriteOutcome::Skipped(path.clone()));
        assert_eq!(fs::read(&path).unwrap(), b"original");
        assert_eq!(files(dir.path()), ["x.jpg"]);
    }

    #[test]
    fn overwrite_replaces_the_file_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.jpg");
        fs::write(&path, b"original").unwrap();
        write_output(&path, b"new", CollisionPolicy::Overwrite).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert_eq!(files(dir.path()), ["x.jpg"]);
    }

    #[test]
    fn sixteen_threads_writing_the_same_name_get_sixteen_distinct_files_and_no_temps() {
        let dir = Arc::new(tempfile::tempdir().unwrap());
        let handles: Vec<_> = (0..16u8)
            .map(|i| {
                let dir = Arc::clone(&dir);
                thread::spawn(move || {
                    let path = dir.path().join("same.jpg");
                    let payload = vec![i; 64 * 1024];
                    let out = write_output(&path, &payload, CollisionPolicy::UniqueSuffix).unwrap();
                    (i, out)
                })
            })
            .collect();
        let mut seen = std::collections::HashSet::new();
        for h in handles {
            let (i, out) = h.join().unwrap();
            let WriteOutcome::Written(p) = out else {
                panic!("skipped")
            };
            assert!(seen.insert(p.clone()), "two writers got {p:?}");
            // Each file holds exactly its own writer's bytes (no interleaving).
            assert!(fs::read(&p).unwrap().iter().all(|&b| b == i));
        }
        assert_eq!(seen.len(), 16);
        assert_eq!(
            files(dir.path()).len(),
            16,
            "no .nicti-tmp or placeholder left over"
        );
    }

    #[test]
    fn a_failure_leaves_no_placeholder_or_temp_behind() {
        let dir = tempfile::tempdir().unwrap();
        // The parent "folder" is a file, so create_dir_all fails.
        let blocker = dir.path().join("blocker");
        fs::write(&blocker, b"x").unwrap();
        let err = write_output(
            &blocker.join("x.jpg"),
            b"data",
            CollisionPolicy::UniqueSuffix,
        );
        assert!(matches!(err, Err(WriteError::CreateDir { .. })));
        assert_eq!(files(dir.path()), ["blocker"]);

        // Renaming a file onto a directory fails: the claim must be cleaned up.
        let target = dir.path().join("t.jpg");
        fs::create_dir(&target).unwrap();
        let res = write_output(&target, b"data", CollisionPolicy::Overwrite);
        assert!(res.is_err());
        assert_eq!(files(dir.path()), ["blocker", "t.jpg"], "temp file removed");
    }

    #[test]
    fn suffix_helper_handles_extensionless_names() {
        assert_eq!(with_suffix(Path::new("d/x"), 3), PathBuf::from("d/x-3"));
        assert_eq!(
            with_suffix(Path::new("d/x.tar.gz"), 2),
            PathBuf::from("d/x.tar-2.gz")
        );
    }
}
