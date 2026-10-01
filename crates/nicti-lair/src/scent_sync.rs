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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Markers {
    /// `None` = unrated, `Some(-1)` = reject, `Some(0..=5)` = stars.
    pub rating: Option<i8>,
    pub label: Option<String>,
    pub pick: bool,
    /// Each keyword as its name path (`["Events", "Anthrocon"]`); flat keywords are 1-segment.
    pub keywords: BTreeSet<Vec<String>>,
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

impl Markers {
    fn is_empty(&self) -> bool {
        *self == Markers::default()
    }

    fn hash(&self) -> blake3::Hash {
        let mut h = blake3::Hasher::new();
        h.update(format!("{:?}|{:?}|{}", self.rating, self.label, self.pick).as_bytes());
        for kw in &self.keywords {
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

fn keyword_name_paths(
    store: &dyn CatalogStore,
    asset_id: i64,
) -> Result<BTreeSet<Vec<String>>, CatalogError> {
    let tagged = store.keywords_for(asset_id)?;
    if tagged.is_empty() {
        return Ok(BTreeSet::new());
    }
    let names: HashMap<i64, String> = store
        .list_keywords()?
        .into_iter()
        .map(|k| (k.id, k.name))
        .collect();
    Ok(tagged
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
        .collect())
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
    })
}

fn apply_to_catalog(
    store: &dyn CatalogStore,
    asset_id: i64,
    m: &Markers,
) -> Result<(), ScentSyncError> {
    store.set_meta(&[(
        asset_id,
        AssetMeta {
            rating: m.rating.map(i64::from),
            flag: m.pick.then_some(1),
            label: m.label.clone(),
        },
    )])?;

    // Resolve (creating as needed) each wanted path down to its leaf keyword id.
    let mut wanted: BTreeSet<i64> = BTreeSet::new();
    for path in &m.keywords {
        let mut parent: Option<i64> = None;
        for i in 0..path.len() {
            let segs: Vec<&str> = path[..=i].iter().map(String::as_str).collect();
            let kw: Keyword = match store.keyword_by_path(&segs)? {
                Some(k) => k,
                None => {
                    let id = store.create_keyword(parent, &path[i])?;
                    Keyword {
                        id,
                        parent_id: parent,
                        name: path[i].clone(),
                        path: String::new(),
                    }
                }
            };
            parent = Some(kw.id);
        }
        if let Some(leaf) = parent {
            wanted.insert(leaf);
        }
    }
    let current: BTreeSet<i64> = store.keywords_for(asset_id)?.iter().map(|k| k.id).collect();
    for id in current.difference(&wanted) {
        store.untag(&[asset_id], *id)?;
    }
    for id in wanted.difference(&current) {
        store.tag(&[asset_id], *id)?;
    }
    Ok(())
}

struct Loaded {
    text: String,
    hash: blake3::Hash,
    mtime_ms: u128,
    markers: Markers,
}

/// Reads and parses the sidecar; `Ok(None)` only when the file genuinely doesn't exist. A read or
/// parse failure is an error, never "blank" -- so a half-written sidecar can't be overwritten.
fn load(path: &Path) -> Result<Option<Loaded>, ScentSyncError> {
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
    let text = String::from_utf8(bytes).map_err(|e| ScentSyncError::Xmp {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;
    let meta = lrc_fields::read(&text).map_err(|e| ScentSyncError::Xmp {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;
    Ok(Some(Loaded {
        text,
        hash,
        mtime_ms,
        markers: Markers::from_lrc(&meta),
    }))
}

fn state_for(path: &Path, prior: Option<SidecarState>) -> SidecarState {
    let mut s = prior.unwrap_or_default();
    s.path = path.to_string_lossy().into_owned();
    s
}

fn seen(s: &mut SidecarState, l: &Loaded) {
    s.last_seen_hash = Some(l.hash.as_bytes().to_vec());
    s.last_seen_mtime_ms = i64::try_from(l.mtime_ms).ok();
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
    let _guard = SYNC_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let path = sidecar_path(raw_path);
    let prior = store.sidecar_state(asset_id)?;
    // Cheap precheck for rescans of a large library: a stat, not a read + hash, when the sidecar's
    // mtime is exactly what nicti last recorded and nothing is pending.
    // A dirty catalog (auto-write off, or a write that failed) must never take either early return:
    // the newer-wins comparison below is what pushes its markers to the sidecar.
    if let Some(p) = prior
        .as_ref()
        .filter(|p| p.catalog_dirty_since_ms.is_none())
    {
        if !p.needs_review {
            let on_disk = fs::metadata(&path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .and_then(|d| i64::try_from(d.as_millis()).ok());
            if on_disk.is_some() && on_disk == p.last_seen_mtime_ms {
                return Ok(SyncOutcome::InSync);
            }
        }
    }
    let Some(loaded) = load(&path)? else {
        return Ok(SyncOutcome::NoSidecar);
    };
    if let Some(p) = prior
        .as_ref()
        .filter(|p| p.catalog_dirty_since_ms.is_none())
    {
        // Unchanged since nicti last looked, and nothing pending: nothing to do.
        if hash_is(&p.last_seen_hash, &loaded.hash) && !p.needs_review {
            return Ok(SyncOutcome::InSync);
        }
    }
    let catalog = catalog_markers(store, asset_id)?;
    let mut state = state_for(&path, prior.clone());
    seen(&mut state, &loaded);

    if catalog == loaded.markers {
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
    let resolution = match (&prior, state.catalog_dirty_since_ms) {
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
            write_locked(store, asset_id, raw_path, now_ms, true)
        }
        Resolution::FlagForManualReview => {
            state.needs_review = true;
            state.catalog_dirty_since_ms.get_or_insert(now_ms);
            store.record_sidecar(asset_id, &state)?;
            Ok(SyncOutcome::NeedsReview)
        }
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
    write_locked(store, asset_id, raw_path, now_ms, false)
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
    let path = sidecar_path(raw_path);
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
        let path = sidecar_path(raw_path);
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
        return write_locked(store, asset_id, raw_path, now_ms, true);
    }
    let path = sidecar_path(raw_path);
    let Some(loaded) = load(&path)? else {
        return Ok(SyncOutcome::NoSidecar);
    };
    apply_to_catalog(store, asset_id, &loaded.markers)?;
    let mut state = state_for(&path, store.sidecar_state(asset_id)?);
    seen(&mut state, &loaded);
    state.catalog_dirty_since_ms = None;
    state.needs_review = false;
    store.record_sidecar(asset_id, &state)?;
    Ok(SyncOutcome::AppliedSidecar)
}

fn write_locked(
    store: &dyn CatalogStore,
    asset_id: i64,
    raw_path: &Path,
    now_ms: i64,
    force: bool,
) -> Result<SyncOutcome, ScentSyncError> {
    let path = sidecar_path(raw_path);
    let prior = store.sidecar_state(asset_id)?;
    let mut state = state_for(&path, prior.clone());
    let catalog = catalog_markers(store, asset_id)?;
    let loaded = load(&path)?;

    if let Some(l) = &loaded {
        if l.markers == catalog {
            seen(&mut state, l);
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
            seen(&mut state, l);
            store.record_sidecar(asset_id, &state)?;
            return Ok(SyncOutcome::NeedsReview);
        }
    }

    let base = loaded.as_ref().map(|l| l.text.as_str()).unwrap_or(TEMPLATE);
    let patched = packet::apply(base, &catalog.to_patch()).map_err(|e| ScentSyncError::Xmp {
        path: path.clone(),
        message: e.to_string(),
    })?;
    atomic_write(&path, patched.as_bytes()).map_err(|source| ScentSyncError::Io {
        path: path.clone(),
        source,
    })?;
    // Hash of the bytes just written -- never a re-read of the file (ADR-0059).
    let written = blake3::hash(patched.as_bytes());
    state.last_written_hash = Some(written.as_bytes().to_vec());
    state.last_seen_hash = state.last_written_hash.clone();
    state.last_seen_mtime_ms = fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|d| i64::try_from(d.as_millis()).ok());
    state.catalog_dirty_since_ms = None;
    state.needs_review = false;
    store.record_sidecar(asset_id, &state)?;
    Ok(SyncOutcome::WroteSidecar)
}
