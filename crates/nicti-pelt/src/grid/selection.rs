//! The grid's multi-selection (#32): a set of positions in the id snapshot, stored as sorted,
//! disjoint index ranges. "Select all" over a million photos is one range, not a million entries,
//! and shift-selecting a thousand is one range too.
//!
//! Pure index arithmetic; [`GridSession`](super::session::GridSession) owns one and keeps it on
//! the same *photos* (not the same slots) when the snapshot changes, via [`Selection::remap`].
//! An empty selection means "just the cursor" -- plain clicking never builds a selection.

// `single_range_in_vec_init` guards against `vec![0..n]` written by someone who wanted `n`
// elements. Here a one-range `Vec<Range<usize>>` is exactly what "select all" *is*.
#![allow(clippy::single_range_in_vec_init)]

use std::collections::HashSet;
use std::ops::Range;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Selection {
    /// Sorted by start, non-empty, non-overlapping, and not adjacent (adjacent ranges merge).
    ranges: Vec<Range<usize>>,
    /// Where a shift-extension starts from: the last plain or ctrl click / the cursor when the
    /// selection began.
    anchor: Option<usize>,
}

impl Selection {
    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    pub fn len(&self) -> usize {
        self.ranges.iter().map(|r| r.len()).sum()
    }

    pub fn anchor(&self) -> Option<usize> {
        self.anchor
    }

    pub fn set_anchor(&mut self, anchor: Option<usize>) {
        self.anchor = anchor;
    }

    pub fn clear(&mut self) {
        self.ranges.clear();
    }

    pub fn contains(&self, index: usize) -> bool {
        // The range whose start is the greatest one <= index is the only candidate.
        let after = self.ranges.partition_point(|r| r.start <= index);
        after > 0 && self.ranges[after - 1].contains(&index)
    }

    /// The highest selected index, if any -- O(1), unlike walking [`Self::indices`].
    pub fn last(&self) -> Option<usize> {
        self.ranges.last().map(|r| r.end - 1)
    }

    /// The selected indices, ascending.
    pub fn indices(&self) -> impl Iterator<Item = usize> + '_ {
        self.ranges.iter().flat_map(|r| r.clone())
    }

    /// Replaces the selection with `a..=b` (either order): shift-click / shift-arrow.
    pub fn set_range(&mut self, a: usize, b: usize) {
        let (lo, hi) = (a.min(b), a.max(b));
        self.ranges = vec![lo..hi + 1];
    }

    /// Selects everything in `0..total`.
    pub fn select_all(&mut self, total: usize) {
        self.ranges = if total == 0 { vec![] } else { vec![0..total] };
    }

    /// Adds `index` if it isn't selected, removes it if it is: ctrl-click.
    pub fn toggle(&mut self, index: usize) {
        if self.contains(index) {
            self.remove(index);
        } else {
            self.insert(index);
        }
    }

    pub fn insert(&mut self, index: usize) {
        let at = self.ranges.partition_point(|r| r.start <= index);
        // Merge with the neighbour on either side when they touch.
        let touches_prev = at > 0 && self.ranges[at - 1].end == index;
        let touches_next = at < self.ranges.len() && self.ranges[at].start == index + 1;
        match (touches_prev, touches_next) {
            (true, true) => {
                let next_end = self.ranges[at].end;
                self.ranges[at - 1].end = next_end;
                self.ranges.remove(at);
            }
            (true, false) => self.ranges[at - 1].end = index + 1,
            (false, true) => self.ranges[at].start = index,
            (false, false) => self.ranges.insert(at, index..index + 1),
        }
    }

    fn remove(&mut self, index: usize) {
        let at = self.ranges.partition_point(|r| r.start <= index);
        if at == 0 {
            return;
        }
        let r = self.ranges[at - 1].clone();
        if !r.contains(&index) {
            return;
        }
        let left = r.start..index;
        let right = index + 1..r.end;
        self.ranges.remove(at - 1);
        let mut at = at - 1;
        for part in [left, right] {
            if !part.is_empty() {
                self.ranges.insert(at, part);
                at += 1;
            }
        }
    }

    /// Re-expresses the selection (and anchor) against a new id snapshot, keeping the same
    /// *photos* selected: an import adding assets, or a new sort, moves indices under it.
    /// Photos the new snapshot no longer contains drop out. O(old + new), only on a snapshot change.
    pub fn remap(&mut self, old_ids: &[i64], new_ids: &[i64]) {
        if self.ranges.is_empty() && self.anchor.is_none() {
            return;
        }
        let selected: HashSet<i64> = self
            .indices()
            .filter_map(|i| old_ids.get(i).copied())
            .collect();
        let anchor_id = self.anchor.and_then(|i| old_ids.get(i).copied());

        self.ranges.clear();
        self.anchor = None;
        let mut run_start: Option<usize> = None;
        for (i, id) in new_ids.iter().enumerate() {
            if Some(*id) == anchor_id {
                self.anchor = Some(i);
            }
            let hit = selected.contains(id);
            match (hit, run_start) {
                (true, None) => run_start = Some(i),
                (false, Some(s)) => {
                    self.ranges.push(s..i);
                    run_start = None;
                }
                _ => {}
            }
        }
        if let Some(s) = run_start {
            self.ranges.push(s..new_ids.len());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sel(ranges: &[Range<usize>]) -> Selection {
        Selection {
            ranges: ranges.to_vec(),
            anchor: None,
        }
    }

    #[test]
    fn set_range_accepts_either_direction_and_replaces() {
        let mut s = sel(&[0..2]);
        s.set_range(9, 5);
        assert_eq!(s.ranges, vec![5..10]);
        assert_eq!(s.len(), 5);
        assert!(s.contains(5) && s.contains(9) && !s.contains(4) && !s.contains(10));
    }

    #[test]
    fn select_all_is_one_range_even_for_a_million_photos() {
        let mut s = Selection::default();
        s.select_all(1_000_000);
        assert_eq!(s.ranges.len(), 1);
        assert_eq!(s.len(), 1_000_000);
        s.select_all(0);
        assert!(s.is_empty());
    }

    #[test]
    fn toggling_adds_merges_neighbours_and_splits() {
        let mut s = Selection::default();
        s.toggle(3);
        s.toggle(5);
        assert_eq!(s.ranges, vec![3..4, 5..6]);
        s.toggle(4); // bridges the gap
        assert_eq!(s.ranges, vec![3..6]);
        s.toggle(4); // splits it again
        assert_eq!(s.ranges, vec![3..4, 5..6]);
        s.toggle(3);
        s.toggle(5);
        assert!(s.is_empty());
    }

    #[test]
    fn insert_extends_at_either_end_and_keeps_order() {
        let mut s = Selection::default();
        for i in [10, 2, 6, 3, 9, 8] {
            s.insert(i);
        }
        assert_eq!(s.ranges, vec![2..4, 6..7, 8..11]);
        assert_eq!(s.indices().collect::<Vec<_>>(), vec![2, 3, 6, 8, 9, 10]);
    }

    #[test]
    fn contains_is_exact_at_range_edges_and_in_gaps() {
        let s = sel(&[2..4, 8..9]);
        let hits: Vec<usize> = (0..12).filter(|i| s.contains(*i)).collect();
        assert_eq!(hits, vec![2, 3, 8]);
    }

    #[test]
    fn remove_of_an_unselected_index_is_a_no_op() {
        let mut s = sel(&[2..4]);
        s.remove(7);
        s.remove(0);
        assert_eq!(s.ranges, vec![2..4]);
    }

    #[test]
    fn remap_keeps_the_same_photos_selected_when_indices_move() {
        // Photos 10,11,12 selected among 10..15; a new sort reverses the order.
        let old: Vec<i64> = (10..15).collect();
        let new: Vec<i64> = (10..15).rev().collect();
        let mut s = sel(&[0..3]);
        s.anchor = Some(0); // photo 10
        s.remap(&old, &new);
        assert_eq!(s.ranges, vec![2..5], "10, 11, 12 now sit at the end");
        assert_eq!(s.anchor, Some(4), "the anchor follows photo 10");
    }

    #[test]
    fn remap_drops_photos_that_left_the_snapshot_and_survives_growth() {
        let old = vec![1, 2, 3, 4];
        let mut s = sel(&[1..3]); // photos 2, 3
        s.remap(&old, &[1, 3, 9, 4]); // 2 is gone, 9 is new
        assert_eq!(s.ranges, vec![1..2]);

        let mut s = sel(&[0..1]);
        s.remap(&[7], &[5, 6, 7]); // an import prepended photos
        assert_eq!(s.ranges, vec![2..3]);
    }

    #[test]
    fn remap_with_nothing_selected_is_free_and_leaves_it_empty() {
        let mut s = Selection::default();
        s.remap(&[1, 2], &[3, 4]);
        assert!(s.is_empty() && s.anchor.is_none());
    }
}
