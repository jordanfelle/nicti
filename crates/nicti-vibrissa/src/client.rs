//! A client for the control socket: connect, run the mutual handshake (including checking the
//! server's proof, which is what stops an impostor that bound the port first), then send
//! requests. A [`Session`] connects lazily and drops the connection on any error, so the next call
//! reconnects; with a discovered token file it looks the file up again, which is how an app restart
//! (new pid, new file) is survived. It never retries a request itself: one that was sent may
//! already have been applied.
//!
//! Every wait has a *total* deadline (not a per-read timeout a trickling peer could renew), and
//! requests are strictly one at a time per connection, so any reply line is the answer to the
//! request in flight.

use std::io::{self, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::wire::{find_token_file, mac, mac_eq, read_line_limited, TokenInfo};

/// Protocol version this client speaks (the app's `PROTOCOL`).
pub const PROTOCOL: u64 = 2;
/// Longest reply line accepted. Replies are not bound by the app's 1 MiB *request* cap: a
/// screenshot is a few MiB.
pub const MAX_REPLY: usize = 64 << 20;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// The whole handshake, hello to proof.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// One request, write to last byte of the reply: a little over the app's own 30 s request
/// timeout, so its error reaches us before ours fires.
const REPLY_TIMEOUT: Duration = Duration::from_secs(40);

/// Anything the MCP layer can send commands through.
pub trait Backend {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String>;
}

/// Where the token file is.
#[derive(Debug, Clone)]
pub enum TokenLocation {
    /// This file, exactly.
    File(PathBuf),
    /// The one `nicti-control-*.json` in this directory, looked up on every (re)connect.
    Discover(PathBuf),
}

/// Where to find the app.
#[derive(Debug, Clone)]
pub struct Source {
    pub token: TokenLocation,
    /// Overrides the port in the token file (an `ssh -L` forward usually has a different local port).
    pub port: Option<u16>,
}

impl Source {
    pub fn file(path: impl Into<PathBuf>) -> Self {
        Self {
            token: TokenLocation::File(path.into()),
            port: None,
        }
    }
}

/// A reader whose reads all share one deadline: a peer sending a byte every few seconds cannot
/// keep a call alive past it.
struct DeadlineRead {
    stream: TcpStream,
    deadline: Instant,
}

impl Read for DeadlineRead {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let left = self
            .deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::TimedOut, "timed out waiting for the app")
            })?;
        self.stream.set_read_timeout(Some(left))?;
        self.stream.read(buf)
    }
}

struct Conn {
    stream: TcpStream,
    reader: BufReader<DeadlineRead>,
    next_id: u64,
}

impl Conn {
    fn set_deadline(&mut self, after: Duration) {
        self.reader.get_mut().deadline = Instant::now() + after;
    }
}

pub struct Session {
    source: Source,
    conn: Option<Conn>,
}

impl Session {
    pub fn new(source: Source) -> Self {
        Self { source, conn: None }
    }

    fn connect(&self) -> Result<Conn, String> {
        let path = match &self.source.token {
            TokenLocation::File(p) => p.clone(),
            TokenLocation::Discover(dir) => find_token_file(dir)?,
        };
        let info = TokenInfo::read(&path)?;
        let port = self.source.port.unwrap_or(info.port);
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
            .map_err(|e| format!("can't connect to the app on 127.0.0.1:{port}: {e}"))?;
        let _ = stream.set_nodelay(true);
        stream
            .set_write_timeout(Some(CONNECT_TIMEOUT))
            .map_err(|e| e.to_string())?;
        let read_half = stream.try_clone().map_err(|e| e.to_string())?;
        let mut conn = Conn {
            stream,
            reader: BufReader::new(DeadlineRead {
                stream: read_half,
                deadline: Instant::now() + HANDSHAKE_TIMEOUT,
            }),
            next_id: 1,
        };
        handshake(&mut conn, &info.token)?;
        Ok(conn)
    }
}

fn new_cnonce() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| format!("the OS random generator failed: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn handshake(conn: &mut Conn, token: &str) -> Result<(), String> {
    let hello = read_line_limited(&mut conn.reader, 4096)
        .map_err(|e| format!("no hello from the app: {e}"))?
        .ok_or("the app closed the connection")?;
    let hello: Value = serde_json::from_str(&hello).map_err(|_| "that is not a nicti app")?;
    if hello["hello"] != "nicti-control" {
        // The app answers a connection it has no room for with {"ok":false,"error":..}.
        return Err(hello["error"].as_str().map_or_else(
            || "that port is not the nicti control channel".to_owned(),
            |e| format!("the app refused the connection: {e}"),
        ));
    }
    if hello["protocol"].as_u64() != Some(PROTOCOL) {
        return Err(format!(
            "the app speaks control protocol {}, this client speaks {PROTOCOL}",
            hello["protocol"]
        ));
    }
    let nonce = hello["nonce"].as_str().ok_or("hello has no nonce")?;
    let cnonce = new_cnonce()?;
    let auth = json!({"auth": mac(token, "client", nonce, &cnonce), "cnonce": cnonce});
    writeln!(conn.stream, "{auth}").map_err(|e| e.to_string())?;
    let reply = read_line_limited(&mut conn.reader, 4096)
        .map_err(|e| format!("handshake failed: {e}"))?
        .ok_or("the app closed the connection (wrong token file?)")?;
    let reply: Value = serde_json::from_str(&reply).map_err(|_| "bad handshake reply")?;
    if reply["ok"] != true {
        return Err(match reply["error"].as_str() {
            Some(e) => format!("the app refused the handshake: {e}"),
            None => "the app refused the token (is the token file from this instance?)".to_owned(),
        });
    }
    let expect = mac(token, "server", nonce, &cnonce);
    match reply["proof"].as_str() {
        Some(p) if mac_eq(p, &expect) => Ok(()),
        _ => Err("the server's proof is wrong: something other than the app owns that port".into()),
    }
}

impl Backend for Session {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        if self.conn.is_none() {
            self.conn = Some(self.connect()?);
        }
        let Some(conn) = self.conn.as_mut() else {
            return Err("not connected".into());
        };
        let id = conn.next_id;
        conn.next_id += 1;
        conn.set_deadline(REPLY_TIMEOUT);
        let line = json!({"id": id, "method": method, "params": params});
        let io = (|| -> Result<Value, String> {
            writeln!(conn.stream, "{line}").map_err(|e| e.to_string())?;
            let text = read_line_limited(&mut conn.reader, MAX_REPLY)
                .map_err(|e| e.to_string())?
                .ok_or("the app closed the connection")?;
            serde_json::from_str::<Value>(&text).map_err(|e| e.to_string())
        })();
        let v = match io {
            Ok(v) => v,
            Err(e) => {
                self.conn = None; // reconnect next time; this request is not repeated
                return Err(format!("connection to the app failed: {e}"));
            }
        };
        // One request at a time per connection, so this is the answer to ours. The app's own
        // refusals ("request too large", "not UTF-8") carry no id; they are still its answer.
        if !v["id"].is_null() && v["id"] != id {
            self.conn = None;
            return Err("the app answered a different request (protocol error)".to_owned());
        }
        if v["ok"] == true {
            Ok(v["result"].clone())
        } else {
            Err(v["error"]
                .as_str()
                .unwrap_or("the app reported an error")
                .to_owned())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader as StdReader};
    use std::net::TcpListener;
    use std::thread;

    /// A scripted app: runs the server side of the handshake with `token`, then hands the
    /// stream to `then`.
    fn app(
        token: &'static str,
        then: impl FnOnce(TcpStream) + Send + 'static,
    ) -> (u16, thread::JoinHandle<()>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let h = thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            writeln!(
                s,
                r#"{{"hello":"nicti-control","protocol":{PROTOCOL},"nonce":"abcd"}}"#
            )
            .unwrap();
            let mut r = StdReader::new(s.try_clone().unwrap());
            let mut line = String::new();
            r.read_line(&mut line).unwrap();
            let v: Value = serde_json::from_str(&line).unwrap();
            let cn = v["cnonce"].as_str().unwrap().to_owned();
            assert_eq!(v["auth"], mac(token, "client", "abcd", &cn).as_str());
            writeln!(
                s,
                r#"{{"ok":true,"proof":"{}"}}"#,
                mac(token, "server", "abcd", &cn)
            )
            .unwrap();
            then(s);
        });
        (port, h)
    }

    fn token_file(dir: &std::path::Path, name: &str, port: u16, token: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(
            &p,
            json!({"port": port, "token": token, "pid": 1}).to_string(),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        p
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("vib-c-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        d
    }

    #[test]
    fn the_apps_id_less_refusal_is_reported_not_skipped() {
        let d = tmp("idless");
        let (port, h) = app("tok", |mut s| {
            let mut r = StdReader::new(s.try_clone().unwrap());
            let mut l = String::new();
            r.read_line(&mut l).unwrap();
            writeln!(s, r#"{{"ok":false,"error":"request too large"}}"#).unwrap();
        });
        let mut c = Session::new(Source::file(token_file(
            &d,
            "nicti-control-1.json",
            port,
            "tok",
        )));
        assert_eq!(c.call("x", json!({})).unwrap_err(), "request too large");
        h.join().unwrap();
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_reply_to_another_request_is_a_protocol_error_and_drops_the_connection() {
        let d = tmp("wrongid");
        let (port, h) = app("tok", |mut s| {
            let mut r = StdReader::new(s.try_clone().unwrap());
            let mut l = String::new();
            r.read_line(&mut l).unwrap();
            writeln!(s, r#"{{"id":99,"ok":true,"result":1}}"#).unwrap();
        });
        let mut c = Session::new(Source::file(token_file(
            &d,
            "nicti-control-1.json",
            port,
            "tok",
        )));
        assert!(c
            .call("x", json!({}))
            .unwrap_err()
            .contains("different request"));
        assert!(c.conn.is_none());
        h.join().unwrap();
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_trickling_peer_cannot_outlast_the_total_deadline() {
        let d = tmp("trickle");
        let (port, h) = app("tok", |mut s| {
            // Never finishes a line; a byte every 100 ms.
            for _ in 0..200 {
                if s.write_all(b" ").is_err() {
                    return;
                }
                thread::sleep(Duration::from_millis(100));
            }
        });
        let mut c = Session::new(Source::file(token_file(
            &d,
            "nicti-control-1.json",
            port,
            "tok",
        )));
        // A short test-only deadline: the first call connects, then we shrink the reply window.
        c.conn = Some(c.connect().unwrap());
        let started = Instant::now();
        let conn = c.conn.as_mut().unwrap();
        conn.set_deadline(Duration::from_millis(600));
        let r = read_line_limited(&mut conn.reader, MAX_REPLY);
        assert!(r.is_err(), "{r:?}");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "the trickle renewed the timeout"
        );
        drop(c);
        let _ = h.join();
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_refused_connection_shows_the_apps_reason() {
        let d = tmp("full");
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let h = thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            writeln!(s, r#"{{"ok":false,"error":"too many connections"}}"#).unwrap();
        });
        let mut c = Session::new(Source::file(token_file(
            &d,
            "nicti-control-1.json",
            port,
            "tok",
        )));
        let e = c.call("x", json!({})).unwrap_err();
        assert!(e.contains("too many connections"), "{e}");
        h.join().unwrap();
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn after_an_app_restart_a_discovering_session_follows_the_new_token_file() {
        let d = tmp("restart");
        let (p1, h1) = app("tok1", |mut s| {
            let mut r = StdReader::new(s.try_clone().unwrap());
            let mut l = String::new();
            r.read_line(&mut l).unwrap();
            writeln!(s, r#"{{"id":1,"ok":true,"result":"one"}}"#).unwrap();
        });
        let old = token_file(&d, "nicti-control-100.json", p1, "tok1");
        let mut c = Session::new(Source {
            token: TokenLocation::Discover(d.clone()),
            port: None,
        });
        assert_eq!(c.call("x", json!({})).unwrap(), "one");
        h1.join().unwrap(); // the app exits; its token file is removed
        std::fs::remove_file(old).unwrap();
        // The next instance has another pid, port and token.
        let (p2, h2) = app("tok2", |mut s| {
            let mut r = StdReader::new(s.try_clone().unwrap());
            let mut l = String::new();
            r.read_line(&mut l).unwrap();
            writeln!(s, r#"{{"id":1,"ok":true,"result":"two"}}"#).unwrap();
        });
        token_file(&d, "nicti-control-200.json", p2, "tok2");
        assert!(
            c.call("x", json!({})).is_err(),
            "the dead connection fails once"
        );
        assert_eq!(c.call("x", json!({})).unwrap(), "two");
        h2.join().unwrap();
        let _ = std::fs::remove_dir_all(&d);
    }
}
