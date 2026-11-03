//! Copy from a camera card, as a steppable run (#602): the glue between the verified copy engine
//! ([`crate::offering`], #548) and the catalog. The caller (the UI) decides each file's
//! destination-relative name from the naming templates and hands over the plan; this module copies
//! one file per [`CardCopy::step`], then registers the copies with a [`crate::scruff::Ingest`] over
//! the destination root. The card is only ever read.
//!
//! Failures are never folded into a clean-looking total: a refused, unreadable or unverified file
//! is listed in [`CardCopyReport::failed`] with why, and `verify_failed` is counted on its own
//! because it means the destination disk, not the card, misbehaved.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use crate::offering::{offer_file, CardFile, OfferError, Offered};
use crate::scruff::{Ingest, IngestReport};
use crate::{CatalogError, CatalogStore};

/// One planned copy.
#[derive(Debug, Clone)]
pub struct PlannedCopy {
    pub file: CardFile,
    /// Where it goes under the destination root (validated by [`offer_file`]).
    pub dest_rel: PathBuf,
}

#[derive(Debug, Default)]
pub struct CardCopyReport {
    pub copied: u64,
    pub already_there: u64,
    pub bytes_copied: u64,
    /// Failed or refused files and why.
    pub failed: Vec<(PathBuf, String)>,
    /// The subset of `failed` that did not read back identically from the destination.
    pub verify_failed: u64,
    /// Problems listing the card (unreadable folders), carried through so they are not lost.
    pub listing_problems: Vec<String>,
    /// Final paths of everything now on the destination from this run (copied or already there).
    pub destinations: Vec<PathBuf>,
    /// Files on the card that this run does not copy (videos, other RAW formats, anything outside
    /// `DCIM`, links), summarised like `"3 .mov"`. They block `is_clean`: formatting the card would
    /// lose them.
    pub not_considered: Vec<String>,
    /// Files that were never attempted because the run was cancelled.
    pub not_attempted: u64,
    /// What registering the copies in the catalog did (`None` if it did not run).
    pub ingest: Option<IngestReport>,
    /// A catalog error ended the registration step early.
    pub ingest_error: Option<String>,
    /// Copies whose full-file BLAKE3 (already computed to verify the copy) was recorded as the
    /// asset's trusted `content_hash` baseline, so the later bit-rot check (#304) needs no first
    /// hashing pass over them (#659).
    pub baseline_recorded: u64,
    /// A catalog error while recording the baselines. Not a copy problem: the files are copied and
    /// registered, they just keep no baseline until the baseline pass (#386) hashes them.
    pub baseline_error: Option<String>,
}

impl CardCopyReport {
    /// True when every planned file is on the destination and was registered without a failure.
    pub fn is_clean(&self) -> bool {
        self.failed.is_empty()
            && self.not_attempted == 0
            && self.listing_problems.is_empty()
            && self.not_considered.is_empty()
            && self.ingest_error.is_none()
            && self.ingest.as_ref().is_some_and(|i| i.failed.is_empty())
    }
}

enum Phase {
    Copy,
    Ingest(Ingest),
    /// Recording the verified hashes of the copies as content-hash baselines.
    Baseline,
    Done,
}

/// What the verify read of one copy saw: the destination file's size and mtime right after it
/// landed, with its BLAKE3. Registered as a baseline only if the catalog row still has this exact
/// size and mtime (`record_baseline_hashes`), so a file changed since never gets a stale hash.
struct VerifiedCopy {
    dest: PathBuf,
    size: u64,
    mtime_unix: i64,
    blake3: String,
}

fn mtime_unix(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub struct CardCopy {
    plan: std::vec::IntoIter<PlannedCopy>,
    total_files: u64,
    total_bytes: u64,
    done_bytes: u64,
    done_files: u64,
    dest_root: PathBuf,
    root_id: i64,
    phase: Phase,
    verified: Vec<VerifiedCopy>,
    report: CardCopyReport,
}

impl CardCopy {
    /// `root_id` is the catalog root registered for `dest_root`.
    pub fn new(
        plan: Vec<PlannedCopy>,
        listing_problems: Vec<String>,
        dest_root: &Path,
        root_id: i64,
    ) -> Self {
        let total_files = plan.len() as u64;
        let total_bytes = plan.iter().map(|p| p.file.size).sum();
        Self {
            plan: plan.into_iter(),
            total_files,
            total_bytes,
            done_bytes: 0,
            done_files: 0,
            dest_root: dest_root.to_path_buf(),
            root_id,
            phase: Phase::Copy,
            verified: Vec::new(),
            report: CardCopyReport {
                listing_problems,
                ..Default::default()
            },
        }
    }

    /// Records what the survey found on the card that this run will not copy ([`survey_card`]).
    pub fn with_not_considered(mut self, not_considered: Vec<String>) -> Self {
        self.report.not_considered = not_considered;
        self
    }

    /// `(done, total)` in files; the registration step counts after the copies.
    pub fn progress(&self) -> (u64, u64) {
        (self.done_files, self.total_files)
    }

    pub fn bytes_progress(&self) -> (u64, u64) {
        (self.done_bytes, self.total_bytes)
    }

    /// Advances one file (or one registration chunk). `Ok(true)` while there is more to do.
    /// `cancelled` is polled inside the copy.
    pub fn step(
        &mut self,
        store: &dyn CatalogStore,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<bool, CatalogError> {
        match &mut self.phase {
            Phase::Copy => {
                let Some(item) = self.plan.next() else {
                    self.phase = if self.report.destinations.is_empty() {
                        Phase::Done
                    } else {
                        Phase::Ingest(Ingest::new(self.root_id, &self.dest_root))
                    };
                    return Ok(true);
                };
                if cancelled() {
                    self.report.not_attempted += 1 + self.plan.len() as u64;
                    self.plan = Vec::new().into_iter();
                    return Ok(true);
                }
                match offer_file(&item.file.path, &self.dest_root, &item.dest_rel, cancelled) {
                    Ok(Offered::Copied { dest, size, blake3 }) => {
                        self.report.copied += 1;
                        self.report.bytes_copied += size;
                        self.note_verified(&dest, blake3);
                        self.report.destinations.push(dest);
                    }
                    Ok(Offered::AlreadyThere { dest, blake3 }) => {
                        self.report.already_there += 1;
                        self.note_verified(&dest, blake3);
                        self.report.destinations.push(dest);
                    }
                    Ok(Offered::Cancelled) => {
                        self.report.not_attempted += 1 + self.plan.len() as u64;
                        self.plan = Vec::new().into_iter();
                    }
                    Err(e) => {
                        if matches!(e, OfferError::VerifyFailed(_)) {
                            self.report.verify_failed += 1;
                        }
                        self.report
                            .failed
                            .push((item.file.path.clone(), e.to_string()));
                    }
                }
                self.done_files += 1;
                self.done_bytes += item.file.size;
                Ok(true)
            }
            Phase::Ingest(ingest) => match ingest.step(store) {
                Ok(true) => Ok(true),
                Ok(false) => {
                    let Phase::Ingest(ingest) = std::mem::replace(&mut self.phase, Phase::Done)
                    else {
                        unreachable!("matched Ingest above")
                    };
                    self.report.ingest = Some(ingest.into_report());
                    self.phase = Phase::Baseline;
                    Ok(true)
                }
                Err(e) => {
                    self.report.ingest_error = Some(e.to_string());
                    self.phase = Phase::Done;
                    Ok(false)
                }
            },
            Phase::Baseline => {
                self.record_baselines(store);
                self.phase = Phase::Done;
                Ok(false)
            }
            Phase::Done => Ok(false),
        }
    }

    /// Remembers what the verified read of `dest` saw, for [`Self::record_baselines`]. A file that
    /// can't be stat'ed now simply gets no baseline.
    fn note_verified(&mut self, dest: &Path, blake3: String) {
        if let Ok(meta) = std::fs::metadata(dest) {
            self.verified.push(VerifiedCopy {
                dest: dest.to_path_buf(),
                size: meta.len(),
                mtime_unix: mtime_unix(&meta),
                blake3,
            });
        }
    }

    /// Records each verified copy's BLAKE3 as its asset's content-hash baseline (#659): the hash was
    /// computed to prove the copy and is exactly the full-file hash the baseline needs, so the
    /// first bit-rot check does not have to read these files again. Never fails the run.
    fn record_baselines(&mut self, store: &dyn CatalogStore) {
        let mut entries = Vec::with_capacity(self.verified.len());
        for v in std::mem::take(&mut self.verified) {
            let rel = v.dest.strip_prefix(&self.dest_root).unwrap_or(&v.dest);
            let rel = crate::scruff::normalize_rel_path(rel);
            match store.find_asset_by_path(self.root_id, &rel) {
                Ok(Some(asset)) => entries.push((asset.id, v.size, v.mtime_unix, v.blake3)),
                Ok(None) => {} // not registered (ingest skipped or failed it): nothing to attach to
                Err(e) => {
                    self.report.baseline_error = Some(e.to_string());
                    return;
                }
            }
        }
        match store.record_baseline_hashes(self.root_id, &entries) {
            Ok(written) => self.report.baseline_recorded = written,
            Err(e) => self.report.baseline_error = Some(e.to_string()),
        }
    }

    /// The report so far (a cancelled run's partial result).
    pub fn into_report(mut self) -> CardCopyReport {
        self.report.not_attempted += self.plan.len() as u64;
        self.report
    }
}

/// Names that every card or OS puts on a volume and nobody wants copied.
fn is_card_housekeeping(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    name.starts_with('.')
        || matches!(
            lower.as_str(),
            "system volume information"
                | "$recycle.bin"
                | "thumbs.db"
                | "desktop.ini"
                | "found.000"
        )
}

/// What is on the card under `mount` that a copy of `copied` (the files [`crate::offering::list_card_files`]
/// returned) leaves behind, as short strings (`"3 .mov"`, `"2 link(s)"`, `"1 unreadable folder(s)"`).
/// Empty when the card holds nothing else. Housekeeping names are ignored. The walk does not follow
/// links. Reads directory entries only; run it off the UI thread.
pub fn survey_card(mount: &Path, copied: &HashSet<PathBuf>) -> Vec<String> {
    let mut by_ext: BTreeMap<String, u64> = BTreeMap::new();
    let (mut links, mut unreadable) = (0u64, 0u64);
    let walker = walkdir::WalkDir::new(mount).into_iter().filter_entry(|e| {
        e.depth() == 0 || !e.file_name().to_str().is_some_and(is_card_housekeeping)
    });
    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => {
                unreadable += 1;
                continue;
            }
        };
        let ft = entry.file_type();
        if ft.is_symlink() {
            links += 1;
        } else if ft.is_file() && !copied.contains(entry.path()) {
            let ext = entry
                .path()
                .extension()
                .map(|e| format!(".{}", e.to_string_lossy().to_ascii_lowercase()))
                .unwrap_or_else(|| "(no extension)".into());
            *by_ext.entry(ext).or_insert(0) += 1;
        }
    }
    let mut out: Vec<String> = by_ext
        .into_iter()
        .map(|(e, n)| format!("{n} {e}"))
        .collect();
    if links > 0 {
        out.push(format!("{links} link(s)"));
    }
    if unreadable > 0 {
        out.push(format!("{unreadable} unreadable folder(s)"));
    }
    out
}

/// Windows' `canonicalize` returns verbatim paths (`\\?\C:\x`, `\\?\UNC\srv\share`). They must not
/// become the registered root: the catalog matches root strings exactly, so it would not match the
/// same folder typed as `C:\x` (a duplicate root, duplicate assets). Strips the prefix when the rest
/// is an ordinary drive or UNC path; anything else is returned unchanged.
fn simplify_verbatim(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        let b = rest.as_bytes();
        if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
            return PathBuf::from(rest);
        }
    }
    path
}

/// Whether `dest` is on the same volume as the card mounted at `card` (#659): the case a path
/// prefix cannot see, a destination that reaches the card through a mount alias, junction or
/// symlinked folder. Only a card that *is* a volume counts: a plain folder is on whatever volume
/// holds it, which is no reason to refuse a sibling.
///
/// Unix compares the device of both and requires the card to be a mount point (its device differs
/// from its parent's). Windows compares the volume identity keys ([`crate::volume::identify`]).
/// `false` whenever either side can't be established, so a failed lookup never blocks a copy that
/// the path check allowed.
fn on_card_volume(dest: &Path, card: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let (Ok(d), Ok(c)) = (std::fs::metadata(dest), std::fs::metadata(card)) else {
            return false;
        };
        let card_is_mount_root = card
            .parent()
            .and_then(|p| std::fs::metadata(p).ok())
            .is_none_or(|p| p.dev() != c.dev());
        card_is_mount_root && d.dev() == c.dev()
    }
    #[cfg(windows)]
    {
        match (crate::volume::identify(dest), crate::volume::identify(card)) {
            (Some(d), Some(c)) => d.identity_key == c.identity_key,
            _ => false,
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (dest, card);
        false
    }
}

/// Resolves `dest` and refuses a destination on (or containing) any of the `cards`: copying onto
/// the card would write to the volume the user is about to format, and break the promise that the
/// card is only read. Returns the canonical destination, which is also the spelling to register as
/// the catalog root (so `a/../b` and `b` are one root). Besides the path-prefix test, a destination
/// that reaches the card's volume through an alias is refused too ([`on_card_volume`]).
pub fn check_destination(dest: &Path, cards: &[PathBuf]) -> Result<PathBuf, String> {
    check_destination_with(dest, cards, &on_card_volume)
}

fn check_destination_with(
    dest: &Path,
    cards: &[PathBuf],
    same_volume: &dyn Fn(&Path, &Path) -> bool,
) -> Result<PathBuf, String> {
    let dest = simplify_verbatim(
        dest.canonicalize()
            .map_err(|e| format!("{} can't be used: {e}", dest.display()))?,
    );
    if !dest.is_dir() {
        return Err(format!("{} is not a folder.", dest.display()));
    }
    for card in cards {
        let Ok(card) = card.canonicalize().map(simplify_verbatim) else {
            continue;
        };
        if dest.starts_with(&card) || card.starts_with(&dest) {
            return Err(format!(
                "{} is on (or contains) the card {}; choose a folder on another drive.",
                dest.display(),
                card.display()
            ));
        }
        if same_volume(&dest, &card) {
            return Err(format!(
                "{} is on the same drive as the card {} (reached through a link or mount); \
                 choose a folder on another drive.",
                dest.display(),
                card.display()
            ));
        }
    }
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn done_with_ingest() -> CardCopyReport {
        CardCopyReport {
            copied: 1,
            ingest: Some(IngestReport::default()),
            ..Default::default()
        }
    }

    #[test]
    fn a_clean_report_needs_every_clause_and_each_clause_alone_makes_it_unclean() {
        assert!(done_with_ingest().is_clean());
        let mut r = done_with_ingest();
        r.ingest = None;
        assert!(!r.is_clean(), "ingest never ran");
        let mut r = done_with_ingest();
        r.ingest
            .as_mut()
            .unwrap()
            .failed
            .push((PathBuf::from("a"), "x".into()));
        assert!(!r.is_clean(), "ingest failure");
        let mut r = done_with_ingest();
        r.ingest_error = Some("db".into());
        assert!(!r.is_clean());
        let mut r = done_with_ingest();
        r.not_attempted = 1;
        assert!(!r.is_clean());
        let mut r = done_with_ingest();
        r.listing_problems.push("p".into());
        assert!(!r.is_clean());
        let mut r = done_with_ingest();
        r.not_considered.push("3 .mov".into());
        assert!(!r.is_clean(), "videos left on the card");
        let mut r = done_with_ingest();
        r.failed.push((PathBuf::from("a"), "x".into()));
        assert!(!r.is_clean());
    }

    #[test]
    fn the_survey_names_what_a_copy_leaves_behind_and_ignores_housekeeping() {
        let tmp = tempfile::tempdir().unwrap();
        let card = tmp.path();
        let w = |rel: &str| {
            let p = card.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, b"x").unwrap();
            p
        };
        let nef = w("DCIM/100NIKON/a.NEF");
        w("DCIM/100NIKON/b.MOV");
        w("DCIM/100NIKON/c.MOV");
        w("MISC/d.NEV");
        w("System Volume Information/x.dat");
        w(".Trashes/y");
        let copied: HashSet<PathBuf> = [nef].into_iter().collect();
        let found = survey_card(card, &copied);
        assert_eq!(found, vec!["2 .mov".to_string(), "1 .nev".to_string()]);
        // Nothing left behind: empty.
        let all: HashSet<PathBuf> = [
            card.join("DCIM/100NIKON/a.NEF"),
            card.join("DCIM/100NIKON/b.MOV"),
            card.join("DCIM/100NIKON/c.MOV"),
            card.join("MISC/d.NEV"),
        ]
        .into_iter()
        .collect();
        assert!(survey_card(card, &all).is_empty());
    }

    #[test]
    fn verbatim_prefixes_are_stripped_only_for_plain_drive_and_unc_paths() {
        let f = |s: &str| simplify_verbatim(PathBuf::from(s));
        assert_eq!(f(r"\\?\C:\Users\x"), PathBuf::from(r"C:\Users\x"));
        assert_eq!(f(r"\\?\UNC\srv\share\x"), PathBuf::from(r"\\srv\share\x"));
        assert_eq!(
            f(r"\\?\Volume{1234}\x"),
            PathBuf::from(r"\\?\Volume{1234}\x")
        );
        assert_eq!(f("/plain/path"), PathBuf::from("/plain/path"));
    }

    #[test]
    fn a_destination_on_the_card_or_containing_it_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let card = tmp.path().join("card");
        let inside = card.join("copies");
        let elsewhere = tmp.path().join("photos");
        for d in [&inside, &elsewhere] {
            std::fs::create_dir_all(d).unwrap();
        }
        let cards = vec![card.clone()];
        assert!(check_destination(&inside, &cards).is_err());
        assert!(check_destination(&card, &cards).is_err());
        assert!(
            check_destination(tmp.path(), &cards).is_err(),
            "contains the card"
        );
        let ok = check_destination(&elsewhere.join("../photos"), &cards).unwrap();
        assert_eq!(ok, simplify_verbatim(elsewhere.canonicalize().unwrap()));
        assert!(check_destination(&tmp.path().join("missing"), &cards).is_err());
    }

    #[test]
    fn a_destination_on_the_cards_volume_is_refused_even_without_a_path_relation() {
        let tmp = tempfile::tempdir().unwrap();
        let card = tmp.path().join("card");
        let alias = tmp.path().join("elsewhere");
        let fine = tmp.path().join("fine");
        for d in [&card, &alias, &fine] {
            std::fs::create_dir_all(d).unwrap();
        }
        let cards = vec![card];
        // The comparator is injected: a real second volume can't be made in a unit test.
        let same = |d: &Path, _: &Path| d.ends_with("elsewhere");
        let err = check_destination_with(&alias, &cards, &same).unwrap_err();
        assert!(err.contains("same drive"), "{err}");
        assert!(check_destination_with(&fine, &cards, &same).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn on_card_volume_needs_a_card_that_is_a_mount_root() {
        let tmp = tempfile::tempdir().unwrap();
        let (card, dest) = (tmp.path().join("card"), tmp.path().join("dest"));
        std::fs::create_dir_all(&card).unwrap();
        std::fs::create_dir_all(&dest).unwrap();
        // A plain folder is not a volume: its sibling is not "on the card".
        assert!(!on_card_volume(&dest, &card));
        // The filesystem root is one, and anything on the same device is on it.
        assert!(on_card_volume(Path::new("/"), Path::new("/")));
        // A missing path can't be established, so it never blocks.
        assert!(!on_card_volume(&tmp.path().join("missing"), Path::new("/")));
    }
}
