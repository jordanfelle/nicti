//! Cross-photo undo/redo for marks (#32). One entry is one marking action across however many
//! photos it touched, holding each photo's markers before and after, so undoing a multi-select
//! rating restores every photo's *own* previous value (they need not have shared one).
//!
//! Lives in the writer thread (`worker.rs`) rather than the UI: undo has to be ordered with the
//! writes it reverses, and only the thread that performs them knows which have landed.

use std::collections::VecDeque;

use nicti_lair::AssetMeta;

/// Entries kept. A culling pass is tens of thousands of keypresses; the last few hundred are what
/// a mis-key ever needs.
pub const CAPACITY: usize = 256;
/// Photos' worth of history kept across all entries (each photo costs ~128 bytes: its markers
/// before and after). Entry count alone is no bound -- one select-all over a million photos is a
/// ~128 MB entry -- so the oldest entries are dropped once the total passes this, always keeping
/// the newest entry however large (undoing a select-all is exactly the mis-key that matters).
pub const MAX_TOTAL_PHOTOS: usize = 200_000;

/// One marking action's effect, as `(asset id, markers)` pairs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub before: Vec<(i64, AssetMeta)>,
    pub after: Vec<(i64, AssetMeta)>,
}

#[derive(Debug, Default)]
pub struct UndoRing {
    undo: VecDeque<Entry>,
    redo: Vec<Entry>,
}

impl UndoRing {
    /// Records a new action. Clears the redo stack (a new action forks history) and drops the
    /// oldest entry past [`CAPACITY`].
    pub fn push(&mut self, entry: Entry) {
        self.redo.clear();
        self.undo.push_back(entry);
        self.trim();
    }

    /// Photos held across both stacks.
    fn photos(&self) -> usize {
        self.undo
            .iter()
            .chain(self.redo.iter())
            .map(|e| e.before.len())
            .sum()
    }

    /// Drops the oldest undo entries past [`CAPACITY`] or [`MAX_TOTAL_PHOTOS`], never the newest.
    fn trim(&mut self) {
        while self.undo.len() > CAPACITY
            || (self.undo.len() > 1 && self.photos() > MAX_TOTAL_PHOTOS)
        {
            self.undo.pop_front();
        }
    }

    /// Takes the latest action off the undo stack for reversal; the caller writes its `before`
    /// values and then calls [`Self::commit_undo`] (or [`Self::abort_undo`] on failure).
    pub fn begin_undo(&mut self) -> Option<Entry> {
        self.undo.pop_back()
    }

    pub fn commit_undo(&mut self, entry: Entry) {
        self.redo.push(entry);
    }

    /// The write failed: put the entry back so a retry can find it.
    pub fn abort_undo(&mut self, entry: Entry) {
        self.undo.push_back(entry);
    }

    pub fn begin_redo(&mut self) -> Option<Entry> {
        self.redo.pop()
    }

    pub fn commit_redo(&mut self, entry: Entry) {
        self.undo.push_back(entry);
        self.trim();
    }

    pub fn abort_redo(&mut self, entry: Entry) {
        self.redo.push(entry);
    }

    /// Forgets `ids` everywhere -- their photos were deleted, so restoring their markers would
    /// write to rows that no longer exist. Entries left with no photos are dropped.
    pub fn forget(&mut self, ids: &[i64]) {
        // A set, not `ids.contains`: this runs on the writer thread every frame of a big delete,
        // over every entry's every photo.
        let gone: std::collections::HashSet<i64> = ids.iter().copied().collect();
        let strip = |e: &mut Entry| {
            e.before.retain(|(id, _)| !gone.contains(id));
            e.after.retain(|(id, _)| !gone.contains(id));
            !e.before.is_empty()
        };
        self.undo.retain_mut(strip);
        self.redo.retain_mut(strip);
    }

    #[cfg(test)]
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    #[cfg(test)]
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: i64, from: Option<i64>, to: Option<i64>) -> Entry {
        let m = |rating| AssetMeta {
            rating,
            ..AssetMeta::default()
        };
        Entry {
            before: vec![(id, m(from))],
            after: vec![(id, m(to))],
        }
    }

    #[test]
    fn undo_then_redo_walks_the_history() {
        let mut ring = UndoRing::default();
        ring.push(entry(1, None, Some(3)));
        ring.push(entry(1, Some(3), Some(5)));

        let e = ring.begin_undo().unwrap();
        assert_eq!(e.after[0].1.rating, Some(5));
        ring.commit_undo(e);
        assert!(ring.can_redo());

        let e = ring.begin_redo().unwrap();
        assert_eq!(e.after[0].1.rating, Some(5));
        ring.commit_redo(e);
        assert!(!ring.can_redo());
        assert!(ring.can_undo());
    }

    #[test]
    fn a_new_action_after_an_undo_clears_redo() {
        let mut ring = UndoRing::default();
        ring.push(entry(1, None, Some(3)));
        let e = ring.begin_undo().unwrap();
        ring.commit_undo(e);
        assert!(ring.can_redo());
        ring.push(entry(1, None, Some(1)));
        assert!(!ring.can_redo());
    }

    #[test]
    fn the_ring_drops_its_oldest_entry_past_capacity() {
        let mut ring = UndoRing::default();
        for i in 0..(CAPACITY as i64 + 10) {
            ring.push(entry(i, None, Some(1)));
        }
        let mut n = 0;
        let mut last_id = None;
        while let Some(e) = ring.begin_undo() {
            n += 1;
            last_id = Some(e.before[0].0);
        }
        assert_eq!(n, CAPACITY);
        assert_eq!(last_id, Some(10), "ids 0..10 fell off the front");
    }

    #[test]
    fn an_aborted_undo_or_redo_leaves_the_entry_where_it_was() {
        let mut ring = UndoRing::default();
        ring.push(entry(1, None, Some(3)));
        let e = ring.begin_undo().unwrap();
        ring.abort_undo(e);
        assert!(ring.can_undo() && !ring.can_redo());

        let e = ring.begin_undo().unwrap();
        ring.commit_undo(e);
        let e = ring.begin_redo().unwrap();
        ring.abort_redo(e);
        assert!(ring.can_redo() && !ring.can_undo());
    }

    #[test]
    fn forget_strips_deleted_photos_and_drops_emptied_entries() {
        let mut ring = UndoRing::default();
        ring.push(Entry {
            before: vec![(1, AssetMeta::default()), (2, AssetMeta::default())],
            after: vec![(1, AssetMeta::default()), (2, AssetMeta::default())],
        });
        ring.push(entry(3, None, Some(2)));
        ring.forget(&[1, 3]);
        let e = ring.begin_undo().unwrap();
        assert_eq!(e.before.len(), 1);
        assert_eq!(e.before[0].0, 2, "photo 2's undo survives");
        assert!(
            ring.begin_undo().is_none(),
            "photo 3's entry is gone entirely"
        );
    }

    fn big_entry(n: i64) -> Entry {
        let items: Vec<(i64, AssetMeta)> = (0..n).map(|i| (i, AssetMeta::default())).collect();
        Entry {
            before: items.clone(),
            after: items,
        }
    }

    #[test]
    fn history_is_bounded_by_photos_not_just_entries() {
        let mut ring = UndoRing::default();
        for _ in 0..4 {
            ring.push(big_entry(150_000));
        }
        // 4 x 150k photos would be 600k; only what fits under the cap survives (one entry,
        // since a second would pass 200k).
        let mut kept = 0;
        while ring.begin_undo().is_some() {
            kept += 1;
        }
        assert_eq!(kept, 1);
    }

    #[test]
    fn the_newest_entry_is_kept_however_large_so_a_select_all_can_be_undone() {
        let mut ring = UndoRing::default();
        ring.push(big_entry(10));
        ring.push(big_entry(MAX_TOTAL_PHOTOS as i64 * 3));
        let e = ring
            .begin_undo()
            .expect("the oversized newest entry survives");
        assert_eq!(e.before.len(), MAX_TOTAL_PHOTOS * 3);
        assert!(
            ring.begin_undo().is_none(),
            "the small old one was dropped to make room"
        );
    }
}
