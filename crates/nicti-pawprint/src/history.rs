//! Per-photo edit history: an append-only delta log plus named snapshots, with compaction that
//! coalesces consecutive same-control deltas (a slider drag) into one step without losing the
//! pre-drag undo target. A bulk paste/sync (#52) applies as one `Delta` covering every stage it
//! touched, so it's genuinely one history entry per photo, and undoes as one step by construction
//! rather than by grouping several log entries back together. Promoted from `spikes/pawprint`.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use uuid::Uuid;

use crate::{EditDocument, StageEntry};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StageChange {
    pub stage_id: String,
    pub before: Option<StageEntry>,
    pub after: Option<StageEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Delta {
    pub batch_id: Uuid,
    pub changes: Vec<StageChange>,
    /// Coalescing key for compaction (e.g. `"exposure_slider"`). `None` for multi-stage batch
    /// operations -- those are never merged by `compact`, only single-stage single-control deltas
    /// are (a slider drag).
    pub control: Option<String>,
    pub timestamp_ms: u128,
}

#[derive(Debug, Clone, Serialize)]
enum LogEntry {
    Delta(Delta),
    /// A named marker over the document state at this point. Doesn't itself change the document
    /// (it's captured for reference/rollback by name, not replayed), so it costs nothing in
    /// undo/redo and is never merged away by `compact`.
    Snapshot {
        name: String,
        document: EditDocument,
    },
}

pub struct History {
    document: EditDocument,
    log: Vec<LogEntry>,
    /// One past the most recently applied entry; `log[cursor..]` is the redo stack.
    cursor: usize,
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

impl History {
    pub fn new(document: EditDocument) -> Self {
        Self {
            document,
            log: Vec::new(),
            cursor: 0,
        }
    }

    pub fn document(&self) -> &EditDocument {
        &self.document
    }

    /// Log length, for tests asserting a batch collapsed to N entries.
    pub fn len(&self) -> usize {
        self.log.len()
    }

    pub fn is_empty(&self) -> bool {
        self.log.is_empty()
    }

    /// Whether `undo` would change the document (a snapshot marker has nothing to undo).
    pub fn can_undo(&self) -> bool {
        self.log[..self.cursor]
            .iter()
            .any(|e| matches!(e, LogEntry::Delta(_)))
    }

    /// Whether `redo` would change the document.
    pub fn can_redo(&self) -> bool {
        self.log[self.cursor..]
            .iter()
            .any(|e| matches!(e, LogEntry::Delta(_)))
    }

    /// Approximate on-disk size of the log as it stands, by actually serializing it (not a
    /// guessed per-entry constant) -- used for the ADR's sizing table.
    pub fn serialized_len(&self) -> usize {
        serde_json::to_vec(&self.log).map(|b| b.len()).unwrap_or(0)
    }

    /// Apply one stage change, tagged with a coalescing `control` key. A slider drag calls this
    /// once per tick; `compact()` later merges the run into a single entry.
    pub fn apply(&mut self, stage_id: &str, control: &str, after: StageEntry) {
        self.apply_at(stage_id, control, after, now_ms());
    }

    /// Same as `apply`, but with an explicit timestamp instead of the wall clock -- lets tests
    /// simulate a real gap between two edit sessions (e.g. "pre-drag baseline" vs. "the drag
    /// itself") without sleeping.
    pub fn apply_at(
        &mut self,
        stage_id: &str,
        control: &str,
        after: StageEntry,
        timestamp_ms: u128,
    ) {
        self.push_delta(
            Uuid::new_v4(),
            vec![(stage_id.to_string(), Some(after))],
            Some(control),
            timestamp_ms,
        );
    }

    /// Remove a stage's entry (back to its default) as one history step -- a slider's
    /// double-click reset. A no-op, creating no step, when the stage already has no entry.
    pub fn reset(&mut self, stage_id: &str, control: &str) {
        self.push_delta(
            Uuid::new_v4(),
            vec![(stage_id.to_string(), None)],
            Some(control),
            now_ms(),
        );
    }

    /// Apply several stage changes as a single history entry -- bulk paste/sync (#52), so the
    /// whole batch is one entry per photo (not one per changed stage) and undoes/redoes
    /// atomically by construction.
    pub fn apply_batch(&mut self, batch_id: Uuid, changes: Vec<(String, StageEntry)>) {
        let changes = changes.into_iter().map(|(id, e)| (id, Some(e))).collect();
        self.push_delta(batch_id, changes, None, now_ms());
    }

    /// [`Self::apply_batch`] with a fresh batch id, for a caller with no batch identity of its own
    /// (an interactive multi-stage edit such as Auto tone).
    /// A `None` entry removes that stage (back to its default), so a reset of several stages is
    /// one step too.
    pub fn apply_group(&mut self, changes: Vec<(String, Option<StageEntry>)>) {
        self.push_delta(Uuid::new_v4(), changes, None, now_ms());
    }

    /// ADR-0101 rule 6 (#312): a delta whose every change has `before == after` appends no step and
    /// leaves the redo tail alone. Comparison is strict (`before` is `None` for an absent stage), so
    /// the schema-aware caller resolves an absent entry to its stage default first -- this crate
    /// doesn't know the defaults.
    fn push_delta(
        &mut self,
        batch_id: Uuid,
        changes: Vec<(String, Option<StageEntry>)>,
        control: Option<&str>,
        timestamp_ms: u128,
    ) {
        let unchanged = changes
            .iter()
            .all(|(id, after)| self.document.stages.get(id) == after.as_ref());
        if unchanged {
            return;
        }
        self.log.truncate(self.cursor);
        let mut recorded = Vec::with_capacity(changes.len());
        for (stage_id, after) in changes {
            let before = self.document.stages.get(&stage_id).cloned();
            match &after {
                Some(entry) => {
                    self.document.stages.insert(stage_id.clone(), entry.clone());
                }
                None => {
                    self.document.stages.remove(&stage_id);
                }
            }
            recorded.push(StageChange {
                stage_id,
                before,
                after,
            });
        }
        self.log.push(LogEntry::Delta(Delta {
            batch_id,
            changes: recorded,
            control: control.map(str::to_string),
            timestamp_ms,
        }));
        self.cursor = self.log.len();
    }

    /// A named, never-pruned marker over the current document state.
    pub fn snapshot(&mut self, name: &str) {
        self.log.truncate(self.cursor);
        self.log.push(LogEntry::Snapshot {
            name: name.to_string(),
            document: self.document.clone(),
        });
        self.cursor = self.log.len();
    }

    pub fn snapshot_names(&self) -> Vec<&str> {
        self.log
            .iter()
            .filter_map(|e| match e {
                LogEntry::Snapshot { name, .. } => Some(name.as_str()),
                LogEntry::Delta(_) => None,
            })
            .collect()
    }

    /// The document state captured under a named snapshot, for viewing or restoring "revert to
    /// this named snapshot" without walking undo one step at a time.
    pub fn snapshot_document(&self, name: &str) -> Option<&EditDocument> {
        self.log.iter().find_map(|e| match e {
            LogEntry::Snapshot { name: n, document } if n == name => Some(document),
            _ => None,
        })
    }

    /// Undo the most recently applied entry (one whole batch, or one compacted slider-drag run --
    /// every entry in the log is already an atomic undo unit, so no cross-entry grouping is
    /// needed here).
    pub fn undo(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let idx = self.cursor - 1;
        if let LogEntry::Delta(d) = &self.log[idx] {
            for change in d.changes.iter().rev() {
                match &change.before {
                    Some(entry) => {
                        self.document
                            .stages
                            .insert(change.stage_id.clone(), entry.clone());
                    }
                    None => {
                        self.document.stages.remove(&change.stage_id);
                    }
                }
            }
        }
        self.cursor = idx;
        true
    }

    pub fn redo(&mut self) -> bool {
        if self.cursor >= self.log.len() {
            return false;
        }
        let idx = self.cursor;
        if let LogEntry::Delta(d) = &self.log[idx] {
            for change in &d.changes {
                match &change.after {
                    Some(entry) => {
                        self.document
                            .stages
                            .insert(change.stage_id.clone(), entry.clone());
                    }
                    None => {
                        self.document.stages.remove(&change.stage_id);
                    }
                }
            }
        }
        self.cursor = idx + 1;
        true
    }

    /// Coalesce consecutive single-stage deltas sharing the same `control` key within `window_ms`
    /// of each other into a single delta spanning the run (first entry's `before`, last entry's
    /// `after`). Only single-stage, `control`-tagged deltas (from `apply`/`apply_at`) are ever
    /// merged -- a batch (`control: None`, possibly multiple stages) never coalesces with
    /// anything, so a bulk paste always stays exactly one entry, and never accidentally absorbs
    /// an unrelated slider tick. Only compacts the applied prefix (`..cursor`) -- refuses if
    /// there's a pending redo, so compaction never corrupts a redo chain the user might still walk
    /// forward into. Snapshots are never merged across or away. A run that nets to no change is
    /// dropped (ADR-0101 rule 6).
    pub fn compact(&mut self, window_ms: u128) {
        if self.cursor != self.log.len() {
            return;
        }
        let mut merged: Vec<LogEntry> = Vec::with_capacity(self.log.len());
        for entry in self.log.drain(..) {
            let coalesce = match (&entry, merged.last_mut()) {
                (LogEntry::Delta(d), Some(LogEntry::Delta(prev)))
                    if d.control.is_some()
                        && d.control == prev.control
                        && d.changes.len() == 1
                        && prev.changes.len() == 1
                        && d.changes[0].stage_id == prev.changes[0].stage_id
                        && d.timestamp_ms.saturating_sub(prev.timestamp_ms) <= window_ms =>
                {
                    prev.changes[0].after = d.changes[0].after.clone();
                    prev.timestamp_ms = d.timestamp_ms;
                    true
                }
                _ => false,
            };
            if !coalesce {
                merged.push(entry);
            }
        }
        // A merged run that nets to before == after (a drag returning to its start) is dropped.
        merged.retain(|e| match e {
            LogEntry::Delta(d) => d.changes.iter().any(|c| c.before != c.after),
            LogEntry::Snapshot { .. } => true,
        });
        self.cursor = merged.len();
        self.log = merged;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(v: i64) -> StageEntry {
        StageEntry {
            schema_version: 1,
            params: serde_json::json!({ "v": v }),
        }
    }

    #[test]
    fn slider_drag_compacts_to_one_entry_and_keeps_pre_drag_undo_target() {
        let mut h = History::new(EditDocument::default());
        h.apply_at("nicti.exposure", "exposure_slider", entry(0), 0);
        h.apply_at("nicti.exposure", "exposure_slider", entry(1), 10);
        h.apply_at("nicti.exposure", "exposure_slider", entry(2), 20);
        assert_eq!(h.len(), 3);

        h.compact(50);
        assert_eq!(h.len(), 1);
        assert_eq!(
            h.document().stages.get("nicti.exposure").unwrap().params,
            serde_json::json!({ "v": 2 })
        );

        assert!(h.undo());
        assert!(!h.document().stages.contains_key("nicti.exposure"));
    }

    #[test]
    fn batch_paste_stays_one_entry_and_undoes_atomically() {
        let mut h = History::new(EditDocument::default());
        h.apply_batch(
            Uuid::new_v4(),
            vec![
                ("nicti.wb".to_string(), entry(1)),
                ("nicti.tone".to_string(), entry(2)),
            ],
        );
        assert_eq!(h.len(), 1);
        assert!(h.undo());
        assert!(h.document().stages.is_empty());
        assert!(h.redo());
        assert_eq!(h.document().stages.len(), 2);
    }

    #[test]
    fn compact_never_merges_across_a_batch_entry() {
        let mut h = History::new(EditDocument::default());
        h.apply_at("nicti.exposure", "exposure_slider", entry(0), 0);
        h.apply_batch(Uuid::new_v4(), vec![("nicti.wb".to_string(), entry(9))]);
        h.apply_at("nicti.exposure", "exposure_slider", entry(1), 10);
        h.compact(1000);
        assert_eq!(
            h.len(),
            3,
            "batch entry must not be absorbed into either slider run"
        );
    }

    #[test]
    fn compact_refuses_when_a_redo_is_pending() {
        let mut h = History::new(EditDocument::default());
        h.apply_at("nicti.exposure", "exposure_slider", entry(0), 0);
        h.apply_at("nicti.exposure", "exposure_slider", entry(1), 10);
        h.undo();
        let len_before = h.len();
        h.compact(1000);
        assert_eq!(
            h.len(),
            len_before,
            "compact must not run with a pending redo"
        );
    }

    #[test]
    fn unchanged_delta_appends_no_step_and_keeps_redo() {
        let mut h = History::new(EditDocument::default());
        h.apply_at("nicti.exposure", "exposure_slider", entry(1), 0);
        h.apply_at("nicti.exposure", "exposure_slider", entry(2), 10);
        assert!(h.undo());
        assert_eq!(h.len(), 2);

        h.apply_at("nicti.exposure", "exposure_slider", entry(1), 20);
        h.apply_batch(
            Uuid::new_v4(),
            vec![("nicti.exposure".to_string(), entry(1))],
        );
        assert_eq!(h.len(), 2, "no-op deltas must not append or truncate redo");
        assert!(h.redo(), "redo tail must survive a no-op delta");
        assert_eq!(
            h.document().stages["nicti.exposure"].params,
            serde_json::json!({ "v": 2 })
        );
    }

    #[test]
    fn partially_changed_batch_still_records() {
        let mut h = History::new(EditDocument::default());
        h.apply_batch(Uuid::new_v4(), vec![("a".to_string(), entry(1))]);
        h.apply_batch(
            Uuid::new_v4(),
            vec![("a".to_string(), entry(1)), ("b".to_string(), entry(2))],
        );
        assert_eq!(h.len(), 2);
    }

    #[test]
    fn absent_stage_is_not_equal_to_a_present_one() {
        let mut h = History::new(EditDocument::default());
        h.apply_batch(Uuid::new_v4(), vec![("a".to_string(), entry(0))]);
        assert_eq!(h.len(), 1, "None -> Some is a change at this layer");
    }

    #[test]
    fn compact_drops_a_run_that_returns_to_its_start() {
        let mut h = History::new(EditDocument::default());
        h.apply_at("nicti.exposure", "exposure_slider", entry(1), 0);
        h.apply_at("nicti.exposure", "exposure_slider", entry(2), 1000);
        h.apply_at("nicti.exposure", "exposure_slider", entry(1), 1010);
        assert_eq!(h.len(), 3);
        h.compact(50);
        assert_eq!(
            h.len(),
            1,
            "drag 1 -> 2 -> 1 nets to nothing, first entry stays"
        );
        assert_eq!(
            h.document().stages["nicti.exposure"].params,
            serde_json::json!({ "v": 1 })
        );
    }

    #[test]
    fn reset_removes_the_stage_as_one_undoable_step() {
        let mut h = History::new(EditDocument::default());
        h.apply_at("a", "a_slider", entry(1), 0);
        h.reset("a", "a_slider");
        assert!(!h.document().stages.contains_key("a"));
        assert_eq!(h.len(), 2);
        assert!(h.undo());
        assert_eq!(
            h.document().stages["a"].params,
            serde_json::json!({ "v": 1 })
        );
        assert!(h.redo());
        assert!(!h.document().stages.contains_key("a"));
    }

    #[test]
    fn resetting_an_absent_stage_is_a_no_op_that_keeps_redo() {
        let mut h = History::new(EditDocument::default());
        h.apply_at("a", "a_slider", entry(1), 0);
        assert!(h.undo());
        h.reset("a", "a_slider");
        assert_eq!(h.len(), 1, "nothing to remove: no step, redo tail intact");
        assert!(h.redo());
    }
}
