//! Per-photo edit history: an append-only delta log plus named snapshots,
//! with compaction that coalesces consecutive same-control deltas (a slider
//! drag) into one step without losing the pre-drag undo target. A bulk
//! paste/sync (#52) applies as one `Delta` covering every stage it touched,
//! so it's genuinely one history entry per photo, and undoes as one step by
//! construction rather than by grouping several log entries back together.

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
    /// Coalescing key for compaction (e.g. `"exposure_slider"`). `None` for
    /// multi-stage batch operations — those are never merged by `compact`,
    /// only single-stage single-control deltas are (a slider drag).
    pub control: Option<String>,
    pub timestamp_ms: u128,
}

#[derive(Debug, Clone, Serialize)]
enum LogEntry {
    Delta(Delta),
    /// A named marker over the document state at this point. Doesn't itself
    /// change the document (it's captured for reference/rollback by name,
    /// not replayed), so it costs nothing in undo/redo and is never merged
    /// away by `compact`.
    Snapshot {
        name: String,
        document: EditDocument,
    },
}

pub struct History {
    document: EditDocument,
    log: Vec<LogEntry>,
    /// One past the most recently applied entry; `log[cursor..]` is the redo
    /// stack.
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

    /// Approximate on-disk size of the log as it stands, by actually
    /// serializing it (not a guessed per-entry constant) — used for the
    /// ADR's sizing table.
    pub fn serialized_len(&self) -> usize {
        serde_json::to_vec(&self.log).map(|b| b.len()).unwrap_or(0)
    }

    /// Apply one stage change, tagged with a coalescing `control` key. A
    /// slider drag calls this once per tick; `compact()` later merges the
    /// run into a single entry.
    pub fn apply(&mut self, stage_id: &str, control: &str, after: StageEntry) {
        self.apply_at(stage_id, control, after, now_ms());
    }

    /// Same as `apply`, but with an explicit timestamp instead of the wall
    /// clock — lets tests simulate a real gap between two edit sessions
    /// (e.g. "pre-drag baseline" vs. "the drag itself") without sleeping.
    pub fn apply_at(
        &mut self,
        stage_id: &str,
        control: &str,
        after: StageEntry,
        timestamp_ms: u128,
    ) {
        self.push_delta(
            Uuid::new_v4(),
            vec![(stage_id.to_string(), after)],
            Some(control),
            timestamp_ms,
        );
    }

    /// Apply several stage changes as a single history entry — bulk
    /// paste/sync (#52), so the whole batch is one entry per photo (not one
    /// per changed stage) and undoes/redoes atomically by construction.
    pub fn apply_batch(&mut self, batch_id: Uuid, changes: Vec<(String, StageEntry)>) {
        self.push_delta(batch_id, changes, None, now_ms());
    }

    fn push_delta(
        &mut self,
        batch_id: Uuid,
        changes: Vec<(String, StageEntry)>,
        control: Option<&str>,
        timestamp_ms: u128,
    ) {
        self.log.truncate(self.cursor);
        let mut recorded = Vec::with_capacity(changes.len());
        for (stage_id, after) in changes {
            let before = self.document.stages.get(&stage_id).cloned();
            self.document.stages.insert(stage_id.clone(), after.clone());
            recorded.push(StageChange {
                stage_id,
                before,
                after: Some(after),
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

    /// The document state captured under a named snapshot, for viewing or
    /// restoring "revert to this named snapshot" without walking undo one
    /// step at a time.
    pub fn snapshot_document(&self, name: &str) -> Option<&EditDocument> {
        self.log.iter().find_map(|e| match e {
            LogEntry::Snapshot { name: n, document } if n == name => Some(document),
            _ => None,
        })
    }

    /// Undo the most recently applied entry (one whole batch, or one
    /// compacted slider-drag run — every entry in the log is already an
    /// atomic undo unit, so no cross-entry grouping is needed here).
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

    /// Coalesce consecutive single-stage deltas sharing the same `control`
    /// key within `window_ms` of each other into a single delta spanning
    /// the run (first entry's `before`, last entry's `after`). Only
    /// single-stage, `control`-tagged deltas (from `apply`/`apply_at`) are
    /// ever merged — a batch (`control: None`, possibly multiple stages)
    /// never coalesces with anything, so a bulk paste always stays exactly
    /// one entry, and never accidentally absorbs an unrelated slider tick.
    /// Only compacts the applied prefix (`..cursor`) — refuses if there's a
    /// pending redo, so compaction never corrupts a redo chain the user
    /// might still walk forward into. Snapshots are never merged across or
    /// away.
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
        self.cursor = merged.len();
        self.log = merged;
    }
}
