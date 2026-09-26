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
    // `Path::canonicalize` on Windows can return a verbatim path (`\\?\H:\...`), which after
    // slash normalization becomes `//?/H:/...` -- strip that prefix so it lines up with the
    // enumerated mount points, which never carry it.
    let abs_str = abs.to_string_lossy().replace('\\', "/");
    let abs_str = abs_str.strip_prefix("//?/").unwrap_or(&abs_str);

    // Two bugs an earlier draft had: (1) a bare `starts_with` matches a sibling path with a
    // shared prefix that isn't a real path-component boundary (e.g. mount point `C:/Mount` would
    // also match `C:/MountOther`); (2) among multiple genuinely-matching mount points, the first
    // one enumerated (not the most specific/longest one) would win. Track the longest
    // component-boundary match instead of returning on the first hit.
    let mut best: Option<(usize, volume::VolumeInfo, String)> = None;
    for v in volumes {
        for mp in &v.mount_points {
            let mp_norm = mp.trim_end_matches(['\\', '/']).replace('\\', "/");
            let component_match = abs_str == mp_norm
                || abs_str
                    .strip_prefix(&mp_norm)
                    .is_some_and(|rest| rest.starts_with('/'));
            if component_match
                && best
                    .as_ref()
                    .is_none_or(|(best_len, _, _)| mp_norm.len() > *best_len)
            {
                best = Some((mp_norm.len(), v.clone(), mp.clone()));
            }
        }
    }
    if let Some((_, v, mp)) = best {
        let key = volume::identity_key(&v).with_context(|| {
            format!("volume at {mp} has no usable identity key -- see ADR-0020")
        })?;
        return Ok((key, v, mp));
    }
    anyhow::bail!("no mounted volume found containing {}", dir.display())
}

/// The portion of `dir`'s canonicalized path that sits *below* `mount_point` -- what
/// `insert_root`'s `rel_path` should be. Passing an empty string unconditionally (an earlier
/// draft's bug) is only correct when `dir` *is* the volume root; if it's a subfolder (e.g.
/// `H:\Photos`), an empty root_rel would make every asset resolve to `H:\<rel>` instead of
/// `H:\Photos\<rel>`, silently pointing at the wrong path.
fn root_rel_path_for(dir: &std::path::Path, mount_point: &str) -> Result<String> {
    let abs = dir
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", dir.display()))?;
    let abs_str = abs.to_string_lossy().replace('\\', "/");
    let abs_str = abs_str.strip_prefix("//?/").unwrap_or(&abs_str);
    let mp_norm = mount_point.trim_end_matches(['\\', '/']).replace('\\', "/");
    // Same component-boundary requirement `current_volume_for` enforces: a bare `strip_prefix`
    // would treat mount point `H:/Mount` as a prefix of `H:/MountOther/Photos` too, silently
    // producing a wrong-but-non-crashing `root_rel` ("ther/Photos") instead of failing loudly.
    // `dir == mount_point` (root_rel is empty) is the one case where there's no `/` to require.
    let root_rel = if abs_str == mp_norm {
        ""
    } else {
        abs_str
            .strip_prefix(&mp_norm)
            .filter(|rest| rest.starts_with('/'))
            .with_context(|| {
                format!(
                    "{} is not actually under mount point {mount_point}",
                    dir.display()
                )
            })?
            .trim_start_matches('/')
    };
    Ok(root_rel.to_string())
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
        "indexed {} file(s), {} error(s), {} fingerprint failure(s), volume identity {identity_key}",
        stats.files_indexed, stats.errors, stats.fingerprint_failures
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
        // One unreadable/truncated-mid-read candidate must not abort the scan for every other
        // candidate -- an earlier draft used `?` here, so a single bad file on the unrecognized
        // volume would silently prevent every other asset from getting a chance to relink.
        let size = match entry.metadata() {
            Ok(m) => m.len(),
            Err(_) => continue,
        };
        // Compute both hash tiers for each candidate: `homing build --tier full` stores a
        // full-file hash on the asset side, but relink only ever computed a partial hash here --
        // an unchanged full-tier asset on an offline volume could never match by fingerprint at
        // all and would be reported `Lost` even when its real file was sitting right there. This
        // is relink-time-only work (not routine import), so paying for both tiers per candidate
        // is acceptable.
        let partial_hash = match fingerprint::partial_hash(entry.path()) {
            Ok(h) => h,
            Err(_) => continue,
        };
        let full_hash = match fingerprint::full_hash(entry.path()) {
            Ok(h) => h,
            Err(_) => continue,
        };
        candidates.insert(rel_path, (size, partial_hash, full_hash));
    }

    let results = relink::relink_against_unknown_volume(&conn, &candidates)?;

    // Persist every successful match into the catalog -- an earlier draft only ever printed the
    // match to stdout, so a later `homing resolve` would still read each asset's stale (offline
    // volume's) root_id/rel_path and report it unresolved forever. `scan_dir` itself becomes a
    // new `root` under whichever volume currently owns it (registered lazily, only if at least
    // one match needs it, so a scan that finds nothing doesn't create an unused root row).
    let mut new_root_id: Option<i64> = None;
    let mut relinked = 0;
    let mut lost = 0;
    for (id, outcome) in &results {
        let matched_path = match outcome {
            relink::ResolveOutcome::RelinkedByFingerprint(p) => {
                println!("asset {id} -> relinked to {p} (fingerprint match)");
                Some(p)
            }
            relink::ResolveOutcome::RelinkedBySizeName(p) => {
                println!(
                    "asset {id} -> relinked to {p} (size+name only, no fingerprint available)"
                );
                Some(p)
            }
            relink::ResolveOutcome::Lost => {
                lost += 1;
                println!("asset {id} -> lost, no match");
                None
            }
            _ => None,
        };
        let Some(matched_path) = matched_path else {
            continue;
        };
        relinked += 1;

        let root_id = match new_root_id {
            Some(id) => id,
            None => {
                let (identity_key, info, mount_point) = current_volume_for(scan_dir)?;
                let root_rel = root_rel_path_for(scan_dir, &mount_point)?;
                let vid = schema::upsert_volume(
                    &conn,
                    &identity_key,
                    info.label.as_deref(),
                    info.total_bytes,
                    info.removable,
                    &mount_point,
                    relink::now_unix(),
                )?;
                let rid = schema::insert_root(&conn, vid, &root_rel)?;
                new_root_id = Some(rid);
                rid
            }
        };
        schema::relink_asset(&conn, *id, root_id, matched_path, &path::fold(matched_path))?;
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

#[cfg(test)]
mod tests {
    use super::root_rel_path_for;
    use tempfile::TempDir;

    #[test]
    fn root_rel_path_for_is_empty_when_dir_is_the_volume_root() {
        let dir = TempDir::new().unwrap();
        let abs = dir.path().canonicalize().unwrap();
        let mount_point = abs.to_string_lossy().replace('\\', "/");
        assert_eq!(root_rel_path_for(dir.path(), &mount_point).unwrap(), "");
    }

    #[test]
    fn root_rel_path_for_captures_a_subfolder_below_the_mount_point() {
        let dir = TempDir::new().unwrap();
        let sub = dir.path().join("Photos").join("2026");
        std::fs::create_dir_all(&sub).unwrap();
        let mount_point = dir
            .path()
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        assert_eq!(
            root_rel_path_for(&sub, &mount_point).unwrap(),
            "Photos/2026"
        );
    }

    #[test]
    fn root_rel_path_for_rejects_a_sibling_prefix_that_is_not_a_real_component_match() {
        // "Mount" is a *string* prefix of "MountOther" but not its path-component ancestor --
        // exactly the false-positive class current_volume_for is fixed against elsewhere in this
        // file. This function must reject it the same way, not silently return a
        // wrong-but-non-crashing "ther/Photos". Both directories are real (canonicalize must
        // succeed) so the rejection under test is the strip_prefix component check itself, not
        // an I/O error from a nonexistent path.
        let base = TempDir::new().unwrap();
        let mount = base.path().join("Mount");
        let sibling = base.path().join("MountOther").join("Photos");
        std::fs::create_dir_all(&mount).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        let mount_point = mount
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        assert!(root_rel_path_for(&sibling, &mount_point).is_err());
    }
}
