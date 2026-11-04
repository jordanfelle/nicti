//! XMP sidecar sync (#60, ADR-0059): the one place the catalog and `nicti-scent` meet.
//!
//! Two directions, both keyed by the per-asset `asset_sidecar` row (what nicti last saw/wrote in
//! the `.xmp`, and since when the catalog has diverged from it):
//!
//! - [`import_sidecar`] -- sidecar -> catalog, on ingest and on Synchronize Folder.
//! - [`write_sidecar`] -- catalog -> sidecar, after every marker change (auto-write).
//!
//! Sides are compared by a *canonical marker set* ([`Markers`]) rather than packet bytes, so LRC
//! reformatting a packet isn't a conflict and a no-op edit never touches the file. Conflicts
//! follow ADR-0021's newer-wins rule (`nicti_scent::conflict::resolve_conflict`), with the
//! catalog side's "mtime" being `catalog_dirty_since_ms`. The write path is stricter than the
//! read path: if the sidecar changed under us and we haven't ingested that change, a write is
//! *held for review* rather than clobbering it.
//!
//! Only layer (a) (rating / reject / label / keywords, plus nicti's own `nicti:pick`) is handled
//! here; ADR-0059 is still Proposed pending #187, so the Reject/Pick/label mappings are its
//! current best guess.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use nicti_scent::conflict::{read_hashed, resolve_conflict, Resolution, Side};
use nicti_scent::gpx::Gps;
use nicti_scent::lrc_fields::{self, LrcMeta};
use nicti_scent::packet::{self, Patch};
use nicti_scent::sidecar::atomic_write;
pub use nicti_scent::sidecar::sidecar_path;

use crate::{AssetMeta, CatalogError, CatalogStore, Keyword, SidecarState};

/// Two changes closer together than this can't be ordered reliably (filesystem mtime granularity,
/// LRC's own save latency), so they're flagged for review instead of guessing a winner.
pub const AMBIGUITY_WINDOW_MS: u128 = 2_000;

/// One process-wide writer lock. ADR-0059 requires single-writer discipline across the
/// read-hash -> gate -> write -> record-hash sequence; every entry point below takes this.
static SYNC_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, thiserror::Error)]
pub enum ScentSyncError {
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    #[error("reading or writing {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path} is not a parseable XMP packet: {message}")]
    Xmp { path: PathBuf, message: String },
    #[error("asset {0} not found")]
    NoAsset(i64),
}

/// What a sync call did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncOutcome {
    /// No `.xmp` exists next to the RAW (import only).
    NoSidecar,
    /// Both sides already agreed; nothing written.
    InSync,
    /// The sidecar's markers were applied to the catalog.
    AppliedSidecar,
    /// The catalog's markers were written to the sidecar.
    WroteSidecar,
    /// Both sides changed and neither was touched; the asset is flagged for review.
    NeedsReview,
}

/// The LRC-visible markers of one asset, canonicalised so the two sides compare by meaning.
#[derive(Debug, Clone, Default)]
pub struct Markers {
    /// `None` = unrated, `Some(-1)` = reject, `Some(0..=5)` = stars.
    pub rating: Option<i8>,
    pub label: Option<String>,
    pub pick: bool,
    /// Each keyword as its name path (`["Events", "Anthrocon"]`); flat keywords are 1-segment.
    pub keywords: BTreeSet<Vec<String>>,
    /// The `exif:GPS*` position (#536). Deliberately outside `==` and the hash: a position never
    /// raises a conflict by itself (it is carried alongside the markers, written whenever the
    /// catalog has one, and imported when the catalog has none or the sidecar wins).
    pub gps: Option<Gps>,
}

/// Whether two positions are the same to the precision XMP stores (1e-6 degrees ~ 0.1 m, 1 cm).
fn gps_close(a: &Gps, b: &Gps) -> bool {
    (a.latitude - b.latitude).abs() < 1e-5
        && (a.longitude - b.longitude).abs() < 1e-5
        && match (a.altitude, b.altitude) {
            (Some(x), Some(y)) => (x - y).abs() < 0.02,
            (None, None) => true,
            _ => false,
        }
}

/// One keyword path segment in canonical form: trimmed, and with `|` (the `lr:hierarchicalSubject`
/// separator, which can't be escaped) replaced by `/`. Applied to *both* sides so a name that
/// can't survive the XMP round trip still compares equal to itself instead of flapping forever.
pub fn norm_segment(seg: &str) -> String {
    seg.trim().replace('|', "/")
}

fn norm_path<I: IntoIterator<Item = String>>(segs: I) -> Option<Vec<String>> {
    let p: Vec<String> = segs
        .into_iter()
        .map(|s| norm_segment(&s))
        .filter(|s| !s.is_empty())
        .collect();
    (!p.is_empty()).then_some(p)
}

/// A keyword path as the catalog compares it (NFC + lowercase per segment, like `keyword.name_fold`),
/// so a case-only difference between the sidecar and the catalog is not a conflict (#375).
fn fold_path(p: &[String]) -> Vec<String> {
    p.iter()
        .map(|s| crate::sqlite::fold_keyword_name(s))
        .collect()
}

impl Markers {
    fn folded_keywords(&self) -> BTreeSet<Vec<String>> {
        self.keywords.iter().map(|p| fold_path(p)).collect()
    }
}

impl Markers {
    /// Equality without case folding: whether the sidecar would need rewriting to show exactly
    /// these keyword spellings (a case-only rename is "equal" for conflict detection only).
    fn exactly_eq(&self, o: &Self) -> bool {
        self == o
            && self.keywords == o.keywords
            // Only a catalog position needs to show up in the file; a file-only one is left alone.
            && match (&self.gps, &o.gps) {
                (Some(file), Some(cat)) => gps_close(file, cat),
                (_, None) => true,
                (None, Some(_)) => false,
            }
    }
}

impl PartialEq for Markers {
    fn eq(&self, o: &Self) -> bool {
        self.rating == o.rating
            && self.label == o.label
            && self.pick == o.pick
            && self.folded_keywords() == o.folded_keywords()
    }
}
impl Eq for Markers {}

impl Markers {
    fn is_empty(&self) -> bool {
        *self == Markers::default()
    }

    fn hash(&self) -> blake3::Hash {
        let mut h = blake3::Hasher::new();
        h.update(format!("{:?}|{:?}|{}", self.rating, self.label, self.pick).as_bytes());
        for kw in &self.folded_keywords() {
            h.update(b"\x1f");
            h.update(kw.join("\x1e").as_bytes());
        }
        h.finalize()
    }

    /// Canonicalises a parsed sidecar. LRC lists every keyword's leaf in `dc:subject` *and* the
    /// full path in `lr:hierarchicalSubject`, so a flat keyword that's the leaf of some
    /// hierarchical path is the same keyword, not a second top-level one.
    pub fn from_lrc(meta: &LrcMeta) -> Markers {
        let mut keywords: BTreeSet<Vec<String>> = meta
            .hierarchical_keywords
            .iter()
            .filter_map(|p| norm_path(p.iter().cloned()))
            .collect();
        let leaves: BTreeSet<&str> = keywords
            .iter()
            .filter_map(|p| p.last().map(String::as_str))
            .collect();
        let flat_only: Vec<Vec<String>> = meta
            .keywords
            .iter()
            .filter(|k| !leaves.contains(k.as_str()))
            .filter_map(|k| norm_path([k.clone()]))
            .collect();
        keywords.extend(flat_only);
        Markers {
            rating: meta.rating,
            label: meta.label.clone(),
            pick: meta.pick,
            keywords,
            gps: None,
        }
    }

    fn to_patch(&self) -> Patch {
        let mut flat: Vec<String> = self
            .keywords
            .iter()
            .filter_map(|p| p.last().cloned())
            .collect();
        flat.sort();
        flat.dedup();
        Patch {
            rating: Some(self.rating),
            label: Some(self.label.clone()),
            keywords: Some(flat),
            // Every keyword goes in as a path, single-segment ones included: that is what lets a
            // top-level "Anthrocon" and an "Events|Anthrocon" on one photo stay two keywords on
            // read-back (a flat entry that is the leaf of any path is treated as that path's).
            hierarchical_keywords: Some(self.keywords.iter().cloned().collect()),
            nicti_pick: Some(self.pick),
            // `None` leaves whatever position the file has; only a catalog position is written.
            gps: self.gps.map(Some),
            ..Default::default()
        }
    }
}

/// A valid, empty packet for a RAW that has no sidecar yet. Declares every prefix `Patch` writes.
const TEMPLATE: &str = "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n\
<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\
<rdf:Description rdf:about=\"\" xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\" \
xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmlns:lr=\"http://ns.adobe.com/lightroom/1.0/\"/>\
</rdf:RDF></x:xmpmeta>\n<?xpacket end=\"w\"?>";

/// The name path of each keyword in `tagged`, canonicalised like the sidecar side
/// ([`norm_path`]), given `names` (`list_keywords` id -> name). Each keyword's materialised
/// `path` is a `/`-joined chain of ancestor ids ending at itself.
pub fn name_paths(tagged: &[Keyword], names: &HashMap<i64, String>) -> BTreeSet<Vec<String>> {
    tagged
        .iter()
        .filter_map(|k| {
            norm_path(
                k.path
                    .split('/')
                    .filter(|s| !s.is_empty())
                    .filter_map(|s| s.parse::<i64>().ok())
                    .filter_map(|id| names.get(&id).cloned()),
            )
        })
        .collect()
}

fn keyword_name_paths(
    store: &dyn CatalogStore,
    asset_id: i64,
) -> Result<BTreeSet<Vec<String>>, CatalogError> {
    let tagged = store.keywords_for(asset_id)?;
    if tagged.is_empty() {
        return Ok(BTreeSet::new());
    }
    let names = store.keyword_names_for(asset_id)?;
    Ok(name_paths(&tagged, &names))
}

/// The catalog's current markers for one asset.
pub fn catalog_markers(store: &dyn CatalogStore, asset_id: i64) -> Result<Markers, ScentSyncError> {
    let meta = store
        .get_meta(&[asset_id])?
        .remove(&asset_id)
        .ok_or(ScentSyncError::NoAsset(asset_id))?;
    Ok(Markers {
        rating: meta.rating.and_then(|r| i8::try_from(r).ok()),
        label: meta.label,
        pick: meta.flag == Some(1),
        keywords: keyword_name_paths(store, asset_id)?,
        gps: store.get_gps(&[asset_id])?.remove(&asset_id),
    })
}

fn apply_to_catalog(
    store: &dyn CatalogStore,
    asset_id: i64,
    m: &Markers,
) -> Result<(), ScentSyncError> {
    // Resolve (creating as needed) each wanted path down to its leaf keyword id. Creating a keyword
    // that ends up unused is harmless; the asset itself changes in one transaction below.
    let mut wanted: BTreeSet<i64> = BTreeSet::new();
    for path in &m.keywords {
        let mut parent: Option<i64> = None;
        for i in 0..path.len() {
            let segs: Vec<&str> = path[..=i].iter().map(String::as_str).collect();
            let id = match store.keyword_by_path(&segs)? {
                Some(k) => k.id,
                None => store.create_keyword(parent, &path[i])?,
            };
            parent = Some(id);
        }
        if let Some(leaf) = parent {
            wanted.insert(leaf);
        }
    }
    let ids: Vec<i64> = wanted.into_iter().collect();
    store.replace_asset_markers(
        asset_id,
        &AssetMeta {
            rating: m.rating.map(i64::from),
            flag: m.pick.then_some(1),
            label: m.label.clone(),
        },
        &ids,
    )?;
    // A file with no position never clears the catalog's (a sidecar LRC rewrote without GPS is
    // not a decision to drop it); a file with one is the newer side here, so it wins.
    if let Some(g) = m.gps {
        store.set_gps(asset_id, Some(g))?;
    }
    Ok(())
}

struct Loaded {
    text: String,
    hash: blake3::Hash,
    mtime_ms: u128,
    markers: Markers,
    /// The whole JPEG as read, when the XMP is embedded; the write splices the new packet into
    /// these bytes. `None` for a `.xmp` sidecar.
    file: Option<JpegSnapshot>,
}

/// A JPEG's bytes plus the (length, mtime) it had *before* they were read: the write refuses to
/// replace the file if either has moved on, so another program's save is never rolled back.
struct JpegSnapshot {
    bytes: Vec<u8>,
    stamp: Option<(u64, std::time::SystemTime)>,
}

fn stamp_of(path: &Path) -> Option<(u64, std::time::SystemTime)> {
    let m = fs::metadata(path).ok()?;
    Some((m.len(), m.modified().ok()?))
}

/// Replaces an original photo with `bytes`, carefully: resolves a symlink to its target, refuses a
/// read-only file, keeps its permissions, fsyncs before the rename, and aborts if the file changed
/// since `snapshot_stamp` (a concurrent save by another program).
fn replace_jpeg(
    path: &Path,
    bytes: &[u8],
    snapshot_stamp: Option<(u64, std::time::SystemTime)>,
) -> std::io::Result<()> {
    use std::io::Write;
    let real = fs::canonicalize(path)?;
    let meta = fs::metadata(&real)?;
    if meta.permissions().readonly() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "JPEG is read-only; not rewriting it",
        ));
    }
    let unchanged = snapshot_stamp.is_some_and(|(len, mtime)| {
        meta.len() == len && meta.modified().ok().is_some_and(|m| m == mtime)
    });
    if !unchanged {
        return Err(std::io::Error::other(
            "JPEG changed while its XMP was being updated; will retry",
        ));
    }
    let dir = real.parent().unwrap_or_else(|| Path::new("."));
    let name = real
        .file_name()
        .ok_or_else(|| std::io::Error::other("path has no file name"))?
        .to_string_lossy()
        .into_owned();
    let tmp = dir.join(format!(".{name}.scent-tmp"));
    let result = (|| {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.set_permissions(meta.permissions())?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, &real)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Reads and parses the sidecar; `Ok(None)` only when the file genuinely doesn't exist. A read or
/// parse failure is an error, never "blank" -- so a half-written sidecar can't be overwritten.
fn load(path: &Path) -> Result<Option<Loaded>, ScentSyncError> {
    let stamp = stamp_of(path);
    let (bytes, hash, mtime_ms) = match read_hashed(path) {
        Ok(v) => v,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(ScentSyncError::Io {
                path: path.to_path_buf(),
                source,
            })
        }
    };
    let xmp_err = |message: String| ScentSyncError::Xmp {
        path: path.to_path_buf(),
        message,
    };
    let (text, hash, file) = if crate::scruff::is_jpeg(path) {
        // Embedded: the hash covers the XMP packet only, so re-encoding the pixels or touching
        // EXIF never reads as an XMP change. A JPEG with no packet has nothing to import.
        let Some(text) =
            nicti_scent::embedded::read_xmp(&bytes).map_err(|e| xmp_err(e.to_string()))?
        else {
            return Ok(None);
        };
        let hash = blake3::hash(text.as_bytes());
        (text, hash, Some(JpegSnapshot { bytes, stamp }))
    } else {
        let text = String::from_utf8(bytes).map_err(|e| xmp_err(e.to_string()))?;
        (text, hash, None)
    };
    let meta = lrc_fields::read(&text).map_err(|e| xmp_err(e.to_string()))?;
    let mut markers = Markers::from_lrc(&meta);
    markers.gps = nicti_scent::gpx::read_gps(&text);
    Ok(Some(Loaded {
        text,
        hash,
        mtime_ms,
        markers,
        file,
    }))
}

/// The file's mtime in epoch milliseconds, `None` if it can't be stat'ed.
fn mtime_ms_of(path: &Path) -> Option<u128> {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis())
}

fn state_for(path: &Path, prior: Option<SidecarState>) -> SidecarState {
    let mut s = prior.unwrap_or_default();
    s.path = path.to_string_lossy().into_owned();
    s
}

/// The mtime worth remembering for `import_sidecar`'s stat-only precheck, or `None` when it can't
/// be trusted. A file touched within `AMBIGUITY_WINDOW_MS` of `now_ms` (or stamped in the future)
/// may be rewritten again inside the same filesystem timestamp tick -- coarse on Windows/FAT -- and
/// that rewrite would keep this exact mtime. Same "racily clean" rule git applies to its index:
/// leave it unrecorded so the next import re-hashes, and records a trusted mtime once it has aged.
/// Known limits: it compares the file's clock to nicti's, so a NAS clock more than the window behind
/// ours can still be trusted too early; and rows recorded before this guard keep their old mtime.
fn trusted_mtime(mtime_ms: u128, now_ms: i64) -> Option<i64> {
    let mtime = i64::try_from(mtime_ms).ok()?;
    let window = i64::try_from(AMBIGUITY_WINDOW_MS).unwrap_or(i64::MAX);
    (now_ms.saturating_sub(mtime) >= window).then_some(mtime)
}

fn seen(s: &mut SidecarState, l: &Loaded, now_ms: i64) {
    s.last_seen_hash = Some(l.hash.as_bytes().to_vec());
    s.last_seen_mtime_ms = trusted_mtime(l.mtime_ms, now_ms);
}

/// A row `import_sidecar` leaves for a JPEG with no XMP packet: an mtime and nothing else.
fn is_blank_marker(p: &SidecarState) -> bool {
    p.last_seen_hash.is_none()
        && p.last_written_hash.is_none()
        && p.catalog_dirty_since_ms.is_none()
        && !p.needs_review
}

fn hash_is(stored: &Option<Vec<u8>>, h: &blake3::Hash) -> bool {
    stored.as_deref() == Some(h.as_bytes().as_slice())
}

/// sidecar -> catalog. Call on ingest and on Synchronize Folder, for every asset whose RAW has
/// (or may have) an `.xmp` beside it.
pub fn import_sidecar(
    store: &dyn CatalogStore,
    asset_id: i64,
    raw_path: &Path,
    now_ms: i64,
) -> Result<SyncOutcome, ScentSyncError> {
    import_sidecar_inner(store, asset_id, raw_path, now_ms, false)
}

/// [`import_sidecar`] for a file ingest just found changed (size or mtime moved): skips the
/// mtime-only precheck, since a tool that preserves mtime (`exiftool -P`) can add an XMP packet
/// without moving it.
pub fn import_sidecar_changed(
    store: &dyn CatalogStore,
    asset_id: i64,
    raw_path: &Path,
    now_ms: i64,
) -> Result<SyncOutcome, ScentSyncError> {
    import_sidecar_inner(store, asset_id, raw_path, now_ms, true)
}

fn import_sidecar_inner(
    store: &dyn CatalogStore,
    asset_id: i64,
    raw_path: &Path,
    now_ms: i64,
    file_changed: bool,
) -> Result<SyncOutcome, ScentSyncError> {
    let _guard = SYNC_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let path = xmp_target(raw_path);
    let prior = store.sidecar_state(asset_id)?;
    // Cheap precheck for rescans of a large library: a stat, not a read + hash, when the sidecar's
    // mtime is exactly what nicti last recorded and nothing is pending.
    // A dirty catalog (auto-write off, or a write that failed) must never take either early return:
    // the newer-wins comparison below is what pushes its markers to the sidecar.
    if let Some(p) = prior
        .as_ref()
        .filter(|p| p.catalog_dirty_since_ms.is_none() && !file_changed)
    {
        if !p.needs_review {
            let on_disk = mtime_ms_of(&path).and_then(|ms| i64::try_from(ms).ok());
            if on_disk.is_some() && on_disk == p.last_seen_mtime_ms {
                return Ok(SyncOutcome::InSync);
            }
        }
    }
    // Stat before the read: if the file changes during it, the recorded mtime is already stale
    // and the next import simply looks again.
    let pre_read_mtime = mtime_ms_of(&path);
    let Some(loaded) = load(&path)? else {
        // A JPEG that exists but carries no XMP packet (#665): remember that we looked, keyed by
        // its mtime, so the stat-only precheck skips the whole-file read + hash on every rescan.
        // The row holds no hashes, which `is_blank_marker` uses to keep it from reading as a
        // prior sync. Only ever written over nothing or an older marker.
        let mtime = pre_read_mtime.and_then(|ms| trusted_mtime(ms, now_ms));
        if crate::scruff::is_jpeg(&path)
            && prior.as_ref().is_none_or(is_blank_marker)
            && mtime.is_some()
            && prior.as_ref().and_then(|p| p.last_seen_mtime_ms) != mtime
        {
            let mut state = state_for(&path, prior.clone());
            state.last_seen_mtime_ms = mtime;
            store.record_sidecar(asset_id, &state)?;
        }
        return Ok(SyncOutcome::NoSidecar);
    };
    // A no-packet marker row is not a prior sync: the first packet to appear resolves exactly as
    // it would have with no row at all.
    let prior = prior.filter(|p| !is_blank_marker(p));
    if let Some(p) = prior
        .as_ref()
        .filter(|p| p.catalog_dirty_since_ms.is_none())
    {
        // Unchanged since nicti last looked, and nothing pending: nothing to do.
        if hash_is(&p.last_seen_hash, &loaded.hash) && !p.needs_review {
            // Content unchanged: if the mtime has since aged out of the racy window, record it so
            // the next rescan takes the stat-only path again.
            let mtime = trusted_mtime(loaded.mtime_ms, now_ms);
            if mtime.is_some() && mtime != p.last_seen_mtime_ms {
                let mut state = state_for(&path, prior.clone());
                seen(&mut state, &loaded, now_ms);
                store.record_sidecar(asset_id, &state)?;
            }
            return Ok(SyncOutcome::InSync);
        }
    }
    let catalog = catalog_markers(store, asset_id)?;
    let mut state = state_for(&path, prior.clone());
    seen(&mut state, &loaded, now_ms);

    if catalog == loaded.markers {
        // Markers agree; positions are settled separately (they are outside `==`). The catalog is
        // "newer" exactly when it is marked dirty; otherwise the file is, so its position wins.
        let file_gps = loaded.markers.gps;
        let differs = match (catalog.gps, file_gps) {
            (Some(c), Some(f)) => !gps_close(&c, &f),
            (Some(_), None) => true,
            _ => false,
        };
        if differs && state.catalog_dirty_since_ms.is_some() {
            // A pending catalog position (a geotag with auto-write off or a held write). Push it,
            // unless the review hold is up or the file's own position changed since nicti last saw
            // the file: then neither side is safe to prefer.
            let file_changed_under_us = file_gps.is_some()
                && !prior.as_ref().is_some_and(|p| {
                    hash_is(&p.last_seen_hash, &loaded.hash)
                        || hash_is(&p.last_written_hash, &loaded.hash)
                });
            if prior.as_ref().is_some_and(|p| p.needs_review) || file_changed_under_us {
                state.needs_review = true;
                store.record_sidecar(asset_id, &state)?;
                return Ok(SyncOutcome::NeedsReview);
            }
            return write_locked(store, asset_id, raw_path, now_ms, true, false);
        }
        if let Some(g) = file_gps {
            // Catalog has none, or is not newer and the file's differs: adopt the file's. (A file
            // with no position never clears the catalog's.)
            if catalog.gps.is_none() || differs {
                store.set_gps(asset_id, Some(g))?;
            }
        }
        state.catalog_dirty_since_ms = None;
        state.needs_review = false;
        store.record_sidecar(asset_id, &state)?;
        return Ok(SyncOutcome::InSync);
    }

    // A conflict already held for review stays held until someone resolves it (`resolve_review`):
    // the catalog-dirty time recorded at hold time is when the *write was attempted*, not when
    // the user's edit raced the file, so re-running newer-wins here would quietly overwrite the
    // un-ingested edit the hold exists to protect.
    if prior.as_ref().is_some_and(|p| p.needs_review) {
        return Ok(SyncOutcome::NeedsReview);
    }

    // Who wins? With no record of a prior sync, the catalog's edits are unknown: a pristine
    // catalog yields to the file, anything else is held for review rather than guessed at.
    // A JPEG's file mtime moves for pixel/EXIF edits that never touched the XMP, so it can't
    // show the packet was edited: when the packet is byte-for-byte what nicti last saw or
    // wrote, the only pending change is the catalog's.
    let packet_unchanged = loaded.file.is_some()
        && prior.as_ref().is_some_and(|p| {
            hash_is(&p.last_seen_hash, &loaded.hash) || hash_is(&p.last_written_hash, &loaded.hash)
        });
    let resolution = match (&prior, state.catalog_dirty_since_ms) {
        (Some(_), Some(_)) if packet_unchanged => Resolution::PreferCatalog,
        (None, _) if catalog.is_empty() => Resolution::PreferSidecar,
        // The file carries no markers at all (e.g. only `crs:` develop data), so there is nothing
        // in it to lose: the catalog's markers are simply added to it.
        (None, _) if loaded.markers.is_empty() => Resolution::PreferCatalog,
        (None, _) => Resolution::FlagForManualReview,
        (Some(_), None) => Resolution::PreferSidecar,
        (Some(_), Some(dirty_ms)) => resolve_conflict(
            &Side {
                content_hash: catalog.hash(),
                mtime_ms: u128::try_from(dirty_ms).unwrap_or(0),
            },
            &Side {
                content_hash: loaded.markers.hash(),
                mtime_ms: loaded.mtime_ms,
            },
            AMBIGUITY_WINDOW_MS,
        ),
    };
    match resolution {
        Resolution::PreferSidecar | Resolution::NoConflict => {
            apply_to_catalog(store, asset_id, &loaded.markers)?;
            state.catalog_dirty_since_ms = None;
            state.needs_review = false;
            store.record_sidecar(asset_id, &state)?;
            Ok(SyncOutcome::AppliedSidecar)
        }
        Resolution::PreferCatalog => {
            // The catalog side is newer; already under SYNC_LOCK, so call the unlocked writer.
            write_locked(store, asset_id, raw_path, now_ms, true, false)
        }
        Resolution::FlagForManualReview => {
            state.needs_review = true;
            state.catalog_dirty_since_ms.get_or_insert(now_ms);
            store.record_sidecar(asset_id, &state)?;
            Ok(SyncOutcome::NeedsReview)
        }
    }
}

/// Where an asset's XMP lives: the JPEG itself (embedded APP1 packet, #371 / ADR-0059) or the
/// `.xmp` beside a RAW. `IMG.xmp` belongs to the RAW of a RAW+JPEG pair, so a JPEG never maps to it.
pub fn xmp_target(raw_path: &Path) -> PathBuf {
    if crate::scruff::is_jpeg(raw_path) {
        raw_path.to_path_buf()
    } else {
        sidecar_path(raw_path)
    }
}

/// catalog -> sidecar. Call after every marker change when auto-write is on.
pub fn write_sidecar(
    store: &dyn CatalogStore,
    asset_id: i64,
    raw_path: &Path,
    now_ms: i64,
) -> Result<SyncOutcome, ScentSyncError> {
    let _guard = SYNC_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    write_locked(store, asset_id, raw_path, now_ms, false, false)
}

/// Records that the catalog changed while auto-write is off, so a later import knows the
/// catalog side is newer than whatever the sidecar holds.
pub fn mark_catalog_dirty(
    store: &dyn CatalogStore,
    asset_id: i64,
    raw_path: &Path,
    now_ms: i64,
) -> Result<(), ScentSyncError> {
    let _guard = SYNC_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let path = xmp_target(raw_path);
    let mut state = state_for(&path, store.sidecar_state(asset_id)?);
    state.catalog_dirty_since_ms = Some(now_ms);
    store.record_sidecar(asset_id, &state)?;
    Ok(())
}

/// [`mark_catalog_dirty`] for many assets under one lock acquisition (#62: a catalog import
/// changes thousands of markers at once). Each item is `(asset_id, raw_path)`.
pub fn mark_catalog_dirty_many(
    store: &dyn CatalogStore,
    items: &[(i64, std::path::PathBuf)],
    now_ms: i64,
) -> Result<(), ScentSyncError> {
    let _guard = SYNC_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    for (asset_id, raw_path) in items {
        let path = xmp_target(raw_path);
        let mut state = state_for(&path, store.sidecar_state(*asset_id)?);
        state.catalog_dirty_since_ms = Some(now_ms);
        store.record_sidecar(*asset_id, &state)?;
    }
    Ok(())
}

/// Settles a flagged conflict: `use_catalog` overwrites the sidecar, otherwise the sidecar is
/// applied to the catalog. Either way the review flag clears.
pub fn resolve_review(
    store: &dyn CatalogStore,
    asset_id: i64,
    raw_path: &Path,
    use_catalog: bool,
    now_ms: i64,
) -> Result<SyncOutcome, ScentSyncError> {
    let _guard = SYNC_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    if use_catalog {
        return write_locked(store, asset_id, raw_path, now_ms, true, false);
    }
    let path = xmp_target(raw_path);
    let Some(loaded) = load(&path)? else {
        return Ok(SyncOutcome::NoSidecar);
    };
    apply_to_catalog(store, asset_id, &loaded.markers)?;
    let mut state = state_for(&path, store.sidecar_state(asset_id)?);
    seen(&mut state, &loaded, now_ms);
    state.catalog_dirty_since_ms = None;
    state.needs_review = false;
    store.record_sidecar(asset_id, &state)?;
    Ok(SyncOutcome::AppliedSidecar)
}

/// What [`clear_position`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClearOutcome {
    /// The position is gone from the catalog and from the file.
    Cleared,
    /// There was no position anywhere.
    NothingToClear,
    /// The file carries a position and XMP auto-write is off: clearing only the catalog would let
    /// the next import adopt the file's position straight back, so nothing was changed.
    NeedsAutoWrite,
    /// The sidecar changed under us and the write is held for review (see `resolve_review`);
    /// nothing was changed.
    HeldForReview,
}

/// Removes an asset's GPS position (#668), from the catalog and -- through the usual write-back --
/// from its XMP. A file position the catalog doesn't mirror is never cleared by an ordinary write
/// (a file-only position is adopted, not removed), so this writes an explicit clear. The catalog
/// is cleared only after the file write succeeded, so a failed or held write loses nothing.
pub fn clear_position(
    store: &dyn CatalogStore,
    asset_id: i64,
    raw_path: &Path,
    auto_write: bool,
    now_ms: i64,
) -> Result<ClearOutcome, ScentSyncError> {
    let _guard = SYNC_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let path = xmp_target(raw_path);
    let file_has_gps = load(&path)?.is_some_and(|l| l.markers.gps.is_some());
    let in_catalog = store.get_gps(&[asset_id])?.contains_key(&asset_id);
    if !file_has_gps && !in_catalog {
        return Ok(ClearOutcome::NothingToClear);
    }
    if file_has_gps {
        if !auto_write {
            return Ok(ClearOutcome::NeedsAutoWrite);
        }
        if write_locked(store, asset_id, raw_path, now_ms, false, true)? == SyncOutcome::NeedsReview
        {
            return Ok(ClearOutcome::HeldForReview);
        }
    }
    store.set_gps(asset_id, None)?;
    Ok(ClearOutcome::Cleared)
}

fn write_locked(
    store: &dyn CatalogStore,
    asset_id: i64,
    raw_path: &Path,
    now_ms: i64,
    force: bool,
    clear_gps: bool,
) -> Result<SyncOutcome, ScentSyncError> {
    let path = xmp_target(raw_path);
    let prior = store.sidecar_state(asset_id)?;
    let mut state = state_for(&path, prior.clone());
    let catalog = catalog_markers(store, asset_id)?;
    let loaded = load(&path)?;

    if let Some(l) = &loaded {
        // `exactly_eq` ignores a position only the file has, which is exactly what a clear must
        // still remove.
        let must_clear = clear_gps && l.markers.gps.is_some();
        if l.markers.exactly_eq(&catalog) && !must_clear {
            seen(&mut state, l, now_ms);
            state.catalog_dirty_since_ms = None;
            state.needs_review = false;
            store.record_sidecar(asset_id, &state)?;
            return Ok(SyncOutcome::InSync);
        }
        let unchanged_externally = prior
            .as_ref()
            .map(|p| hash_is(&p.last_seen_hash, &l.hash) || hash_is(&p.last_written_hash, &l.hash))
            .unwrap_or(false);
        let held = prior.as_ref().is_some_and(|p| p.needs_review);
        if !force && (held || !unchanged_externally) {
            // Changed under us (or already held): never clobber an un-ingested edit.
            state.needs_review = true;
            state.catalog_dirty_since_ms.get_or_insert(now_ms);
            seen(&mut state, l, now_ms);
            store.record_sidecar(asset_id, &state)?;
            return Ok(SyncOutcome::NeedsReview);
        }
    }

    let base = loaded.as_ref().map(|l| l.text.as_str()).unwrap_or(TEMPLATE);
    let mut patch = catalog.to_patch();
    if clear_gps {
        // `Some(None)` removes the `exif:GPS*` properties; the plain patch leaves a file-only
        // position alone.
        patch.gps = Some(None);
    }
    let patched = packet::apply(base, &patch).map_err(|e| ScentSyncError::Xmp {
        path: path.clone(),
        message: e.to_string(),
    })?;
    let io_err = |source| ScentSyncError::Io {
        path: path.clone(),
        source,
    };
    if crate::scruff::is_jpeg(&path) {
        // Splice the packet into the JPEG, never creating the file: when the load saw no packet,
        // stat then read it fresh. Everything outside the APP1-XMP segment is copied verbatim.
        let (original, stamp) = match loaded.as_ref().and_then(|l| l.file.as_ref()) {
            Some(snap) => (snap.bytes.clone(), snap.stamp),
            None => {
                let stamp = stamp_of(&path);
                (fs::read(&path).map_err(io_err)?, stamp)
            }
        };
        let rewritten = nicti_scent::embedded::write_xmp(&original, &patched).map_err(|e| {
            ScentSyncError::Xmp {
                path: path.clone(),
                message: e.to_string(),
            }
        })?;
        replace_jpeg(&path, &rewritten, stamp).map_err(io_err)?;
        // Our own splice moved the file's size, mtime and head-64KB fingerprint (#666). Re-record
        // them, or the next scan re-decodes T0 for a pixel-identical file and a rename before
        // that scan no longer matches by fingerprint. Best effort: a failure only costs that
        // rescan.
        // Only when the row still described the file as it was just before our splice: if another
        // program had re-saved it since the last scan, the row's T0/dimensions/orientation are
        // stale, and refreshing would hide that -- leave the change for the next scan to see.
        let row_matches_pre_splice = match (store.get_asset(asset_id), stamp) {
            (Ok(Some(a)), Some((len, mtime))) => {
                a.size_bytes == len
                    && mtime
                        .duration_since(std::time::UNIX_EPOCH)
                        .is_ok_and(|d| i64::try_from(d.as_secs()).is_ok_and(|s| s == a.mtime_unix))
            }
            _ => false,
        };
        if row_matches_pre_splice {
            if let Ok((size, mtime_unix, fp)) = crate::scruff::file_identity(&path) {
                let _ = store.refresh_file_identity(asset_id, size, mtime_unix, &fp);
            }
        }
    } else {
        atomic_write(&path, patched.as_bytes()).map_err(io_err)?;
    }
    // Hash of the bytes just written -- never a re-read of the file (ADR-0059).
    let written = blake3::hash(patched.as_bytes());
    state.last_written_hash = Some(written.as_bytes().to_vec());
    state.last_seen_hash = state.last_written_hash.clone();
    state.last_seen_mtime_ms = fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|d| trusted_mtime(d.as_millis(), now_ms));
    state.catalog_dirty_since_ms = None;
    state.needs_review = false;
    store.record_sidecar(asset_id, &state)?;
    Ok(SyncOutcome::WroteSidecar)
}

#[cfg(test)]
mod name_paths_tests {
    use super::*;

    fn kw(id: i64, path: &str) -> Keyword {
        Keyword {
            id,
            parent_id: None,
            name: String::new(),
            path: path.into(),
        }
    }

    #[test]
    fn resolves_ancestor_chains_normalises_and_drops_unknown_leaves() {
        let names: HashMap<i64, String> =
            [(1, "Events".into()), (2, " A|B ".into()), (3, "Fox".into())]
                .into_iter()
                .collect();
        let got = name_paths(&[kw(2, "/1/2/"), kw(3, "/3/"), kw(9, "/9/")], &names);
        let want: BTreeSet<Vec<String>> = [
            vec!["Events".to_string(), "A/B".to_string()],
            vec!["Fox".to_string()],
        ]
        .into_iter()
        .collect();
        assert_eq!(got, want);
    }
}
