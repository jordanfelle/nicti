//! Host-side authorization (ADR-0214 section 3). WASI's own switch is binary (an import is linked
//! or not); the filtering that makes a grant *scoped* is Claw's job, done here as pure functions
//! the future `outgoing-handler` and preopen code call before acting: a request's authority
//! against the declared hosts and the **currently active** action scope, over HTTPS only, and a
//! path against the single directory the user picked.

use std::path::{Path, PathBuf};

use crate::manifest::{Capability, Host, Manifest, NetworkScope};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Denied {
    #[error("the module declared no network capability for the {0:?} action")]
    NoNetworkGrant(NetworkScope),
    #[error("the {declared:?} grant is not the action that is active ({active:?})")]
    ScopeNotActive {
        declared: NetworkScope,
        active: NetworkScope,
    },
    #[error("only https:// URLs are allowed")]
    NotHttps,
    #[error("malformed URL authority: {0}")]
    BadAuthority(&'static str),
    #[error("host {0:?} is not in the module's allowed_hosts")]
    HostNotAllowed(String),
    #[error("path escapes the granted directory")]
    PathEscapes,
    #[error("path is not usable: {0}")]
    BadPath(String),
}

/// The authority of an `https://` URL, validated and lowercased; port 443 (explicit or implied)
/// is the only port. Userinfo, IP literals, percent-escapes, backslashes and trailing dots are
/// refused, because each is a classic way to make a different host look like an allowed one.
fn https_host(url: &str) -> Result<String, Denied> {
    let rest = url
        .get(..8)
        .filter(|p| p.eq_ignore_ascii_case("https://"))
        .map(|_| &url[8..])
        .ok_or(Denied::NotHttps)?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    if authority.is_empty() {
        return Err(Denied::BadAuthority("empty host"));
    }
    if authority
        .chars()
        .any(|c| c.is_control() || c.is_whitespace() || matches!(c, '@' | '\\' | '%' | '[' | ']'))
    {
        return Err(Denied::BadAuthority(
            "userinfo, escapes, brackets or whitespace",
        ));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (authority, None),
    };
    if port.is_some_and(|p| p != "443") {
        return Err(Denied::BadAuthority("only port 443"));
    }
    let host = host.to_ascii_lowercase();
    Host::parse(&host).map_err(|_| Denied::BadAuthority("not a canonical DNS host"))?;
    Ok(host)
}

/// Whether the module may issue a request to `url` while `active` is the running action.
pub fn authorize_request(
    manifest: &Manifest,
    active: NetworkScope,
    url: &str,
) -> Result<(), Denied> {
    let grant = manifest.capabilities.iter().find_map(|c| match c {
        Capability::Network {
            scope,
            allowed_hosts,
            ..
        } => Some((*scope, allowed_hosts)),
        _ => None,
    });
    // A module declares at most one network capability per scope; find the one for `active`.
    let for_active = manifest.capabilities.iter().find_map(|c| match c {
        Capability::Network {
            scope,
            allowed_hosts,
            ..
        } if *scope == active => Some(allowed_hosts),
        _ => None,
    });
    let hosts = match (for_active, grant) {
        (Some(hosts), _) => hosts,
        (None, Some((declared, _))) => {
            return Err(Denied::ScopeNotActive { declared, active });
        }
        (None, None) => return Err(Denied::NoNetworkGrant(active)),
    };
    let host = https_host(url)?;
    if hosts.iter().any(|h| h.as_str() == host) {
        Ok(())
    } else {
        Err(Denied::HostNotAllowed(host))
    }
}

/// Resolves `requested` (absolute, or relative to `root`) and returns its canonical path only if
/// it stays inside the canonical `root`. A path that does not exist yet (a file about to be
/// created) is judged by its parent directory. `..`, symlinks out of the root and a root that is
/// itself a symlink are all caught by comparing canonical paths, not strings.
pub fn authorize_path(root: &Path, requested: &Path) -> Result<PathBuf, Denied> {
    let bad = |e: std::io::Error| Denied::BadPath(e.to_string());
    let root = root.canonicalize().map_err(bad)?;
    if root.parent().is_none() {
        return Err(Denied::BadPath(
            "a filesystem root cannot be granted".into(),
        ));
    }
    let joined = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        root.join(requested)
    };
    let resolved = match joined.canonicalize() {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // A dangling symlink also reads as NotFound; creating through it would write wherever
            // it points.
            if std::fs::symlink_metadata(&joined).is_ok() {
                return Err(Denied::PathEscapes);
            }
            let name = joined
                .file_name()
                .filter(|n| *n != "..")
                .ok_or_else(|| Denied::BadPath("no file name".into()))?;
            let parent = joined
                .parent()
                .ok_or_else(|| Denied::BadPath("no parent".into()))?;
            parent.canonicalize().map_err(bad)?.join(name)
        }
        Err(e) => return Err(bad(e)),
    };
    if resolved.starts_with(&root) {
        Ok(resolved)
    } else {
        Err(Denied::PathEscapes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{parse, tests::GOOD};
    use std::fs;

    fn manifest() -> Manifest {
        parse(GOOD).unwrap()
    }

    const WD: NetworkScope = NetworkScope::WeightDownload;

    #[test]
    fn a_declared_host_over_https_in_the_active_scope_is_allowed() {
        let m = manifest();
        for url in [
            "https://cdn.vendor.example.com/weights.onnx",
            "https://CDN.Vendor.Example.com/x",
            "HTTPS://cdn.vendor.example.com",
            "https://cdn.vendor.example.com:443/x?y=1#z",
            "https://cdn.vendor.example.com?q",
        ] {
            assert_eq!(authorize_request(&m, WD, url), Ok(()), "{url}");
        }
    }

    #[test]
    fn plain_http_and_other_schemes_are_refused() {
        let m = manifest();
        for url in [
            "http://cdn.vendor.example.com/x",
            "ftp://cdn.vendor.example.com/x",
            "//cdn.vendor.example.com/x",
            "cdn.vendor.example.com/x",
            "",
            "https:/cdn.vendor.example.com",
        ] {
            assert_eq!(
                authorize_request(&m, WD, url),
                Err(Denied::NotHttps),
                "{url}"
            );
        }
    }

    #[test]
    fn lookalike_authorities_do_not_reach_an_allowed_host() {
        let m = manifest();
        for url in [
            "https://evil.example.com@cdn.vendor.example.com/x",
            "https://cdn.vendor.example.com@evil.example.com/x",
            "https://cdn.vendor.example.com.evil.example.com/x",
            "https://evilcdn.vendor.example.com/x",
            "https://sub.cdn.vendor.example.com/x",
            "https://cdn.vendor.example.com:8443/x",
            "https://cdn.vendor.example.com:80/x",
            "https://cdn.vendor.example.com./x",
            "https://cdn%2evendor.example.com/x",
            "https://cdn.vendor.example.com\\@evil.example.com/",
            "https://cdn.vendor.example.com /x",
            "https://[::1]/x",
            "https://127.0.0.1/x",
            "https://10.0.0.5/x",
            "https:///x",
            "https://",
            "https://xn--cdn.vendor.example.com/x",
        ] {
            assert!(authorize_request(&m, WD, url).is_err(), "{url}");
        }
        assert!(matches!(
            authorize_request(&m, WD, "https://other.example.com/x"),
            Err(Denied::HostNotAllowed(h)) if h == "other.example.com"
        ));
    }

    #[test]
    fn the_grant_only_applies_to_the_action_that_is_active() {
        let m = manifest();
        assert_eq!(
            authorize_request(
                &m,
                NetworkScope::CloudFeature,
                "https://cdn.vendor.example.com/x"
            ),
            Err(Denied::ScopeNotActive {
                declared: WD,
                active: NetworkScope::CloudFeature
            })
        );
        let none = parse(
            "id = \"a.b\"\nversion = \"1.0.0\"\nhost_api_version = 1\nextension_points = [\"Exporter\"]\n",
        )
        .unwrap();
        assert_eq!(
            authorize_request(&none, WD, "https://cdn.vendor.example.com/x"),
            Err(Denied::NoNetworkGrant(WD))
        );
    }

    #[test]
    fn a_path_inside_the_granted_directory_is_allowed() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("refs")).unwrap();
        fs::write(dir.path().join("refs/a.jpg"), b"x").unwrap();
        let root = dir.path().join("refs");
        let inside = authorize_path(&root, Path::new("a.jpg")).unwrap();
        assert_eq!(inside, root.canonicalize().unwrap().join("a.jpg"));
        assert!(
            authorize_path(&root, &root.join("a.jpg")).is_ok(),
            "absolute inside"
        );
        assert!(
            authorize_path(&root, Path::new("new.jpg")).is_ok(),
            "a file to be created"
        );
        assert!(authorize_path(&root, Path::new(".")).is_ok());
    }

    #[test]
    fn a_path_outside_the_directory_is_refused_however_it_is_spelled() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("refs");
        fs::create_dir_all(&root).unwrap();
        fs::write(dir.path().join("secret.txt"), b"s").unwrap();
        for p in [
            "../secret.txt",
            "../../etc/passwd",
            "sub/../../secret.txt",
            "../refs/../secret.txt",
        ] {
            assert!(authorize_path(&root, Path::new(p)).is_err(), "{p}");
        }
        assert_eq!(
            authorize_path(&root, &dir.path().join("secret.txt")),
            Err(Denied::PathEscapes)
        );
        // A sibling whose name merely starts with the root's name is outside it.
        fs::create_dir_all(dir.path().join("refs_evil")).unwrap();
        assert_eq!(
            authorize_path(&root, &dir.path().join("refs_evil")),
            Err(Denied::PathEscapes)
        );
        // Creating a file whose name is `..` is refused.
        assert!(authorize_path(&root, Path::new("newdir/..")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_the_directory_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("refs");
        fs::create_dir_all(&root).unwrap();
        let outside = dir.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret.txt"), b"s").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        assert_eq!(
            authorize_path(&root, Path::new("link/secret.txt")),
            Err(Denied::PathEscapes)
        );
        // A not-yet-existing file below a symlinked directory is judged by where it would land.
        assert_eq!(
            authorize_path(&root, Path::new("link/new.txt")),
            Err(Denied::PathEscapes)
        );
        // A root that is itself a symlink is resolved, then contained.
        let alias = dir.path().join("alias");
        std::os::unix::fs::symlink(&root, &alias).unwrap();
        fs::write(root.join("ok.txt"), b"k").unwrap();
        assert!(authorize_path(&alias, Path::new("ok.txt")).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_symlink_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("refs");
        fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(dir.path().join("new.txt"), root.join("link")).unwrap();
        assert_eq!(
            authorize_path(&root, Path::new("link")),
            Err(Denied::PathEscapes)
        );
        assert!(authorize_path(Path::new("/"), Path::new("etc/passwd")).is_err());
    }

    #[test]
    fn a_missing_root_is_an_error_not_a_grant_of_everything() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            authorize_path(&dir.path().join("nope"), Path::new("a")),
            Err(Denied::BadPath(_))
        ));
    }
}
