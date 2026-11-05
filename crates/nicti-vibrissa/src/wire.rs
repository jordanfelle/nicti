//! Pieces both ends of the socket must agree on byte for byte: the handshake MAC and the token
//! file. `nicti-pelt`'s server uses [`mac`] from here, so the two cannot drift apart.

use std::io::{self, BufRead};
use std::path::{Path, PathBuf};

use serde_json::Value;

/// `blake3_keyed(blake3(token), len|role | len|nonce | len|cnonce)`, hex. `role` is `"client"` or
/// `"server"`. Each part is length-prefixed so ("a","bc") and ("ab","c") can never collide.
pub fn mac(token: &str, role: &str, nonce: &str, cnonce: &str) -> String {
    let key = *blake3::hash(token.as_bytes()).as_bytes();
    let mut msg = Vec::new();
    for part in [role, nonce, cnonce] {
        msg.extend_from_slice(&(part.len() as u32).to_le_bytes());
        msg.extend_from_slice(part.as_bytes());
    }
    blake3::keyed_hash(&key, &msg).to_hex().to_string()
}

/// Constant-time equality for two MACs (no early exit on the first differing byte).
pub fn mac_eq(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

/// One line without its newline, refusing anything longer than `max` without buffering the
/// excess. `None` at a clean end of stream.
pub fn read_line_limited<R: BufRead>(r: &mut R, max: usize) -> io::Result<Option<String>> {
    let mut line = Vec::new();
    loop {
        let buf = r.fill_buf()?;
        if buf.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "unterminated line",
                ))
            };
        }
        let (take, done) = match buf.iter().position(|&b| b == b'\n') {
            Some(i) => (i, true),
            None => (buf.len(), false),
        };
        if line.len() + take > max {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
        }
        line.extend_from_slice(&buf[..take]);
        r.consume(take + usize::from(done));
        if done {
            break;
        }
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    String::from_utf8(line)
        .map(Some)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "not UTF-8"))
}

/// A token file is a few dozen bytes.
const MAX_TOKEN_FILE: u64 = 4096;

/// What the app writes to `nicti-control-<pid>.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenInfo {
    pub port: u16,
    pub token: String,
    pub pid: u32,
}

impl TokenInfo {
    pub fn parse(text: &str) -> Result<Self, String> {
        let v: Value = serde_json::from_str(text).map_err(|e| format!("not a token file: {e}"))?;
        let port = v["port"]
            .as_u64()
            .and_then(|p| u16::try_from(p).ok())
            .filter(|p| *p != 0)
            .ok_or("token file has no valid `port`")?;
        let token = v["token"]
            .as_str()
            .filter(|t| !t.is_empty())
            .ok_or("token file has no `token`")?
            .to_owned();
        let pid = v["pid"]
            .as_u64()
            .and_then(|p| u32::try_from(p).ok())
            .ok_or("token file has no `pid`")?;
        Ok(Self { port, token, pid })
    }

    /// Reads a token file, refusing anything but a small regular file (not a symlink, FIFO or
    /// device: a FIFO would block forever, `/dev/zero` would fill memory) that, on Unix, only its
    /// owner can touch. The app creates it `0600`. Use a private directory: a planted file in a
    /// shared one would make the client trust whoever planted it.
    pub fn read(path: &Path) -> Result<Self, String> {
        let shown = path.display();
        let meta =
            std::fs::symlink_metadata(path).map_err(|e| format!("can't read {shown}: {e}"))?;
        if !meta.is_file() {
            return Err(format!("{shown} is not a regular file"));
        }
        if meta.len() > MAX_TOKEN_FILE {
            return Err(format!("{shown} is too large to be a token file"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            // The directory matters as much as the file: anyone who can write it can rename a
            // planted file over this one. It must be owned by the file's owner and not
            // group/world-writable (so `/tmp`, sticky or not, is refused).
            let dir = path
                .parent()
                .filter(|d| !d.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            let dmeta = std::fs::metadata(dir)
                .map_err(|e| format!("can't inspect {}: {e}", dir.display()))?;
            if dmeta.permissions().mode() & 0o022 != 0 || dmeta.uid() != meta.uid() {
                return Err(format!(
                    "{} is writable by other users or owned by someone else: a planted token \
                     file could replace the real one; keep it in a private directory",
                    dir.display()
                ));
            }
            if meta.permissions().mode() & 0o077 != 0 {
                return Err(format!(
                    "{shown} is readable by other users (mode {:o}); the app creates it 0600",
                    meta.permissions().mode() & 0o777
                ));
            }
        }
        let text = std::fs::read_to_string(path).map_err(|e| format!("can't read {shown}: {e}"))?;
        Self::parse(&text)
    }
}

/// The per-user directory the app writes its token file to (`$XDG_RUNTIME_DIR`, or on Windows
/// `%LOCALAPPDATA%\nicti`); `None` when neither is set.
pub fn default_token_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) {
        if d.is_absolute() {
            return Some(d);
        }
    }
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .filter(|d| d.is_absolute())
        .map(|d| d.join("nicti"))
}

/// The one `nicti-control-*.json` in `dir`. Several (several running instances, or stale files)
/// is an error that lists them: guessing which app to drive is not acceptable.
pub fn find_token_file(dir: &Path) -> Result<PathBuf, String> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| format!("can't read {}: {e}", dir.display()))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| std::fs::symlink_metadata(p).is_ok_and(|m| m.is_file()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("nicti-control-") && n.ends_with(".json"))
        })
        .collect();
    found.sort();
    match found.len() {
        0 => Err(format!(
            "no nicti-control-*.json in {}: is the app running with --control?",
            dir.display()
        )),
        1 => Ok(found.remove(0)),
        _ => Err(format!(
            "several token files in {} ({}): pass --token-file for the instance to drive",
            dir.display(),
            found
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn mac_is_keyed_role_bound_and_length_prefixed() {
        let a = mac("tok", "client", "n", "c");
        assert_eq!(a, mac("tok", "client", "n", "c"));
        assert_ne!(a, mac("tok", "server", "n", "c"), "role is bound in");
        assert_ne!(a, mac("tok2", "client", "n", "c"), "token is the key");
        assert_ne!(
            mac("t", "a", "bc", "d"),
            mac("t", "ab", "c", "d"),
            "no concatenation collision"
        );
        assert!(mac_eq(&a, &a) && !mac_eq(&a, &mac("tok", "server", "n", "c")) && !mac_eq(&a, "x"));
    }

    #[test]
    fn lines_are_capped_without_buffering_the_excess() {
        let mut r = Cursor::new(b"abc\r\nxyz".to_vec());
        assert_eq!(
            read_line_limited(&mut r, 10).unwrap().as_deref(),
            Some("abc")
        );
        assert!(read_line_limited(&mut r, 10).is_err(), "unterminated");
        let mut r = Cursor::new(vec![b'a'; 100]);
        assert!(read_line_limited(&mut r, 10).is_err());
        let mut r = Cursor::new(Vec::new());
        assert_eq!(read_line_limited(&mut r, 10).unwrap(), None);
    }

    #[test]
    fn a_token_file_needs_port_token_and_pid() {
        let ok = TokenInfo::parse(r#"{"port":4000,"token":"ab","pid":7}"#).unwrap();
        assert_eq!((ok.port, ok.token.as_str(), ok.pid), (4000, "ab", 7));
        for bad in [
            r#"{"port":0,"token":"a","pid":1}"#,
            r#"{"port":70000,"token":"a","pid":1}"#,
            r#"{"port":1,"token":"","pid":1}"#,
            r#"{"port":1,"token":"a"}"#,
            "[]",
            "nope",
        ] {
            assert!(TokenInfo::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn token_files_are_found_only_when_unambiguous() {
        let d = std::env::temp_dir().join(format!("vib-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        assert!(find_token_file(&d)
            .unwrap_err()
            .contains("no nicti-control"));
        std::fs::write(d.join("other.json"), "{}").unwrap();
        std::fs::create_dir(d.join("nicti-control-9.json")).unwrap(); // a directory is not a token file
        std::fs::write(d.join("nicti-control-1.json"), "{}").unwrap();
        assert!(find_token_file(&d)
            .unwrap()
            .ends_with("nicti-control-1.json"));
        std::fs::write(d.join("nicti-control-2.json"), "{}").unwrap();
        assert!(find_token_file(&d).unwrap_err().contains("--token-file"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn a_token_file_must_be_a_private_regular_file() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let d = std::env::temp_dir().join(format!("vib-priv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let body = r#"{"port":4000,"token":"ab","pid":7}"#;
        let f = d.join("t.json");
        std::fs::write(&f, body).unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(TokenInfo::read(&f).is_ok());
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(TokenInfo::read(&f).unwrap_err().contains("other users"));
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = d.join("link.json");
        symlink(&f, &link).unwrap();
        assert!(TokenInfo::read(&link)
            .unwrap_err()
            .contains("not a regular file"));
        assert!(TokenInfo::read(&d)
            .unwrap_err()
            .contains("not a regular file"));
        // A world-writable directory is refused even with a private file inside.
        let open_dir = d.join("open");
        std::fs::create_dir(&open_dir).unwrap();
        let inner = open_dir.join("t.json");
        std::fs::write(&inner, body).unwrap();
        std::fs::set_permissions(&inner, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::set_permissions(&open_dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(TokenInfo::read(&inner)
            .unwrap_err()
            .contains("writable by other users"));
        std::fs::set_permissions(&open_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(TokenInfo::read(&inner).is_ok());
        let big = d.join("big.json");
        std::fs::write(&big, vec![b' '; 5000]).unwrap();
        std::fs::set_permissions(&big, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(TokenInfo::read(&big).unwrap_err().contains("too large"));
        let _ = std::fs::remove_dir_all(&d);
    }
}
