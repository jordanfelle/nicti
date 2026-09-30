//! Runs a paste/sync/preset against the catalog, and undoes it (#52).
//!
//! Undo is session-local: `LastBatch` keeps each changed photo's previous document in memory until
//! the next batch replaces it. It stands in until #324 wires real History into Develop.

use nicti_lair::{CatalogError, CatalogStore};
use nicti_pawprint::EditDocument;

use super::{plan, Change, Clipboard};

/// What a batch did, for the one summary line (ADR-0101 rule 5: no per-photo prompts).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct BatchOutcome {
    pub applied: usize,
    pub unchanged: usize,
    pub missing: usize,
}

/// What an undo did. `skipped` are photos edited since the batch -- left exactly as they are.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct UndoOutcome {
    pub restored: usize,
    pub skipped: usize,
}

/// The last batch, enough to reverse it.
#[derive(Debug)]
pub struct LastBatch {
    pub label: String,
    entries: Vec<(Change, blake3::Hash)>,
}

impl LastBatch {
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn asset_ids(&self) -> impl Iterator<Item = i64> + '_ {
        self.entries.iter().map(|(c, _)| c.asset_id)
    }

    /// Puts every photo back to its pre-batch document -- except one whose document is no longer
    /// the one this batch wrote, which was edited since and is skipped rather than overwritten.
    pub fn undo(&self, store: &dyn CatalogStore) -> Result<UndoOutcome, CatalogError> {
        let ids: Vec<i64> = self.asset_ids().collect();
        let current = store.get_master_edits(&ids)?;
        let mut restore: Vec<(i64, EditDocument)> = Vec::new();
        for ((change, after_hash), (_, now)) in self.entries.iter().zip(current) {
            let untouched = now
                .as_ref()
                .and_then(|d| d.content_hash().ok())
                .is_some_and(|h| h == *after_hash);
            if untouched {
                restore.push((change.asset_id, change.before.clone()));
            }
        }
        store.put_master_edits(&restore)?;
        Ok(UndoOutcome {
            restored: restore.len(),
            skipped: self.entries.len() - restore.len(),
        })
    }
}

/// Reads every target's document, plans the paste, and writes all the changes in one transaction.
/// Returns `None` for the undo handle when nothing changed. Nothing is written if the write fails.
pub fn run_batch(
    store: &dyn CatalogStore,
    clip: &Clipboard,
    ids: &[i64],
    label: &str,
) -> Result<(BatchOutcome, Option<LastBatch>), CatalogError> {
    let batch = plan(clip, store.get_master_edits(ids)?);
    let writes: Vec<(i64, EditDocument)> = batch
        .changes
        .iter()
        .map(|c| (c.asset_id, c.after.clone()))
        .collect();
    store.put_master_edits(&writes)?;

    let outcome = BatchOutcome {
        applied: batch.changes.len(),
        unchanged: batch.unchanged,
        missing: batch.missing,
    };
    if batch.changes.is_empty() {
        return Ok((outcome, None));
    }
    let entries = batch
        .changes
        .into_iter()
        .map(|c| {
            // The hash was just serialised by the write above, so it can't fail here.
            let hash = c
                .after
                .content_hash()
                .map_err(|e| CatalogError::Document(e.to_string()))?;
            Ok((c, hash))
        })
        .collect::<Result<Vec<_>, CatalogError>>()?;
    Ok((
        outcome,
        Some(LastBatch {
            label: label.to_string(),
            entries,
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knead::StageSet;
    use nicti_lair::SqliteCatalog;
    use nicti_pawprint::StageEntry;
    use nicti_tapetum::stages::EXPOSURE;
    use serde_json::json;

    fn doc(v: f64) -> EditDocument {
        let mut d = EditDocument::default();
        d.stages.insert(
            EXPOSURE.to_string(),
            StageEntry {
                schema_version: 1,
                params: json!({ "ev": v }),
            },
        );
        d
    }

    /// `n` photos, each with exposure `ev = 0.0`.
    fn catalog(n: usize) -> (SqliteCatalog, Vec<i64>) {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let ids = crate::test_support::seed_assets(&store, n);
        store
            .put_master_edits(&ids.iter().map(|&i| (i, doc(0.0))).collect::<Vec<_>>())
            .unwrap();
        (store, ids)
    }

    fn clip(v: f64) -> Clipboard {
        Clipboard::from_document(&doc(v), &StageSet::all())
    }

    #[test]
    fn a_batch_writes_every_changed_photo_and_counts_the_rest() {
        let (store, ids) = catalog(3);
        store
            .put_master_edit(ids[1], &clip(2.0).apply(&doc(0.0)))
            .unwrap();
        let mut with_missing = ids.clone();
        with_missing.push(999_999);

        let (out, last) = run_batch(&store, &clip(2.0), &with_missing, "Sync").unwrap();
        assert_eq!(
            out,
            BatchOutcome {
                applied: 2,
                unchanged: 1,
                missing: 1
            }
        );
        assert_eq!(
            last.unwrap().len(),
            2,
            "the unchanged photo gets no undo entry"
        );
        for id in ids {
            assert_eq!(store.get_master_edit(id).unwrap(), Some(doc(2.0)));
        }
    }

    #[test]
    fn a_batch_that_changes_nothing_has_no_undo_handle() {
        let (store, ids) = catalog(2);
        let (out, last) = run_batch(&store, &clip(0.0), &ids, "Paste").unwrap();
        assert_eq!(
            out,
            BatchOutcome {
                applied: 0,
                unchanged: 2,
                missing: 0
            }
        );
        assert!(last.is_none());
    }

    #[test]
    fn undo_restores_the_previous_documents() {
        let (store, ids) = catalog(3);
        let (_, last) = run_batch(&store, &clip(2.0), &ids, "Sync").unwrap();
        let out = last.unwrap().undo(&store).unwrap();
        assert_eq!(
            out,
            UndoOutcome {
                restored: 3,
                skipped: 0
            }
        );
        for id in ids {
            assert_eq!(store.get_master_edit(id).unwrap(), Some(doc(0.0)));
        }
    }

    #[test]
    fn undo_skips_a_photo_edited_since_the_batch() {
        let (store, ids) = catalog(3);
        let (_, last) = run_batch(&store, &clip(2.0), &ids, "Sync").unwrap();
        store.put_master_edit(ids[1], &doc(7.0)).unwrap();

        let out = last.unwrap().undo(&store).unwrap();
        assert_eq!(
            out,
            UndoOutcome {
                restored: 2,
                skipped: 1
            }
        );
        assert_eq!(store.get_master_edit(ids[1]).unwrap(), Some(doc(7.0)));
        assert_eq!(store.get_master_edit(ids[0]).unwrap(), Some(doc(0.0)));
    }

    #[test]
    fn a_thousand_photos_batch_quickly() {
        let (store, ids) = catalog(1000);
        let start = std::time::Instant::now();
        let (out, _) = run_batch(&store, &clip(2.0), &ids, "Sync").unwrap();
        assert_eq!(out.applied, 1000);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "took {:?}",
            start.elapsed()
        );
    }
}
