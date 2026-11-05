//! Resolves and opens this session's catalog file. `SqliteCatalog::open` creates the file if it's
//! missing (see its own doc comment) -- there's no separate "create" call to make.

use std::path::PathBuf;

use nicti_lair::{CatalogError, SqliteCatalog};

const DEFAULT_CATALOG_FILENAME: &str = "nicti.catalog.sqlite";

/// The catalog path for this session: the first *positional* CLI argument (a double-clicked
/// catalog file or a scripted launch), else `nicti.catalog.sqlite` in the current directory when
/// one already exists there (so a catalog created by an earlier build is still found), else the
/// per-user data directory. Flags (`--control`, ...) are never a catalog path (#758).
pub fn resolve_path() -> PathBuf {
    resolve_path_from(
        std::env::args().skip(1),
        &std::env::current_dir().unwrap_or_default(),
        default_data_dir().as_deref(),
    )
}

/// The testable core of [`resolve_path`].
pub(crate) fn resolve_path_from(
    args: impl IntoIterator<Item = String>,
    cwd: &std::path::Path,
    data_dir: Option<&std::path::Path>,
) -> PathBuf {
    if let Some(positional) = positional_arg(args) {
        return PathBuf::from(positional);
    }
    let legacy = cwd.join(DEFAULT_CATALOG_FILENAME);
    if legacy.is_file() {
        return legacy;
    }
    match data_dir {
        Some(dir) => {
            // Best effort: `SqliteCatalog::open` reports a real failure in the launch window.
            let _ = std::fs::create_dir_all(dir);
            dir.join(DEFAULT_CATALOG_FILENAME)
        }
        None => PathBuf::from(DEFAULT_CATALOG_FILENAME),
    }
}

/// The first argument that is not an option or an option's value. Options that take a value
/// (`--control-export-dir X`) skip it; anything starting with `-` is an option.
fn positional_arg(args: impl IntoIterator<Item = String>) -> Option<String> {
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--control-export-dir" || arg == "--control-port" {
            args.next();
        } else if !arg.starts_with('-') {
            return Some(arg);
        }
    }
    None
}

/// `%LOCALAPPDATA%\nicti`, `$XDG_DATA_HOME/nicti`, or `~/.local/share/nicti`; `None` when none of
/// those can be resolved.
fn default_data_dir() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .filter(|v| !v.is_empty())
                    .map(|h| PathBuf::from(h).join(".local/share"))
            })
    }?;
    Some(base.join("nicti"))
}

pub fn open(path: &std::path::Path) -> Result<SqliteCatalog, CatalogError> {
    SqliteCatalog::open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn a_leading_flag_is_not_the_catalog_path() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let got = resolve_path_from(args(&["--control"]), dir.path(), Some(&data));
        assert_eq!(got, data.join(DEFAULT_CATALOG_FILENAME));
        assert!(data.is_dir(), "the data directory is created");
    }

    #[test]
    fn the_first_positional_wins_wherever_the_flags_are() {
        let d = std::path::Path::new("/nonexistent");
        for a in [
            &["my.sqlite", "--control"][..],
            &["--control", "my.sqlite"],
            &["--control-export-dir", "/x", "my.sqlite", "--control"],
        ] {
            assert_eq!(
                resolve_path_from(args(a), d, None),
                PathBuf::from("my.sqlite"),
                "{a:?}"
            );
        }
    }

    #[test]
    fn a_flag_value_is_not_a_catalog() {
        let d = std::path::Path::new("/nonexistent");
        let got = resolve_path_from(args(&["--control-export-dir", "/exports"]), d, None);
        assert_eq!(got, PathBuf::from(DEFAULT_CATALOG_FILENAME));
    }

    #[test]
    fn an_existing_catalog_in_the_cwd_is_still_found() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(DEFAULT_CATALOG_FILENAME), b"").unwrap();
        let data = dir.path().join("data");
        let got = resolve_path_from(args(&[]), dir.path(), Some(&data));
        assert_eq!(got, dir.path().join(DEFAULT_CATALOG_FILENAME));
        assert!(
            !data.exists(),
            "no data directory is made when the cwd one is used"
        );
    }

    #[test]
    fn no_data_dir_falls_back_to_the_bare_filename() {
        let d = std::path::Path::new("/nonexistent");
        assert_eq!(
            resolve_path_from(args(&[]), d, None),
            PathBuf::from(DEFAULT_CATALOG_FILENAME)
        );
    }
}
