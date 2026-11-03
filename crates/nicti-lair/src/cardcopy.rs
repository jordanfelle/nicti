//! Copy from a camera card, as a steppable run (#602): the glue between the verified copy engine
//! ([`crate::offering`], #548) and the catalog. The caller (the UI) decides each file's
//! destination-relative name from the naming templates and hands over the plan; this module copies
//! one file per [`CardCopy::step`], then registers the copies with a [`crate::scruff::Ingest`] over
//! the destination root. The card is only ever read.
//!
//! Failures are never folded into a clean-looking total: a refused, unreadable or unverified file
//! is listed in [`CardCopyReport::failed`] with why, and `verify_failed` is counted on its own
//! because it means the destination disk, not the card, misbehaved.

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
    /// Files that were never attempted because the run was cancelled.
    pub not_attempted: u64,
    /// What registering the copies in the catalog did (`None` if it did not run).
    pub ingest: Option<IngestReport>,
    /// A catalog error ended the registration step early.
    pub ingest_error: Option<String>,
}

impl CardCopyReport {
    /// True when every planned file is on the destination and was registered without a failure.
    pub fn is_clean(&self) -> bool {
        self.failed.is_empty()
            && self.not_attempted == 0
            && self.listing_problems.is_empty()
            && self.ingest_error.is_none()
            && self.ingest.as_ref().is_some_and(|i| i.failed.is_empty())
    }
}

enum Phase {
    Copy,
    Ingest(Ingest),
    Done,
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
            report: CardCopyReport {
                listing_problems,
                ..Default::default()
            },
        }
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
                    Ok(Offered::Copied { dest, size, .. }) => {
                        self.report.copied += 1;
                        self.report.bytes_copied += size;
                        self.report.destinations.push(dest);
                    }
                    Ok(Offered::AlreadyThere { dest, .. }) => {
                        self.report.already_there += 1;
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
                    Ok(false)
                }
                Err(e) => {
                    self.report.ingest_error = Some(e.to_string());
                    self.phase = Phase::Done;
                    Ok(false)
                }
            },
            Phase::Done => Ok(false),
        }
    }

    /// The report so far (a cancelled run's partial result).
    pub fn into_report(mut self) -> CardCopyReport {
        self.report.not_attempted += self.plan.len() as u64;
        self.report
    }
}
