//! `nicti-mcp`: an MCP server on stdio that drives a running nicti app (started with `--control`).
//!
//! ```text
//! nicti-mcp [--token-file PATH] [--port N]
//! ```
//! The token file defaults to the only `nicti-control-*.json` in the per-user runtime directory
//! (`NICTI_CONTROL_TOKEN_FILE` also works). Over `ssh -L` copy the file locally and pass `--port`
//! with the local end of the forward. See `docs/agent-control.md`.

use std::io::{BufRead, Write};
use std::path::PathBuf;

use nicti_vibrissa::client::{Session, Source};
use nicti_vibrissa::mcp::Server;
use nicti_vibrissa::wire::{default_token_dir, find_token_file};

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
    let token_file = match token_file {
        Some(p) => p,
        None => {
            let dir = default_token_dir()
                .ok_or("no XDG_RUNTIME_DIR / LOCALAPPDATA: pass --token-file")?;
            find_token_file(&dir)?
        }
    };
    Ok(Source { token_file, port })
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
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
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
