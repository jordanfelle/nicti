//! Runs a paste/sync/preset against the catalog, and undoes it (#52).
//!
//! Undo is session-local: `LastBatch` keeps each changed photo's previous document in memory until
//! the next batch replaces it. It is a catalog-wide undo across photos; Develop's own per-photo History (#324) covers one open photo.

use nicti_lair::{CatalogError, CatalogStore};
use nicti_pawprint::EditDocument;

use super::straighten::{plan_rotation, Mode, Target};
use super::{plan, BatchPlan, Change, Clipboard};

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

/// The hash `undo` will see for `doc` once it's in the catalog: hashed after a round trip through
/// the stored JSON, not from the in-memory value. `serde_json`'s default float parsing isn't
/// guaranteed to return the exact bits it was given, so hashing `doc` directly could differ by an
/// ULP from what a later read parses, and undo would skip an untouched photo. Both sides go
/// through the same parse, so they always agree.
fn stored_hash(doc: &EditDocument) -> Result<blake3::Hash, CatalogError> {
    let bad = |e: String| CatalogError::Document(e);
    let json = nicti_pawprint::to_canonical_json(doc).map_err(|e| bad(e.to_string()))?;
    let as_stored: EditDocument = serde_json::from_str(&json).map_err(|e| bad(e.to_string()))?;
    as_stored.content_hash().map_err(|e| bad(e.to_string()))
}

/// Reads every target's document, plans the paste, and writes all the changes in one transaction.
/// Returns `None` for the undo handle when nothing changed. Nothing is written if the write fails.
pub fn run_batch(
    store: &dyn CatalogStore,
    clip: &Clipboard,
    ids: &[i64],
    label: &str,
) -> Result<(BatchOutcome, Option<LastBatch>), CatalogError> {
    // A photo listed twice would get two changes and two undo entries.
    let mut seen = std::collections::HashSet::new();
    let ids: Vec<i64> = ids.iter().copied().filter(|id| seen.insert(*id)).collect();
    commit(store, plan(clip, store.get_master_edits(&ids)?), label)
}

/// Writes a plan's changes in one transaction and builds the undo handle (`None` when nothing
/// changed). Nothing is written if the write fails.
fn commit(
    store: &dyn CatalogStore,
    batch: BatchPlan,
    label: &str,
) -> Result<(BatchOutcome, Option<LastBatch>), CatalogError> {
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
            let hash = stored_hash(&c.after)?;
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

/// The rotation-only batch (#614): sets or adds `degrees` to each target's straighten angle, for
/// photos whose EXIF Orientation matches `reference_orientation`. Returns how many were skipped
/// for an unknown or different orientation alongside the usual outcome and undo handle.
pub fn run_rotation_batch(
    store: &dyn CatalogStore,
    ids: &[i64],
    reference_orientation: Option<u8>,
    mode: Mode,
    degrees: f32,
    label: &str,
) -> Result<(BatchOutcome, usize, Option<LastBatch>), CatalogError> {
    let mut seen = std::collections::HashSet::new();
    let ids: Vec<i64> = ids.iter().copied().filter(|id| seen.insert(*id)).collect();
    let targets: Vec<Target> = store
        .get_master_edits(&ids)?
        .into_iter()
        .map(|(asset_id, doc)| {
            let orientation = store.get_asset(asset_id)?.and_then(|a| a.orientation);
            Ok(Target {
                asset_id,
                doc,
                orientation,
            })
        })
        .collect::<Result<_, CatalogError>>()?;
    let rotation = plan_rotation(targets, reference_orientation, mode, degrees);
    let skipped = rotation.orientation_skipped;
    let (outcome, last) = commit(store, rotation.plan, label)?;
    Ok((outcome, skipped, last))
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
    fn a_thousand_photos_are_one_batch_and_one_undo() {
        let (store, ids) = catalog(1000);
        let (out, last) = run_batch(&store, &clip(2.0), &ids, "Sync").unwrap();
        assert_eq!(out.applied, 1000);
        let undo = last.unwrap().undo(&store).unwrap();
        assert_eq!(undo.restored, 1000);
    }

    #[test]
    fn a_photo_listed_twice_is_changed_and_undone_once() {
        let (store, ids) = catalog(2);
        let doubled = [ids[0], ids[1], ids[0]];
        let (out, last) = run_batch(&store, &clip(2.0), &doubled, "Paste").unwrap();
        assert_eq!(out.applied, 2);
        assert_eq!(last.unwrap().len(), 2);
    }

    fn rotation(store: &SqliteCatalog, id: i64) -> f32 {
        use nicti_tapetum::coat::{self, CropParams};
        store
            .get_master_edit(id)
            .unwrap()
            .and_then(|d| d.stages.get(nicti_tapetum::stages::CROP).cloned())
            .map_or(0.0, |e| {
                coat::parse::<CropParams>(&e.params).rotation_degrees
            })
    }

    #[test]
    fn a_rotation_batch_sets_the_angle_skips_other_orientations_and_undoes_as_one() {
        let (store, ids) = catalog(4);
        for (id, o) in [(ids[0], 1), (ids[1], 1), (ids[2], 6)] {
            store.set_orientation(id, o).unwrap();
        } // ids[3] was never read
        let (out, skipped, last) =
            run_rotation_batch(&store, &ids, Some(1), Mode::Set, 2.5, "Straighten").unwrap();
        assert_eq!((out.applied, out.unchanged, out.missing), (2, 0, 0));
        assert_eq!(
            skipped, 2,
            "a sideways photo and an unread one are left alone"
        );
        assert!((rotation(&store, ids[0]) - 2.5).abs() < 1e-6);
        assert!((rotation(&store, ids[1]) - 2.5).abs() < 1e-6);
        assert_eq!(rotation(&store, ids[2]), 0.0);
        assert_eq!(rotation(&store, ids[3]), 0.0);

        let undo = last.unwrap().undo(&store).unwrap();
        assert_eq!(undo.restored, 2);
        assert_eq!(rotation(&store, ids[0]), 0.0);
        assert_eq!(store.get_master_edit(ids[0]).unwrap(), Some(doc(0.0)));
    }

    #[test]
    fn running_the_same_rotation_again_changes_nothing_and_keeps_the_old_undo_valid() {
        let (store, ids) = catalog(2);
        for id in &ids {
            store.set_orientation(*id, 1).unwrap();
        }
        let (_, _, first) =
            run_rotation_batch(&store, &ids, Some(1), Mode::Set, 1.0, "Straighten").unwrap();
        assert!(first.is_some());
        let (out, _, second) =
            run_rotation_batch(&store, &ids, Some(1), Mode::Set, 1.0, "Straighten").unwrap();
        assert_eq!((out.applied, out.unchanged), (0, 2));
        assert!(second.is_none());
    }
}
