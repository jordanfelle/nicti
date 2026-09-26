use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use homing::{fingerprint, mount_events, path, relink, schema, volume};
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(
    name = "homing",
    about = "Throwaway spike for #71: volume identity + drive-remapping research."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Enumerate every mounted volume and print its identity candidates as JSON -- the raw data
    /// ADR-0020's survival table is built from.
    Enumerate,
    /// Watch for volume attach/detach events for `seconds`, comparing the polling backend at
    /// `interval` against the (currently unimplemented) push backend.
    Watch {
        #[arg(long, default_value_t = 30)]
        seconds: u64,
        #[arg(long, value_enum, default_value_t = WatchBackend::Poll)]
        backend: WatchBackend,
        #[arg(long, default_value_t = 1)]
        interval_secs: u64,
    },
    /// Build a fresh catalog (SQLite file at `db`) from `root_dir`, registering it under
    /// whichever mounted volume currently owns it.
    Build {
        root_dir: PathBuf,
        #[arg(long, default_value = "homing.sqlite3")]
        db: PathBuf,
        #[arg(long, default_value = "")]
        root_rel_path: String,
        #[arg(long, value_enum, default_value_t = TierArg::Partial)]
        tier: TierArg,
    },
    /// Re-resolve every asset in `db` against the currently mounted volumes and report a
    /// resolved/offline/lost count -- the core "did the remap survive" check `remap-test.ps1`
    /// runs after each drive-letter/detach-reattach/reformat step.
    Resolve {
        #[arg(long, default_value = "homing.sqlite3")]
        db: PathBuf,
    },
    /// Attempt to relink every asset whose owning volume is offline against files found under
    /// `scan_dir` (a previously unrecognized volume), using tier-(b) partial hashes.
    Relink {
        scan_dir: PathBuf,
        #[arg(long, default_value = "homing.sqlite3")]
        db: PathBuf,
    },
    /// Fingerprint-cost microbenchmark: partial hash, full hash, and EXIF natural-key extraction
    /// timing across every file under `dir`.
    Bench { dir: PathBuf },
}

#[derive(Clone, Copy, ValueEnum)]
enum WatchBackend {
    Poll,
    Push,
}

#[derive(Clone, Copy, ValueEnum)]
enum TierArg {
    None,
    Partial,
    Full,
}

impl From<TierArg> for Option<relink::Tier> {
    fn from(t: TierArg) -> Self {
        match t {
            TierArg::None => None,
            TierArg::Partial => Some(relink::Tier::Partial),
            TierArg::Full => Some(relink::Tier::Full),
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Enumerate => cmd_enumerate(),
        Command::Watch {
            seconds,
            backend,
            interval_secs,
        } => cmd_watch(seconds, backend, interval_secs),
        Command::Build {
            root_dir,
            db,
            root_rel_path,
            tier,
        } => cmd_build(&root_dir, &db, &root_rel_path, tier.into()),
        Command::Resolve { db } => cmd_resolve(&db),
        Command::Relink { scan_dir, db } => cmd_relink(&scan_dir, &db),
        Command::Bench { dir } => cmd_bench(&dir),
    }
}

fn cmd_enumerate() -> Result<()> {
    let volumes = volume::windows_impl::enumerate()?;
    for v in &volumes {
        let key = volume::identity_key(v);
        println!(
            "{}",
            serde_json::json!({
                "identity_key": key,
                "info": v,
            })
        );
    }
    eprintln!("{} volume(s) enumerated", volumes.len());
    Ok(())
}

fn cmd_watch(seconds: u64, backend: WatchBackend, interval_secs: u64) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let events = match backend {
        WatchBackend::Poll => {
            mount_events::windows_impl::poll_for(deadline, Duration::from_secs(interval_secs))?
        }
        WatchBackend::Push => mount_events::windows_impl::watch_push(deadline)?,
    };
    for e in &events {
        println!("{}", serde_json::to_string(e)?);
    }
    println!("{}", mount_events::summarize(&events));
    Ok(())
}

fn current_volume_for(dir: &std::path::Path) -> Result<(String, volume::VolumeInfo, String)> {
    let volumes = volume::windows_impl::enumerate()?;
    let abs = dir
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", dir.display()))?;
    let abs_str = abs.to_string_lossy().replace('\\', "/");
    for v in volumes {
        for mp in &v.mount_points {
            let mp_norm = mp.trim_end_matches(['\\', '/']).replace('\\', "/");
            if abs_str.starts_with(&mp_norm) {
                let key = volume::identity_key(&v).with_context(|| {
                    format!("volume at {mp} has no usable identity key -- see ADR-0020")
                })?;
                return Ok((key, v.clone(), mp.clone()));
            }
        }
    }
    anyhow::bail!("no mounted volume found containing {}", dir.display())
}

fn cmd_build(
    root_dir: &std::path::Path,
    db_path: &std::path::Path,
    root_rel_path: &str,
    tier: Option<relink::Tier>,
) -> Result<()> {
    let (identity_key, info, mount_point) = current_volume_for(root_dir)?;
    let conn = rusqlite::Connection::open(db_path)?;
    schema::migrate(&conn)?;
    let vid = schema::upsert_volume(
        &conn,
        &identity_key,
        info.label.as_deref(),
        info.total_bytes,
        info.removable,
        &mount_point,
        relink::now_unix(),
    )?;
    let stats = relink::build(&conn, vid, root_rel_path, root_dir, tier)?;
    println!(
        "indexed {} file(s), {} error(s), volume identity {identity_key}",
        stats.files_indexed, stats.errors
    );
    Ok(())
}

fn cmd_resolve(db_path: &std::path::Path) -> Result<()> {
    let conn = rusqlite::Connection::open(db_path)?;
    let volumes = volume::windows_impl::enumerate()?;
    let mut mounted = std::collections::HashMap::new();
    let mut seen_keys = Vec::new();
    for v in &volumes {
        if let Some(key) = volume::identity_key(v) {
            if let Some(mp) = v.mount_points.first() {
                mounted.insert(key.clone(), mp.clone());
            }
            seen_keys.push(key);
        }
    }
    schema::mark_offline_except(&conn, &seen_keys)?;

    let mut stmt = conn.prepare("SELECT id FROM asset")?;
    let ids: Vec<i64> = stmt
        .query_map([], |row| row.get(0))?
        .filter_map(|r| r.ok())
        .collect();

    let mut resolved = 0;
    let mut offline = 0;
    for id in ids {
        match schema::resolve(&conn, id, &mounted)? {
            Some(_) => resolved += 1,
            None => offline += 1,
        }
    }
    println!("resolved: {resolved}, offline/unresolved: {offline}");
    Ok(())
}

fn cmd_relink(scan_dir: &std::path::Path, db_path: &std::path::Path) -> Result<()> {
    let conn = rusqlite::Connection::open(db_path)?;
    let mut candidates = std::collections::HashMap::new();
    for entry in walkdir::WalkDir::new(scan_dir)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry.path().strip_prefix(scan_dir).unwrap_or(entry.path());
        let rel_path = path::normalize_rel_path(rel);
        let size = entry.metadata()?.len();
        let hash = fingerprint::partial_hash(entry.path())?;
        candidates.insert(rel_path, (size, hash));
    }

    let results = relink::relink_against_unknown_volume(&conn, &candidates)?;
    let mut relinked = 0;
    let mut lost = 0;
    for (id, outcome) in &results {
        match outcome {
            relink::ResolveOutcome::RelinkedByFingerprint(p) => {
                relinked += 1;
                println!("asset {id} -> relinked to {p}");
            }
            relink::ResolveOutcome::Lost => {
                lost += 1;
                println!("asset {id} -> lost, no fingerprint match");
            }
            _ => {}
        }
    }
    println!("relinked: {relinked}, lost: {lost}");
    Ok(())
}

fn cmd_bench(dir: &std::path::Path) -> Result<()> {
    for entry in walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();

        let t0 = Instant::now();
        let _ = fingerprint::partial_hash(path)?;
        let partial_ms = t0.elapsed().as_secs_f64() * 1000.0;

        let t0 = Instant::now();
        let _ = fingerprint::full_hash(path)?;
        let full_ms = t0.elapsed().as_secs_f64() * 1000.0;

        let t0 = Instant::now();
        let key = fingerprint::natural_key(path)?;
        let natural_ms = t0.elapsed().as_secs_f64() * 1000.0;

        println!(
            "{}",
            serde_json::json!({
                "path": path.display().to_string(),
                "partial_hash_ms": partial_ms,
                "full_hash_ms": full_ms,
                "natural_key_ms": natural_ms,
                "natural_key_present": key.is_some(),
            })
        );
    }
    Ok(())
}
