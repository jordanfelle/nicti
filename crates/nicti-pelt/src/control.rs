//! The opt-in control channel (#543, ADR-0434 slice 2a): a tokened JSON-lines socket on `127.0.0.1`
//! that submits requests to the command table ([`crate::cmd`]) and returns their replies.
//!
//! **Off unless asked for** (`--control[=PORT]` or `NICTI_CONTROL_PORT`). Loopback only, never
//! `0.0.0.0`. The token is 128 random bits from the OS and lives in a file created `O_EXCL` with
//! mode `0600`; the bound port is written *into* that file. The token **never crosses the wire**:
//! the server opens with `{"hello":..,"nonce":N}`, the client answers `{"auth":MAC,"cnonce":C}` with
//! `MAC = blake3_keyed(hash(token), "client"|N|C)` within 5 s, and the server replies with its own
//! proof `blake3_keyed(hash(token), "server"|N|C)`. So another local process that grabbed the port
//! first (Windows lets a second socket bind a listening 127.0.0.1 port unless `SO_EXCLUSIVEADDRUSE`)
//! can neither read the token nor impersonate the app to the client. A non-object, wrong or late
//! answer closes the connection, which also stops a web page's cross-origin `fetch` from smuggling a
//! request in an HTTP body (the browser cannot read a non-HTTP reply either).
//!
//! Threat model (ADR-0434 section 3): anything running as the same user can read the token file and
//! drive the app; that is the same trust boundary as the user's own session and catalog, and is why
//! the channel is off by default. Raw input injection, widgets and screenshots are later slices.

use std::io::{self, BufRead, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

use crate::cmd::{CommandQueue, Request};

/// Longest accepted request line (the ADR's number: nothing the channel carries needs more).
pub const MAX_LINE: usize = 1 << 20;
/// Authenticated connections at once.
pub const MAX_AUTHED: usize = 4;
/// Unauthenticated connections at once; each is held for at most [`AUTH_DEADLINE`].
pub const MAX_UNAUTHED: usize = 4;
/// How long a new connection has to send its token.
pub const AUTH_DEADLINE: Duration = Duration::from_secs(5);
/// How long one request may wait for the UI thread before the client is told it timed out.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// An authenticated connection that sends nothing for this long is closed, so idle sockets
/// cannot lock the agent out of the [`MAX_AUTHED`] slots.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
pub const PROTOCOL: u32 = 2;

/// What `--control...` asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlOptions {
    /// 0 = let the OS pick (the default); the bound port goes into the token file.
    pub port: u16,
    /// `--control-allow-destructive`.
    pub allow_destructive: bool,
}

impl ControlOptions {
    /// Parses the process arguments (without the program name) and `NICTI_CONTROL_PORT`. `None`
    /// when the channel was not asked for. An unknown `--control-*` flag is an error rather than
    /// silently ignored: a typo in a safety flag must not leave the channel looser or tighter than
    /// the operator thinks.
    pub fn parse<I>(args: I, env_port: Option<&str>) -> Result<Option<Self>, String>
    where
        I: IntoIterator<Item = String>,
    {
        let mut on = false;
        let mut port = 0u16;
        let mut allow_destructive = false;
        if let Some(p) = env_port.filter(|p| !p.is_empty()) {
            on = true;
            port = parse_port(p)?;
        }
        for arg in args {
            if arg == "--control" {
                on = true;
            } else if let Some(p) = arg.strip_prefix("--control=") {
                on = true;
                port = parse_port(p)?;
            } else if arg == "--control-allow-destructive" {
                allow_destructive = true;
            } else if arg.starts_with("--control") {
                return Err(format!("unknown option `{arg}`"));
            }
        }
        if allow_destructive && !on {
            return Err("--control-allow-destructive needs --control".into());
        }
        Ok(on.then_some(Self {
            port,
            allow_destructive,
        }))
    }
}

impl ControlOptions {
    /// What a client of this channel may run.
    pub fn policy(self) -> crate::cmd::Policy {
        crate::cmd::Policy {
            allow_destructive: self.allow_destructive,
        }
    }
}

fn parse_port(s: &str) -> Result<u16, String> {
    s.parse::<u16>()
        .map_err(|_| format!("`{s}` is not a port number (0-65535)"))
}

/// Constant-time equality for equal-length byte strings (length is not secret: the token's is
/// fixed). Different lengths are unequal.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// 128 random bits from the OS, as 32 hex characters.
pub fn new_token() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| format!("the OS random generator failed: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// The per-user directory the token file goes in: `$XDG_RUNTIME_DIR`, or on Windows the per-user
/// local app data directory (its default ACL is the protection; Unix mode bits mean nothing
/// there). Anywhere shared (`/tmp`) is refused rather than fallen back to.
pub fn token_dir() -> Result<PathBuf, String> {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) {
        if dir.is_absolute() {
            return Ok(dir);
        }
    }
    #[cfg(windows)]
    if let Some(dir) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) {
        if dir.is_absolute() {
            return Ok(dir.join("nicti"));
        }
    }
    Err(
        "no per-user runtime directory (XDG_RUNTIME_DIR / LOCALAPPDATA); \
         refusing to start the control channel"
            .into(),
    )
}

/// The token file: created exclusively, readable only by the user, removed on drop.
#[derive(Debug)]
pub struct TokenFile {
    path: PathBuf,
}

impl TokenFile {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Writes `{"port":..,"token":"..","pid":..}` to `dir/nicti-control-<pid>.json` (so a second
    /// instance gets its own file). A leftover from a crashed run with the same PID is replaced.
    pub fn create(dir: &Path, port: u16, token: &str) -> Result<Self, String> {
        std::fs::create_dir_all(dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
        check_dir_private(dir)?;
        let pid = std::process::id();
        let path = dir.join(format!("nicti-control-{pid}.json"));
        let body = json!({ "port": port, "token": token, "pid": pid }).to_string();
        for attempt in 0..2 {
            match open_exclusive(&path) {
                Ok(mut file) => {
                    file.write_all(body.as_bytes())
                        .and_then(|()| file.flush())
                        .map_err(|e| {
                            let _ = std::fs::remove_file(&path);
                            format!("can't write {}: {e}", path.display())
                        })?;
                    return Ok(Self { path });
                }
                // Stale from a crash. `remove_file` on a symlink removes the link, never its target.
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists && attempt == 0 => {
                    std::fs::remove_file(&path)
                        .map_err(|e| format!("can't replace {}: {e}", path.display()))?;
                }
                Err(e) => return Err(format!("can't create {}: {e}", path.display())),
            }
        }
        Err(format!("can't create {}", path.display()))
    }
}

impl Drop for TokenFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(unix)]
fn open_exclusive(path: &Path) -> io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    // The mode is applied at creation, so the file is never readable by anyone else, not even
    // for the instant before a chmod.
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn open_exclusive(path: &Path) -> io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

/// The runtime directory must not be accessible to other users, or the file's own mode is moot.
#[cfg(unix)]
fn check_dir_private(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(dir).map_err(|e| format!("can't read {}: {e}", dir.display()))?;
    if meta.permissions().mode() & 0o077 != 0 {
        return Err(format!(
            "{} is accessible to other users; refusing to put the control token there",
            dir.display()
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_dir_private(_dir: &Path) -> Result<(), String> {
    Ok(())
}

/// Reads one `\n`-terminated line of at most `max` bytes. `Ok(None)` is a clean end of stream.
/// A longer line is an error (the rest is never buffered), so a client cannot make the app
/// allocate without bound.
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
        match buf.iter().position(|&b| b == b'\n') {
            Some(i) => {
                if line.len() + i > max {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
                }
                line.extend_from_slice(&buf[..i]);
                r.consume(i + 1);
                break;
            }
            None => {
                if line.len() + buf.len() > max {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
                }
                line.extend_from_slice(buf);
                let n = buf.len();
                r.consume(n);
            }
        }
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    String::from_utf8(line)
        .map(Some)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "not UTF-8"))
}

/// A reader that enforces a *total* deadline across reads: a socket read timeout alone resets
/// on every byte, so a client trickling one byte every few seconds could hold a slot forever.
struct DeadlineRead {
    inner: TcpStream,
    deadline: Option<std::time::Instant>,
}

impl io::Read for DeadlineRead {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(d) = self.deadline {
            let left = d.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "authentication deadline",
                ));
            }
            self.inner.set_read_timeout(Some(left))?;
        }
        self.inner.read(buf)
    }
}

/// Decrements a counter when dropped.
struct Slot(Arc<AtomicUsize>);

impl Slot {
    /// Takes a slot if fewer than `max` are in use.
    fn take(counter: &Arc<AtomicUsize>, max: usize) -> Option<Self> {
        let mut cur = counter.load(Ordering::SeqCst);
        loop {
            if cur >= max {
                return None;
            }
            match counter.compare_exchange(cur, cur + 1, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return Some(Self(Arc::clone(counter))),
                Err(now) => cur = now,
            }
        }
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// What a connection thread needs from the app.
#[derive(Clone)]
pub struct Shared {
    token: Arc<String>,
    submit: mpsc::Sender<Request>,
    /// Wakes the UI thread so it drains the queue (an idle window runs no frames).
    wake: Arc<dyn Fn() + Send + Sync>,
    authed: Arc<AtomicUsize>,
    /// Connections that have not authenticated yet, oldest first. A new one evicts the oldest
    /// when [`MAX_UNAUTHED`] are open, so silent sockets from another local user cannot lock the
    /// real client out: the real client authenticates in milliseconds, they would have to open
    /// [`MAX_UNAUTHED`] new connections inside that window.
    pending: Arc<std::sync::Mutex<std::collections::VecDeque<(u64, TcpStream)>>>,
    next_id: Arc<std::sync::atomic::AtomicU64>,
    /// [`AUTH_DEADLINE`] outside tests.
    auth_deadline: Duration,
    /// [`REQUEST_TIMEOUT`] outside tests.
    request_timeout: Duration,
}

impl Shared {
    pub fn new(
        token: String,
        queue: &CommandQueue,
        wake: Arc<dyn Fn() + Send + Sync>,
        auth_deadline: Duration,
    ) -> Self {
        Self {
            auth_deadline,
            request_timeout: REQUEST_TIMEOUT,
            token: Arc::new(token),
            submit: queue.sender(),
            wake,
            authed: Arc::default(),
            pending: Arc::default(),
            next_id: Arc::default(),
        }
    }
}

/// An unauthenticated connection's place in [`Shared::pending`]; leaves it when dropped.
struct Pending<'a> {
    shared: &'a Shared,
    id: u64,
}

impl<'a> Pending<'a> {
    /// Registers `stream` (a clone, kept so it can be shut down), evicting the oldest if full.
    fn enter(shared: &'a Shared, stream: TcpStream) -> Self {
        let id = shared.next_id.fetch_add(1, Ordering::SeqCst);
        let mut q = shared.pending.lock().unwrap_or_else(|e| e.into_inner());
        while q.len() >= MAX_UNAUTHED {
            if let Some((_, old)) = q.pop_front() {
                let _ = old.shutdown(std::net::Shutdown::Both);
            }
        }
        q.push_back((id, stream));
        Self { shared, id }
    }
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        let mut q = self
            .shared
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        q.retain(|(id, _)| *id != self.id);
    }
}

fn send_line(w: &mut impl Write, v: &Value) -> io::Result<()> {
    let mut s = v.to_string();
    s.push('\n');
    w.write_all(s.as_bytes())?;
    w.flush()
}

fn mac(token: &str, role: &str, nonce: &str, cnonce: &str) -> String {
    let key = *blake3::hash(token.as_bytes()).as_bytes();
    let mut msg = Vec::new();
    for part in [role, nonce, cnonce] {
        // Length-prefixed, so ("a","bc") and ("ab","c") can never collide.
        msg.extend_from_slice(&(part.len() as u32).to_le_bytes());
        msg.extend_from_slice(part.as_bytes());
    }
    blake3::keyed_hash(&key, &msg).to_hex().to_string()
}

/// The client's `cnonce` when `first` is `{"auth":MAC,"cnonce":C}` answering `nonce` with the
/// right MAC. Only a JSON *object* can pass: an HTTP request line or a bare string never parses
/// as one.
fn authenticates(first: &str, token: &str, nonce: &str) -> Option<String> {
    let Ok(Value::Object(o)) = serde_json::from_str::<Value>(first) else {
        return None;
    };
    let cnonce = o.get("cnonce")?.as_str()?;
    if !(16..=64).contains(&cnonce.len()) || !cnonce.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let given = o.get("auth")?.as_str()?;
    ct_eq(
        given.as_bytes(),
        mac(token, "client", nonce, cnonce).as_bytes(),
    )
    .then(|| cnonce.to_owned())
}

/// Runs one connection to completion.
pub fn serve_connection(stream: TcpStream, shared: &Shared) {
    let _ = stream.set_nodelay(true);
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let (Ok(read_half), Ok(evict_handle)) = (stream.try_clone(), stream.try_clone()) else {
        return;
    };
    let pending = Pending::enter(shared, evict_handle);
    let mut writer = stream;
    let Ok(nonce) = new_token() else { return };
    if send_line(
        &mut writer,
        &json!({"hello": "nicti-control", "protocol": PROTOCOL, "nonce": nonce}),
    )
    .is_err()
    {
        return;
    }
    // The size cap is enforced per line by `read_line_limited`, not by the reader. The deadline
    // covers the whole first line, however slowly it is trickled in.
    let mut reader = BufReader::new(DeadlineRead {
        inner: read_half,
        deadline: Some(std::time::Instant::now() + shared.auth_deadline),
    });
    let first = match read_line_limited(&mut reader, MAX_LINE) {
        Ok(Some(l)) => l,
        _ => return,
    };
    let Some(cnonce) = authenticates(&first, &shared.token, &nonce) else {
        let _ = send_line(
            &mut writer,
            &json!({"ok": false, "error": "authentication failed"}),
        );
        return;
    };
    drop(pending);
    let Some(_auth_slot) = Slot::take(&shared.authed, MAX_AUTHED) else {
        let _ = send_line(
            &mut writer,
            &json!({"ok": false, "error": "too many connections"}),
        );
        return;
    };
    reader.get_mut().deadline = None;
    let _ = reader.get_ref().inner.set_read_timeout(Some(IDLE_TIMEOUT));
    let proof = mac(&shared.token, "server", &nonce, &cnonce);
    if send_line(&mut writer, &json!({"ok": true, "proof": proof})).is_err() {
        return;
    }

    loop {
        let line = match read_line_limited(&mut reader, MAX_LINE) {
            Ok(Some(l)) => l,
            Ok(None) => return,
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                let _ = send_line(
                    &mut writer,
                    &json!({"ok": false, "error": "request too large"}),
                );
                return;
            }
            // The line was consumed whole, so the stream is still in sync.
            Err(e) if e.kind() == io::ErrorKind::InvalidInput => {
                if send_line(&mut writer, &json!({"ok": false, "error": "not UTF-8"})).is_err() {
                    return;
                }
                continue;
            }
            Err(_) => return, // idle timeout, reset, or a torn line
        };
        let reply = handle_request(&line, shared);
        if send_line(&mut writer, &reply).is_err() {
            return;
        }
    }
}

/// `{"id":..,"method":"<command id | commands>","params":{..}}` -> `{"id":..,"ok":..,..}`.
fn handle_request(line: &str, shared: &Shared) -> Value {
    let req: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return json!({"ok": false, "error": format!("not JSON: {e}")}),
    };
    let Some(obj) = req.as_object() else {
        return json!({"ok": false, "error": "a request must be a JSON object"});
    };
    // The id is echoed so a client can match replies; it is never interpreted.
    let id = obj.get("id").cloned().unwrap_or(Value::Null);
    let fail = |msg: String| json!({"id": id, "ok": false, "error": msg});
    let Some(method) = obj.get("method").and_then(Value::as_str) else {
        return fail("`method` must be a string".into());
    };
    let params = obj.get("params").cloned().unwrap_or(Value::Null);
    let (tx, rx) = mpsc::channel();
    let cancelled = Arc::new(AtomicBool::new(false));
    if shared
        .submit
        .send(Request {
            id: method.to_owned(),
            params,
            reply: tx,
            cancelled: Arc::clone(&cancelled),
        })
        .is_err()
    {
        return fail("the app is shutting down".into());
    }
    (shared.wake)();
    match rx.recv_timeout(shared.request_timeout) {
        Ok(Ok(result)) => json!({"id": id, "ok": true, "result": result}),
        Ok(Err(e)) => fail(e),
        Err(_) => {
            // The client is told it failed, so the app must not run it later: a retry would then
            // apply a non-idempotent command twice.
            cancelled.store(true, Ordering::SeqCst);
            fail("timed out waiting for the app".into())
        }
    }
}

/// A running channel. Dropping it stops accepting and removes the token file.
pub struct Control {
    token_file: TokenFile,
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
}

impl Control {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn token_path(&self) -> &Path {
        self.token_file.path()
    }
}

impl Drop for Control {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the blocked `accept` so the thread sees the flag.
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_millis(200));
    }
}

/// Binds `127.0.0.1:port`, writes the token file (port included) into `dir` and starts accepting.
pub fn start(
    opts: ControlOptions,
    dir: &Path,
    queue: &CommandQueue,
    wake: Arc<dyn Fn() + Send + Sync>,
) -> Result<Control, String> {
    start_with(opts, dir, queue, wake, AUTH_DEADLINE, REQUEST_TIMEOUT)
}

fn start_with(
    opts: ControlOptions,
    dir: &Path,
    queue: &CommandQueue,
    wake: Arc<dyn Fn() + Send + Sync>,
    auth_deadline: Duration,
    request_timeout: Duration,
) -> Result<Control, String> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, opts.port))
        .map_err(|e| format!("can't listen on 127.0.0.1:{}: {e}", opts.port))?;
    let addr = listener
        .local_addr()
        .map_err(|e| format!("can't read the listening address: {e}"))?;
    let token = new_token()?;
    let token_file = TokenFile::create(dir, addr.port(), &token)?;
    let mut shared = Shared::new(token, queue, wake, auth_deadline);
    shared.request_timeout = request_timeout;
    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = Arc::clone(&stop);
    thread::Builder::new()
        .name("nicti-control".into())
        .spawn(move || {
            for stream in listener.incoming() {
                if stop_flag.load(Ordering::SeqCst) {
                    return;
                }
                let Ok(stream) = stream else {
                    // Out of file descriptors etc.: don't spin on it.
                    thread::sleep(Duration::from_millis(50));
                    continue;
                };
                // Belt and braces: the socket is bound to loopback, but never serve anything else.
                if !stream.peer_addr().is_ok_and(|a| a.ip().is_loopback()) {
                    continue;
                }
                let shared = shared.clone();
                let _ = thread::Builder::new()
                    .name("nicti-control-conn".into())
                    .stack_size(256 * 1024)
                    .spawn(move || serve_connection(stream, &shared));
            }
        })
        .map_err(|e| format!("can't start the control thread: {e}"))?;
    Ok(Control {
        token_file,
        addr,
        stop,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{CommandHost, Policy};
    use std::io::Read;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn options_are_opt_in_strict_and_parse_the_port() {
        let p = |a: &[&str], e: Option<&str>| ControlOptions::parse(args(a), e);
        assert_eq!(p(&[], None), Ok(None));
        assert_eq!(p(&["--other"], None), Ok(None));
        assert_eq!(
            p(&["--control"], None),
            Ok(Some(ControlOptions {
                port: 0,
                allow_destructive: false
            }))
        );
        assert_eq!(p(&["--control=4242"], None).unwrap().unwrap().port, 4242);
        assert_eq!(p(&[], Some("5000")).unwrap().unwrap().port, 5000);
        assert_eq!(p(&[], Some("")), Ok(None), "an empty env var is off");
        assert!(
            p(&["--control", "--control-allow-destructive"], None)
                .unwrap()
                .unwrap()
                .allow_destructive
        );
        for bad in [
            &["--control=70000"][..],
            &["--control=x"],
            &["--control", "--control-allow-destructve"],
            &["--control-allow-destructive"],
        ] {
            assert!(p(bad, None).is_err(), "{bad:?}");
        }
        assert!(p(&[], Some("nope")).is_err());
    }

    #[test]
    fn constant_time_compare_and_tokens() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"abcd"));
        let (a, b) = (new_token().unwrap(), new_token().unwrap());
        assert_eq!(a.len(), 32);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "two tokens collided (the RNG is broken)");
    }

    #[test]
    fn lines_are_capped_without_buffering_the_excess() {
        let mut r = BufReader::new(&b"hello\nworld"[..]);
        assert_eq!(
            read_line_limited(&mut r, 10).unwrap().as_deref(),
            Some("hello")
        );
        // "world" never gets its newline: a torn line is an error, not a request.
        assert!(read_line_limited(&mut r, 10).is_err());
        let big = [b'a'; 100];
        let mut r = BufReader::with_capacity(8, &big[..]);
        assert_eq!(
            read_line_limited(&mut r, 50).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        let mut line = vec![b'a'; 50];
        line.push(b'\n');
        let mut r = BufReader::with_capacity(8, &line[..]);
        assert_eq!(
            read_line_limited(&mut r, 50).unwrap().unwrap().len(),
            50,
            "exactly the cap"
        );
        let mut r = BufReader::new(&b"x\r\n"[..]);
        assert_eq!(read_line_limited(&mut r, 5).unwrap().as_deref(), Some("x"));
        let mut r = BufReader::new(&b""[..]);
        assert_eq!(read_line_limited(&mut r, 5).unwrap(), None);
    }

    #[test]
    fn only_a_json_object_with_the_right_mac_authenticates_and_the_token_never_does() {
        // Built, not spelled out: a long hex literal in source trips the repo's secret scanner.
        let t = "ab".repeat(16);
        let t = t.as_str();
        let nonce = "cd".repeat(16);
        let cn = "ef".repeat(16);
        let line = |auth: &str, cnonce: &str| json!({"auth": auth, "cnonce": cnonce}).to_string();
        let good = mac(t, "client", &nonce, &cn);
        assert_eq!(
            authenticates(&line(&good, &cn), t, &nonce),
            Some(cn.clone())
        );
        // The token itself is not an answer, whatever it is dressed as.
        assert_eq!(authenticates(&line(t, &cn), t, &nonce), None);
        // One character off (same length), a replayed answer to another nonce, the server's proof
        // reflected back as the client's answer, another token's answer: all refused.
        let mut near = good.clone();
        near.replace_range(..1, if good.starts_with('0') { "1" } else { "0" });
        assert_eq!(authenticates(&line(&near, &cn), t, &nonce), None);
        let other_nonce = "12".repeat(16);
        assert_eq!(authenticates(&line(&good, &cn), t, &other_nonce), None);
        let reflected = mac(t, "server", &nonce, &cn);
        assert_eq!(authenticates(&line(&reflected, &cn), t, &nonce), None);
        let other_token = "99".repeat(16);
        let theirs = mac(&other_token, "client", &nonce, &cn);
        assert_eq!(authenticates(&line(&theirs, &cn), t, &nonce), None);
        // The client nonce is part of what is signed, and must look like one.
        assert_eq!(
            authenticates(&line(&good, &"ee".repeat(16)), t, &nonce),
            None
        );
        let short = "ab".repeat(4);
        let short_mac = mac(t, "client", &nonce, &short);
        assert_eq!(authenticates(&line(&short_mac, &short), t, &nonce), None);
        let nonhex = "zz".repeat(16);
        let nonhex_mac = mac(t, "client", &nonce, &nonhex);
        assert_eq!(authenticates(&line(&nonhex_mac, &nonhex), t, &nonce), None);
        for bad in [
            "GET / HTTP/1.1".to_owned(),
            format!(r#""{good}""#),
            r#"["auth"]"#.to_owned(),
            r#"{"auth":"nope","cnonce":"x"}"#.to_owned(),
            json!({"auth": good}).to_string(),
            json!({"cnonce": cn}).to_string(),
            json!({"auth": 123, "cnonce": cn}).to_string(),
            String::new(),
        ] {
            assert_eq!(authenticates(&bad, t, &nonce), None, "{bad}");
        }
        // Length-prefixing: moving a byte between the nonces changes the MAC.
        assert_ne!(mac(t, "client", "aab", "c"), mac(t, "client", "aa", "bc"));
    }

    #[cfg(unix)]
    #[test]
    fn the_token_file_is_private_exclusive_and_removed_on_drop() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile_dir("tok");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let file = TokenFile::create(&dir, 4321, "tok").unwrap();
        let path = file.path().to_path_buf();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "created private, not chmodded afterwards");
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            (v["port"].as_u64(), v["token"].as_str()),
            (Some(4321), Some("tok"))
        );
        // A stale file (a crash left it) is replaced, not trusted.
        std::mem::forget(file);
        let again = TokenFile::create(&dir, 1, "new").unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["token"], "new");
        drop(again);
        assert!(!path.exists(), "removed on exit");
        // A symlink planted at the path is replaced, and its target is left alone.
        let victim = dir.join("victim");
        std::fs::write(&victim, "keep").unwrap();
        std::os::unix::fs::symlink(&victim, &path).unwrap();
        let f = TokenFile::create(&dir, 2, "x").unwrap();
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep");
        drop(f);
        // A runtime dir other users can enter is refused.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(TokenFile::create(&dir, 3, "y")
            .unwrap_err()
            .contains("other users"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn tempfile_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nicti-control-test-{tag}-{}-{}",
            std::process::id(),
            new_token().unwrap()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// How many times a client reached `grid.select` (a visible side effect).
    static SELECTS: AtomicUsize = AtomicUsize::new(0);

    /// A host that answers `ui.inspect` and records `grid.select`.
    #[derive(Default)]
    struct Echo;
    impl CommandHost for Echo {
        fn inspect(&self) -> Value {
            json!({"view": "library"})
        }
        fn view_name(&self) -> &'static str {
            "library"
        }
        fn set_view(&mut self, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn select_asset(&mut self, _: i64) -> Result<(), String> {
            SELECTS.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn known_asset(&self, _: i64) -> bool {
            true
        }
        fn target_ids(&self) -> Vec<i64> {
            Vec::new()
        }
        fn rate(&mut self, _: &[i64], _: Option<i64>) -> Result<(), String> {
            Ok(())
        }
        fn develop_ready(&self) -> Result<(), String> {
            Err("none".into())
        }
        fn develop_get(&self, _: &str) -> Result<Value, String> {
            Err("none".into())
        }
        fn develop_set(&mut self, _: &str, _: &Value) -> Result<Value, String> {
            Err("none".into())
        }
        fn export_open(&mut self) -> Result<(), String> {
            Ok(())
        }
    }

    /// A running channel plus a thread standing in for the UI thread that drains its queue.
    struct Rig {
        control: Control,
        token: String,
        stop: Arc<AtomicBool>,
        ui: Option<thread::JoinHandle<()>>,
        dir: PathBuf,
    }

    impl Rig {
        fn new() -> Self {
            let dir = tempfile_dir("rig");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            }
            let queue = Arc::new(CommandQueue::default());
            let control = start(
                ControlOptions {
                    port: 0,
                    allow_destructive: false,
                },
                &dir,
                &queue,
                Arc::new(|| {}),
            )
            .unwrap();
            let v: Value =
                serde_json::from_str(&std::fs::read_to_string(control.token_path()).unwrap())
                    .unwrap();
            let token = v["token"].as_str().unwrap().to_owned();
            let stop = Arc::new(AtomicBool::new(false));
            let (q, s) = (Arc::clone(&queue), Arc::clone(&stop));
            let ui = thread::spawn(move || {
                let mut host = Echo;
                while !s.load(Ordering::SeqCst) {
                    q.drain(&mut host, Policy::default());
                    thread::sleep(Duration::from_millis(2));
                }
            });
            Self {
                control,
                token,
                stop,
                ui: Some(ui),
                dir,
            }
        }

        fn connect(&self) -> TcpStream {
            let s = TcpStream::connect(self.control.addr()).unwrap();
            s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            s
        }

        fn authed(&self) -> (TcpStream, BufReader<TcpStream>) {
            handshake(self.control.addr(), &self.token).expect("handshake")
        }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(h) = self.ui.take() {
                let _ = h.join();
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// Reads the server's hello and returns its nonce.
    fn read_hello(r: &mut BufReader<TcpStream>) -> String {
        let hello: Value =
            serde_json::from_str(&read_line_limited(r, 4096).unwrap().unwrap()).unwrap();
        assert_eq!(hello["hello"], "nicti-control");
        assert_eq!(hello["protocol"], PROTOCOL);
        hello["nonce"].as_str().unwrap().to_owned()
    }

    fn answer(token: &str, nonce: &str, cnonce: &str) -> String {
        json!({"auth": mac(token, "client", nonce, cnonce), "cnonce": cnonce}).to_string()
    }

    /// The client side of the handshake, including checking the server's proof.
    fn handshake(
        addr: SocketAddr,
        token: &str,
    ) -> Result<(TcpStream, BufReader<TcpStream>), String> {
        let mut s = TcpStream::connect(addr).map_err(|e| e.to_string())?;
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut r = BufReader::new(s.try_clone().unwrap());
        let nonce = read_hello(&mut r);
        let cnonce = new_token().unwrap();
        writeln!(s, "{}", answer(token, &nonce, &cnonce)).map_err(|e| e.to_string())?;
        let reply: Value = serde_json::from_str(
            &read_line_limited(&mut r, 4096)
                .map_err(|e| e.to_string())?
                .ok_or("closed")?,
        )
        .map_err(|e| e.to_string())?;
        if reply["ok"] != true {
            return Err(reply.to_string());
        }
        if reply["proof"] != mac(token, "server", &nonce, &cnonce) {
            return Err("the server's proof is wrong".into());
        }
        Ok((s, r))
    }

    fn ask(s: &mut TcpStream, r: &mut BufReader<TcpStream>, req: &str) -> Value {
        writeln!(s, "{req}").unwrap();
        serde_json::from_str(&read_line_limited(r, MAX_LINE).unwrap().unwrap()).unwrap()
    }

    #[test]
    fn an_authenticated_client_runs_commands_and_gets_ids_errors_and_the_listing_back() {
        let rig = Rig::new();
        assert!(rig.control.addr().ip().is_loopback());
        let (mut s, mut r) = rig.authed();
        let out = ask(&mut s, &mut r, r#"{"id":7,"method":"ui.inspect"}"#);
        assert_eq!(out, json!({"id":7,"ok":true,"result":{"view":"library"}}));
        let out = ask(
            &mut s,
            &mut r,
            r#"{"id":"a","method":"nope.nope","params":{}}"#,
        );
        assert_eq!(
            (out["ok"].clone(), out["id"].clone()),
            (json!(false), json!("a"))
        );
        assert!(out["error"].as_str().unwrap().contains("unknown command"));
        let out = ask(&mut s, &mut r, r#"{"id":1,"method":"commands"}"#);
        assert!(out["result"].as_array().is_some_and(|a| !a.is_empty()));
        // Garbage lines are answered, not fatal.
        assert_eq!(ask(&mut s, &mut r, "not json")["ok"], false);
        assert_eq!(ask(&mut s, &mut r, "[1]")["ok"], false);
        assert_eq!(ask(&mut s, &mut r, r#"{"method":5}"#)["ok"], false);
        assert_eq!(
            ask(&mut s, &mut r, r#"{"id":2,"method":"ui.inspect"}"#)["ok"],
            true
        );
    }

    #[test]
    fn a_wrong_answer_a_non_json_first_line_and_an_http_request_are_closed_without_running_anything(
    ) {
        let rig = Rig::new();
        let before = SELECTS.load(Ordering::SeqCst);
        let nonce_of = |s: &mut TcpStream| {
            let mut r = BufReader::new(s.try_clone().unwrap());
            read_hello(&mut r)
        };
        let cn = new_token().unwrap();
        for variant in 0..6 {
            let mut s = rig.connect();
            let nonce = nonce_of(&mut s);
            let line = match variant {
                0 => answer(&"00".repeat(16), &nonce, &cn), // another token's answer
                1 => answer(&rig.token, &"11".repeat(16), &cn), // an answer to another nonce
                2 => json!({"auth": rig.token, "cnonce": cn}).to_string(), // the token itself
                3 => r#"{"method":"grid.select","params":{"asset":1}}"#.to_owned(),
                4 => "POST / HTTP/1.1".to_owned(),
                _ => format!(r#"["{}"]"#, rig.token),
            };
            writeln!(s, "{line}").unwrap();
            let mut all = String::new();
            let _ = s.read_to_string(&mut all); // the server closes the connection
            assert!(!all.contains("proof"), "variant {variant} got in: {all}");
            assert!(
                !all.contains("\"result\""),
                "variant {variant} ran a command: {all}"
            );
        }
        assert_eq!(
            SELECTS.load(Ordering::SeqCst),
            before,
            "a refused client ran a command"
        );
        // A client that never answers is dropped at the deadline.
        let mut s = rig.connect();
        let mut r = BufReader::new(s.try_clone().unwrap());
        read_hello(&mut r);
        let started = std::time::Instant::now();
        let mut buf = [0u8; 8];
        assert_eq!(s.read(&mut buf).unwrap_or(0), 0);
        assert!(started.elapsed() < AUTH_DEADLINE + Duration::from_secs(3));
    }

    #[test]
    fn a_server_that_does_not_hold_the_token_cannot_pass_the_clients_check() {
        // A process that grabbed the port first sees only the nonce and the client's MAC; it can
        // not compute the server proof without the token, so the client rejects it.
        let rig = Rig::new();
        let imposter = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = imposter.local_addr().unwrap();
        let t = thread::spawn(move || {
            let (mut s, _) = imposter.accept().unwrap();
            writeln!(
                s,
                r#"{{"hello":"nicti-control","protocol":{PROTOCOL},"nonce":"{}"}}"#,
                "ab".repeat(16)
            )
            .unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            let seen = read_line_limited(&mut r, 4096).unwrap().unwrap();
            // The best it can do: claim success with a made-up proof.
            writeln!(s, r#"{{"ok":true,"proof":"{}"}}"#, "00".repeat(32)).unwrap();
            seen
        });
        let err = handshake(addr, &rig.token).unwrap_err();
        assert!(err.contains("proof"), "{err}");
        let seen = t.join().unwrap();
        assert!(
            !seen.contains(&rig.token),
            "the token crossed the wire: {seen}"
        );
    }

    #[test]
    fn the_connection_caps_hold_and_a_closed_connection_frees_its_slot() {
        let rig = Rig::new();
        let mut held: Vec<_> = (0..MAX_AUTHED).map(|_| rig.authed()).collect();
        // A fifth authenticated client is told no (after it proves it knows the token).
        let mut s = rig.connect();
        let mut r = BufReader::new(s.try_clone().unwrap());
        let nonce = read_hello(&mut r);
        writeln!(s, "{}", answer(&rig.token, &nonce, &new_token().unwrap())).unwrap();
        let mut all = String::new();
        let _ = s.read_to_string(&mut all);
        assert!(all.contains("too many"), "{all}");
        // Freeing one makes room again.
        drop(held.pop());
        let mut ok = false;
        for _ in 0..50 {
            if handshake(rig.control.addr(), &rig.token).is_ok() {
                ok = true;
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(ok, "the slot was not released");
    }

    #[test]
    fn silent_connections_cannot_lock_the_real_client_out_the_oldest_is_evicted() {
        let rig = Rig::new();
        // Fill every unauthenticated slot with sockets that say nothing. Reading the hello proves
        // the server registered each one.
        let mut idle: Vec<_> = (0..MAX_UNAUTHED)
            .map(|_| {
                let s = rig.connect();
                let mut r = BufReader::new(s.try_clone().unwrap());
                read_hello(&mut r);
                (s, r)
            })
            .collect();
        // The real client still gets in: it evicts the oldest silent one.
        assert!(handshake(rig.control.addr(), &rig.token).is_ok());
        let (s, _) = &mut idle[0];
        let mut buf = [0u8; 8];
        // Well inside the 5 s auth deadline, which would close it anyway.
        s.set_read_timeout(Some(Duration::from_millis(1500)))
            .unwrap();
        let n = s
            .read(&mut buf)
            .expect("the oldest silent socket was not closed (the read timed out)");
        assert_eq!(n, 0, "the oldest silent socket was sent data");
    }

    #[test]
    fn a_client_trickling_its_answer_is_cut_off_at_the_total_deadline() {
        let dir = tempfile_dir("slow");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let queue = CommandQueue::default();
        let control = start_with(
            ControlOptions {
                port: 0,
                allow_destructive: false,
            },
            &dir,
            &queue,
            Arc::new(|| {}),
            Duration::from_millis(600),
            REQUEST_TIMEOUT,
        )
        .unwrap();
        let s = TcpStream::connect(control.addr()).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut r = BufReader::new(s.try_clone().unwrap());
        read_hello(&mut r);
        let mut w = s.try_clone().unwrap();
        // One byte every 200 ms and never a newline: each read is well inside any per-read
        // timeout, so only a total deadline stops it.
        let feeder = thread::spawn(move || {
            for _ in 0..40 {
                if w.write_all(b" ").is_err() {
                    return;
                }
                thread::sleep(Duration::from_millis(200));
            }
        });
        let started = std::time::Instant::now();
        let mut rest = String::new();
        let mut s = s;
        let _ = s.read_to_string(&mut rest);
        assert!(!rest.contains("proof"), "{rest}");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        drop(s);
        let _ = feeder.join();
        drop(control);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_request_the_client_was_told_timed_out_never_runs_afterwards() {
        let dir = tempfile_dir("cancel");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let queue = CommandQueue::default();
        let control = start_with(
            ControlOptions {
                port: 0,
                allow_destructive: false,
            },
            &dir,
            &queue,
            Arc::new(|| {}),
            AUTH_DEADLINE,
            Duration::from_millis(300),
        )
        .unwrap();
        let token = {
            let v: Value =
                serde_json::from_str(&std::fs::read_to_string(control.token_path()).unwrap())
                    .unwrap();
            v["token"].as_str().unwrap().to_owned()
        };
        let (mut s, mut r) = handshake(control.addr(), &token).unwrap();
        let before = SELECTS.load(Ordering::SeqCst);
        // Nothing drains the queue, so the request times out.
        let out = ask(
            &mut s,
            &mut r,
            r#"{"id":1,"method":"grid.select","params":{"asset":5}}"#,
        );
        assert_eq!(out["ok"], false);
        assert!(out["error"].as_str().unwrap().contains("timed out"));
        // The UI thread comes back later: the abandoned request must not run.
        let mut host = Echo;
        queue.drain(&mut host, Policy::default());
        assert_eq!(
            SELECTS.load(Ordering::SeqCst),
            before,
            "an abandoned request ran"
        );
        drop(s);
        drop(control);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_policy_flag_reaches_the_command_gate_and_a_bad_line_does_not_end_the_session() {
        let on = ControlOptions {
            port: 0,
            allow_destructive: true,
        };
        let off = ControlOptions {
            allow_destructive: false,
            ..on
        };
        assert!(on.policy().allow_destructive && !off.policy().allow_destructive);
        let rig = Rig::new();
        let (mut s, mut r) = rig.authed();
        // A non-UTF-8 line is answered, and the session carries on.
        s.write_all(&[0xff, 0xfe, b'\n']).unwrap();
        let out: Value =
            serde_json::from_str(&read_line_limited(&mut r, 4096).unwrap().unwrap()).unwrap();
        assert!(out["error"].as_str().unwrap().contains("UTF-8"), "{out}");
        assert_eq!(
            ask(&mut s, &mut r, r#"{"id":2,"method":"ui.inspect"}"#)["ok"],
            true
        );
    }

    #[test]
    fn an_oversized_line_is_refused_and_closes_the_connection() {
        let rig = Rig::new();
        let (mut s, mut r) = rig.authed();
        let huge = format!(r#"{{"id":1,"method":"{}"}}"#, "a".repeat(MAX_LINE + 10));
        // The server may reset while we are still writing; either way nothing runs.
        let _ = writeln!(s, "{huge}");
        let reply = read_line_limited(&mut r, 4096).ok().flatten();
        if let Some(reply) = reply {
            assert!(reply.contains("too large"), "{reply}");
        }
        let (mut s2, mut r2) = rig.authed();
        assert_eq!(
            ask(&mut s2, &mut r2, r#"{"id":3,"method":"ui.inspect"}"#)["ok"],
            true
        );
    }

    #[test]
    fn dropping_the_channel_removes_the_token_file_and_stops_listening() {
        let dir = tempfile_dir("drop");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let queue = CommandQueue::default();
        let control = start(
            ControlOptions {
                port: 0,
                allow_destructive: false,
            },
            &dir,
            &queue,
            Arc::new(|| {}),
        )
        .unwrap();
        let (path, addr) = (control.token_path().to_path_buf(), control.addr());
        assert!(path.exists());
        drop(control);
        assert!(!path.exists(), "the token file outlives the channel");
        thread::sleep(Duration::from_millis(100));
        assert!(
            TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_err(),
            "still listening"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
