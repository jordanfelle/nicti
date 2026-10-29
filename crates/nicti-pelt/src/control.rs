//! The opt-in control channel (#543, ADR-0434 slice 2a): a tokened JSON-lines socket on `127.0.0.1`
//! that submits requests to the command table ([`crate::cmd`]) and returns their replies.
//!
//! **Off unless asked for** (`--control[=PORT]` or `NICTI_CONTROL_PORT`). Loopback only, never
//! `0.0.0.0`. Every connection must send `{"auth":"<token>"}` as its first line within 5 s; the
//! token is 128 random bits from the OS, compared in constant time, and lives in a file created
//! `O_EXCL` with mode `0600` (the bound port is written *into* that file, so a predictable port
//! can't be pre-bound by another local user to harvest the token). A non-object first line, a wrong
//! token or a late one closes the connection, which also stops a web page's cross-origin `fetch`
//! from smuggling a request in an HTTP body.
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
pub const PROTOCOL: u32 = 1;

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
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "not UTF-8"))
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
    unauthed: Arc<AtomicUsize>,
    /// [`AUTH_DEADLINE`] outside tests.
    auth_deadline: Duration,
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
            token: Arc::new(token),
            submit: queue.sender(),
            wake,
            authed: Arc::default(),
            unauthed: Arc::default(),
        }
    }
}

fn send_line(w: &mut impl Write, v: &Value) -> io::Result<()> {
    let mut s = v.to_string();
    s.push('\n');
    w.write_all(s.as_bytes())?;
    w.flush()
}

/// Whether `first` is `{"auth":"<token>"}` with the right token. Only a JSON *object* with a
/// string `auth` can pass: an HTTP request line or a bare string never parses as one.
fn authenticates(first: &str, token: &str) -> bool {
    match serde_json::from_str::<Value>(first) {
        Ok(Value::Object(o)) => o
            .get("auth")
            .and_then(Value::as_str)
            .is_some_and(|given| ct_eq(given.as_bytes(), token.as_bytes())),
        _ => false,
    }
}

/// Runs one connection to completion.
pub fn serve_connection(stream: TcpStream, shared: &Shared) {
    let Some(unauth_slot) = Slot::take(&shared.unauthed, MAX_UNAUTHED) else {
        return; // too many half-open connections; drop this one without a word
    };
    let _ = stream.set_nodelay(true);
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let Ok(read_half) = stream.try_clone() else {
        return;
    };
    let mut writer = stream;
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
    if !authenticates(&first, &shared.token) {
        let _ = send_line(
            &mut writer,
            &json!({"ok": false, "error": "authentication failed"}),
        );
        return;
    }
    drop(unauth_slot);
    let Some(_auth_slot) = Slot::take(&shared.authed, MAX_AUTHED) else {
        let _ = send_line(
            &mut writer,
            &json!({"ok": false, "error": "too many connections"}),
        );
        return;
    };
    reader.get_mut().deadline = None;
    let _ = reader.get_ref().inner.set_read_timeout(Some(IDLE_TIMEOUT));
    if send_line(
        &mut writer,
        &json!({"ok": true, "hello": "nicti-control", "protocol": PROTOCOL}),
    )
    .is_err()
    {
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
    if shared
        .submit
        .send(Request {
            id: method.to_owned(),
            params,
            reply: tx,
        })
        .is_err()
    {
        return fail("the app is shutting down".into());
    }
    (shared.wake)();
    match rx.recv_timeout(REQUEST_TIMEOUT) {
        Ok(Ok(result)) => json!({"id": id, "ok": true, "result": result}),
        Ok(Err(e)) => fail(e),
        Err(_) => fail("timed out waiting for the app".into()),
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
    start_with(opts, dir, queue, wake, AUTH_DEADLINE)
}

fn start_with(
    opts: ControlOptions,
    dir: &Path,
    queue: &CommandQueue,
    wake: Arc<dyn Fn() + Send + Sync>,
    auth_deadline: Duration,
) -> Result<Control, String> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, opts.port))
        .map_err(|e| format!("can't listen on 127.0.0.1:{}: {e}", opts.port))?;
    let addr = listener
        .local_addr()
        .map_err(|e| format!("can't read the listening address: {e}"))?;
    let token = new_token()?;
    let token_file = TokenFile::create(dir, addr.port(), &token)?;
    let shared = Shared::new(token, queue, wake, auth_deadline);
    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = Arc::clone(&stop);
    thread::Builder::new()
        .name("nicti-control".into())
        .spawn(move || {
            for stream in listener.incoming() {
                if stop_flag.load(Ordering::SeqCst) {
                    return;
                }
                let Ok(stream) = stream else { continue };
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
    fn only_a_json_object_with_the_right_token_authenticates() {
        // Built, not spelled out: a 32-hex literal in source trips the repo's secret scanner.
        let t = "ab".repeat(16);
        let t = t.as_str();
        assert!(authenticates(&format!(r#"{{"auth":"{t}"}}"#), t));
        // Same length, one character off: the length check alone must not let it through.
        let near = format!("{}ac", "ab".repeat(15));
        assert!(!authenticates(&format!(r#"{{"auth":"{near}"}}"#), t));
        for bad in [
            "GET / HTTP/1.1".to_owned(),
            format!(r#""{t}""#),
            r#"["auth"]"#.to_owned(),
            r#"{"auth":"nope"}"#.to_owned(),
            r#"{"auth":""}"#.to_owned(),
            r#"{"auth":123}"#.to_owned(),
            format!(r#"{{"token":"{t}"}}"#),
            String::new(),
        ] {
            assert!(!authenticates(&bad, t), "{bad}");
        }
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

    /// A host that answers `ui.inspect` and nothing else.
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
            let mut s = self.connect();
            writeln!(s, r#"{{"auth":"{}"}}"#, self.token).unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            let hello = read_line_limited(&mut r, 4096).unwrap().unwrap();
            assert!(hello.contains("nicti-control"), "{hello}");
            (s, r)
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
    fn a_wrong_token_a_non_json_first_line_and_an_http_request_are_all_closed_without_running_anything(
    ) {
        let rig = Rig::new();
        for first in [
            r#"{"auth":"wrong"}"#.to_owned(),
            r#"{"method":"ui.inspect"}"#.to_owned(),
            "POST / HTTP/1.1".to_owned(),
            format!(r#"["{}"]"#, rig.token),
        ] {
            let mut s = rig.connect();
            writeln!(s, "{first}").unwrap();
            let mut all = String::new();
            let _ = s.read_to_string(&mut all); // the server closes the connection
            assert!(!all.contains("hello"), "{first} got in: {all}");
            assert!(!all.contains("view"), "{first} ran a command: {all}");
        }
        // A client that never sends a token is dropped at the deadline.
        let mut s = rig.connect();
        let started = std::time::Instant::now();
        let mut buf = [0u8; 8];
        assert_eq!(s.read(&mut buf).unwrap_or(0), 0);
        assert!(started.elapsed() < AUTH_DEADLINE + Duration::from_secs(3));
    }

    #[test]
    fn the_connection_caps_hold_and_a_closed_connection_frees_its_slot() {
        let rig = Rig::new();
        let mut held: Vec<_> = (0..MAX_AUTHED).map(|_| rig.authed()).collect();
        // A fifth authenticated client is told no.
        let mut s = rig.connect();
        writeln!(s, r#"{{"auth":"{}"}}"#, rig.token).unwrap();
        let mut all = String::new();
        let _ = s.read_to_string(&mut all);
        assert!(all.contains("too many"), "{all}");
        // Freeing one makes room again.
        drop(held.pop());
        let mut ok = false;
        for _ in 0..50 {
            let mut s = rig.connect();
            writeln!(s, r#"{{"auth":"{}"}}"#, rig.token).unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            if read_line_limited(&mut r, 4096)
                .ok()
                .flatten()
                .is_some_and(|l| l.contains("hello"))
            {
                ok = true;
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(ok, "the slot was not released");
    }

    #[test]
    fn silent_unauthenticated_connections_are_capped_so_they_cannot_exhaust_the_server() {
        let rig = Rig::new();
        let idle: Vec<_> = (0..MAX_UNAUTHED).map(|_| rig.connect()).collect();
        thread::sleep(Duration::from_millis(300)); // let the server threads take their slots
        let mut s = rig.connect();
        let _ = writeln!(s, r#"{{"auth":"{}"}}"#, rig.token);
        let mut all = String::new();
        let _ = s.read_to_string(&mut all);
        assert!(
            all.is_empty(),
            "a connection over the cap was served: {all}"
        );
        drop(idle);
    }

    #[test]
    fn a_client_trickling_its_first_line_is_cut_off_at_the_total_deadline() {
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
        )
        .unwrap();
        let mut s = TcpStream::connect(control.addr()).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
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
        let mut buf = [0u8; 8];
        let n = s.read(&mut buf).unwrap_or(0);
        assert_eq!(n, 0, "the connection should be closed, not answered");
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
