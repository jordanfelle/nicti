//! `nicti-mcp`: an MCP server on stdio that drives a running nicti app (started with `--control`).
//!
//! ```text
//! nicti-mcp [--token-file PATH] [--port N]
//! ```
//! The token file defaults to the only `nicti-control-*.json` in the per-user runtime directory
//! (`NICTI_CONTROL_TOKEN_FILE` also works). Over `ssh -L` copy the file locally and pass `--port`
//! with the local end of the forward. See `docs/agent-control.md`.

use std::io::Write;
use std::path::PathBuf;

use nicti_vibrissa::client::{Session, Source, TokenLocation};
use nicti_vibrissa::mcp::Server;
use nicti_vibrissa::wire::{default_token_dir, read_line_limited};

fn parse_args() -> Result<Source, String> {
    let mut token_file: Option<PathBuf> =
        std::env::var_os("NICTI_CONTROL_TOKEN_FILE").map(PathBuf::from);
    let mut port = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--token-file" => {
                token_file = Some(args.next().ok_or("--token-file needs a path")?.into())
            }
            "--port" => {
                let p = args.next().ok_or("--port needs a number")?;
                port = Some(
                    p.parse::<u16>()
                        .ok()
                        .filter(|p| *p != 0)
                        .ok_or("--port must be 1-65535")?,
                );
            }
            "-h" | "--help" => return Err("usage: nicti-mcp [--token-file PATH] [--port N]".into()),
            other => return Err(format!("unknown argument `{other}` (try --help)")),
        }
    }
    // No explicit file: look the one file up again on every (re)connect, so a restarted app (new
    // pid, new file name) is found without restarting this process.
    let token = match token_file {
        Some(p) => TokenLocation::File(p),
        None => TokenLocation::Discover(
            default_token_dir().ok_or("no XDG_RUNTIME_DIR / LOCALAPPDATA: pass --token-file")?,
        ),
    };
    Ok(Source { token, port })
}

fn main() {
    let source = match parse_args() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("nicti-mcp: {e}");
            std::process::exit(2);
        }
    };
    let mut server = Server::new(Session::new(source));
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut out = std::io::stdout().lock();
    loop {
        let line = match read_line_limited(&mut input, MAX_STDIN_LINE) {
            Ok(Some(l)) => l,
            Ok(None) => break, // the client closed stdin
            Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => {
                // Not UTF-8: tell the client, keep serving.
                let reply = r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"input was not UTF-8"}}"#;
                if writeln!(out, "{reply}").and_then(|()| out.flush()).is_err() {
                    break;
                }
                continue;
            }
            Err(e) => {
                eprintln!("nicti-mcp: stopping: {e}");
                std::process::exit(1);
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        if let Some(reply) = server.handle(&line) {
            if writeln!(out, "{reply}").and_then(|()| out.flush()).is_err() {
                break; // the client went away
            }
        }
    }
}

/// Longest line read from the MCP client (a tool call is tiny; this is only a memory bound).
const MAX_STDIN_LINE: usize = 16 << 20;
