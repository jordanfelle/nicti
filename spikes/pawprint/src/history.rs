//! Per-photo edit history: an append-only delta log plus named snapshots,
//! with compaction that coalesces consecutive same-control deltas (a slider
//! drag) into one step without losing the pre-drag undo target. Batches
//! (bulk paste/sync, #52) share a `batch_id` so undo/redo treat the whole
//! batch as one step.

use std::time::{SystemTime, UNIX_EPOCH};

use uuid::Uuid;

use crate::{EditDocument, StageEntry};

#[derive(Debug, Clone, PartialEq)]
pub struct Delta {
    pub batch_id: Uuid,
    pub stage_id: String,
    pub before: Option<StageEntry>,
    pub after: Option<StageEntry>,
    /// Coalescing key for compaction (e.g. `"exposure_slider"`). `None` for
    /// batch operations, which compact by `batch_id` at apply time instead
    /// (one delta per stage per batch, not one per tick).
    pub control: Option<String>,
    pub timestamp_ms: u128,
}

#[derive(Debug, Clone)]
enum LogEntry {
    Delta(Delta),
    /// A named marker over the document state at this point. Doesn't itself
    /// change the document (it's captured for reference/rollback by name,
    /// not replayed), so it costs nothing in undo/redo and is never merged
    /// away by `compact`.
    Snapshot { name: String, document: EditDocument },
}

pub struct History {
    document: EditDocument,
    log: Vec<LogEntry>,
    /// One past the most recently applied entry; `log[cursor..]` is the redo
    /// stack.
    cursor: usize,
}

fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis()
}

impl History {
    pub fn new(document: EditDocument) -> Self {
        Self { document, log: Vec::new(), cursor: 0 }
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

    /// Apply one stage change as a single-entry batch (a fresh `batch_id`
    /// per call), tagged with a coalescing `control` key. A slider drag
    /// calls this once per tick; `compact()` later merges the run.
    pub fn apply(&mut self, stage_id: &str, control: &str, after: StageEntry) {
        self.apply_at(stage_id, control, after, now_ms());
    }

    /// Same as `apply`, but with an explicit timestamp instead of the wall
    /// clock — lets tests simulate a real gap between two edit sessions
    /// (e.g. "pre-drag baseline" vs. "the drag itself") without sleeping.
    pub fn apply_at(&mut self, stage_id: &str, control: &str, after: StageEntry, timestamp_ms: u128) {
        self.apply_with_batch(Uuid::new_v4(), stage_id, Some(control), after, timestamp_ms);
    }

    /// Apply several stage changes under one shared `batch_id` — bulk
    /// paste/sync (#52) so the whole batch undoes in a single step instead
    /// of once per changed stage.
    pub fn apply_batch(&mut self, batch_id: Uuid, changes: Vec<(String, StageEntry)>) {
        let timestamp_ms = now_ms();
        for (stage_id, after) in changes {
            self.apply_with_batch(batch_id, &stage_id, None, after, timestamp_ms);
        }
    }

    fn apply_with_batch(
        &mut self,
        batch_id: Uuid,
        stage_id: &str,
        control: Option<&str>,
        after: StageEntry,
        timestamp_ms: u128,
    ) {
        self.log.truncate(self.cursor);
        let before = self.document.stages.get(stage_id).cloned();
        self.document.stages.insert(stage_id.to_string(), after.clone());
        self.log.push(LogEntry::Delta(Delta {
            batch_id,
            stage_id: stage_id.to_string(),
            before,
            after: Some(after),
            control: control.map(str::to_string),
            timestamp_ms,
        }));
        self.cursor = self.log.len();
    }

    /// A named, never-pruned marker over the current document state.
    pub fn snapshot(&mut self, name: &str) {
        self.log.truncate(self.cursor);
        self.log.push(LogEntry::Snapshot { name: name.to_string(), document: self.document.clone() });
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

    /// Undo the most recently applied entry. If it's a delta that belongs to
    /// a batch, undoes the entire contiguous batch as one step.
    pub fn undo(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let last_idx = self.cursor - 1;
        let start = self.batch_start(last_idx);
        for i in (start..=last_idx).rev() {
            if let LogEntry::Delta(d) = &self.log[i] {
                match &d.before {
                    Some(entry) => {
                        self.document.stages.insert(d.stage_id.clone(), entry.clone());
                    }
                    None => {
                        self.document.stages.remove(&d.stage_id);
                    }
                }
            }
        }
        self.cursor = start;
        true
    }

    pub fn redo(&mut self) -> bool {
        if self.cursor >= self.log.len() {
            return false;
        }
        let first_idx = self.cursor;
        let end = self.batch_end(first_idx);
        for i in first_idx..=end {
            if let LogEntry::Delta(d) = &self.log[i] {
                match &d.after {
                    Some(entry) => {
                        self.document.stages.insert(d.stage_id.clone(), entry.clone());
                    }
                    None => {
                        self.document.stages.remove(&d.stage_id);
                    }
                }
            }
        }
        self.cursor = end + 1;
        true
    }

    fn batch_start(&self, idx: usize) -> usize {
        let batch_id = match &self.log[idx] {
            LogEntry::Delta(d) => d.batch_id,
            LogEntry::Snapshot { .. } => return idx,
        };
        let mut start = idx;
        while start > 0 {
            match &self.log[start - 1] {
                LogEntry::Delta(d) if d.batch_id == batch_id => start -= 1,
                _ => break,
            }
        }
        start
    }

    fn batch_end(&self, idx: usize) -> usize {
        let batch_id = match &self.log[idx] {
            LogEntry::Delta(d) => d.batch_id,
            LogEntry::Snapshot { .. } => return idx,
        };
        let mut end = idx;
        while end + 1 < self.log.len() {
            match &self.log[end + 1] {
                LogEntry::Delta(d) if d.batch_id == batch_id => end += 1,
                _ => break,
            }
        }
        end
    }

    /// Coalesce consecutive deltas sharing `(stage_id, control)` within
    /// `window_ms` of each other into a single delta spanning the run
    /// (first entry's `before`, last entry's `after`). Only compacts the
    /// applied prefix (`..cursor`) — refuses if there's a pending redo, so
    /// compaction never corrupts a redo chain the user might still walk
    /// forward into. Snapshots are never merged across or away.
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
                        && d.stage_id == prev.stage_id
                        && d.timestamp_ms.saturating_sub(prev.timestamp_ms) <= window_ms =>
                {
                    prev.after = d.after.clone();
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
