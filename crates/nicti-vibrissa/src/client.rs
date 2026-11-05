//! A client for the control socket: connect, run the mutual handshake (including checking the
//! server's proof, which is what stops an impostor that bound the port first), then send
//! requests. A [`Session`] connects lazily and drops the connection on any I/O error, so the next
//! call reconnects (and re-reads the token file, which is how an app restart is survived). It
//! never retries a request itself: one that was sent may already have been applied.

use std::io::{BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};

use crate::wire::{mac, mac_eq, read_line_limited, TokenInfo};

/// Protocol version this client speaks (the app's `PROTOCOL`).
pub const PROTOCOL: u64 = 2;
/// Longest reply line accepted. Replies are not bound by the app's 1 MiB *request* cap: a
/// screenshot is a few MiB.
pub const MAX_REPLY: usize = 64 << 20;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// A little over the app's own 30 s request timeout, so its error reaches us before ours fires.
const REPLY_TIMEOUT: Duration = Duration::from_secs(40);

/// Anything the MCP layer can send commands through.
pub trait Backend {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String>;
}

/// Where to find the app.
#[derive(Debug, Clone)]
pub struct Source {
    pub token_file: PathBuf,
    /// Overrides the port in the token file (an `ssh -L` forward usually has a different local port).
    pub port: Option<u16>,
}

struct Conn {
    stream: TcpStream,
    reader: BufReader<TcpStream>,
    next_id: u64,
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
        let info = TokenInfo::read(&self.source.token_file)?;
        let port = self.source.port.unwrap_or(info.port);
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
            .map_err(|e| format!("can't connect to the app on 127.0.0.1:{port}: {e}"))?;
        let _ = stream.set_nodelay(true);
        stream
            .set_read_timeout(Some(REPLY_TIMEOUT))
            .map_err(|e| e.to_string())?;
        stream
            .set_write_timeout(Some(CONNECT_TIMEOUT))
            .map_err(|e| e.to_string())?;
        let reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
        let mut conn = Conn {
            stream,
            reader,
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
        return Err("that port is not the nicti control channel".into());
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
        return Err("the app refused the token (is the token file from this instance?)".into());
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
        let line = json!({"id": id, "method": method, "params": params});
        let io = (|| -> Result<Value, String> {
            writeln!(conn.stream, "{line}").map_err(|e| e.to_string())?;
            loop {
                let text = read_line_limited(&mut conn.reader, MAX_REPLY)
                    .map_err(|e| e.to_string())?
                    .ok_or("the app closed the connection")?;
                let v: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
                if v["id"] == id {
                    return Ok(v);
                }
                // A reply to an earlier request that timed out here: skip it.
            }
        })();
        match io {
            Err(e) => {
                self.conn = None; // reconnect next time; this request is not repeated
                Err(format!("connection to the app failed: {e}"))
            }
            Ok(v) if v["ok"] == true => Ok(v["result"].clone()),
            Ok(v) => Err(v["error"]
                .as_str()
                .unwrap_or("the app reported an error")
                .to_owned()),
        }
    }
}
