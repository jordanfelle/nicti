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

/// How far [`Selection::remap`]'s merge-walk first looks ahead to resync after an insertion or
/// removal: scattered single changes resolve here, cheaply, on whichever side has the photo.
const WALK_NEAR: usize = 64;
/// The wider second look, tried only when both near looks missed (a bulk import is one long run of
/// inserts). Total scanning is budgeted, see `remap_walk`.
const WALK_WINDOW: usize = 1 << 16;

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
    ///
    /// When the new snapshot is the old one with photos inserted and removed (an import, a filter
    /// tweak) a merge-walk answers in one pass with hash sets sized by the *difference*, not the
    /// library (#498: the 1M-entry set was ~190 ms). A real reorder (new sort) falls back to the set.
    /// Photo ids are unique within a snapshot (they are catalog row ids); both paths assume it.
    pub fn remap(&mut self, old_ids: &[i64], new_ids: &[i64]) {
        if (self.ranges.is_empty() && self.anchor.is_none()) || old_ids == new_ids {
            return;
        }
        if let Some((ranges, anchor)) = self.remap_walk(old_ids, new_ids) {
            self.ranges = ranges;
            self.anchor = anchor;
            return;
        }
        self.remap_by_set(old_ids, new_ids);
    }

    /// Merge-walk remap. `None` when the snapshots aren't "same order, plus/minus some photos"
    /// (too many differences, or a photo that moved), so the caller
    /// takes the set path. Exact: every new photo not matched in the walk is declared inserted
    /// and every unmatched old one deleted; the two sets being disjoint proves none of the
    /// "inserted" photos was really an old (possibly selected) photo that moved.
    fn remap_walk(
        &self,
        old_ids: &[i64],
        new_ids: &[i64],
    ) -> Option<(Vec<Range<usize>>, Option<usize>)> {
        fn emit(out: &mut Vec<Range<usize>>, run: &mut Option<usize>, j: usize, hit: bool) {
            match (hit, *run) {
                (true, None) => *run = Some(j),
                (false, Some(s)) => {
                    out.push(s..j);
                    *run = None;
                }
                _ => {}
            }
        }
        enum Skip {
            New(usize),
            Old(usize),
        }
        /// Position of `id` within the first `window` of `hay`; `None` (via `?`) once the scan
        /// budget is spent.
        fn scan(hay: &[i64], id: i64, window: usize, budget: &mut usize) -> Option<Option<usize>> {
            let hay = &hay[..hay.len().min(window)];
            let found = hay.iter().position(|x| *x == id);
            let cost = found.map_or(hay.len(), |d| d + 1);
            *budget = budget.checked_sub(cost)?;
            Some(found)
        }
        /// At a mismatch, how many new photos to skip as inserted or old ones as deleted to line
        /// up again. Near look on both sides first, so a scattered removal never pays for a wide
        /// failed scan; the wide look is for bulk runs. Outer `None`: budget spent.
        fn resync(old: &[i64], new: &[i64], budget: &mut usize) -> Option<Option<Skip>> {
            for window in [WALK_NEAR, WALK_WINDOW] {
                let ins = scan(new, old[0], window, budget)?;
                let del = scan(old, new[0], window, budget)?;
                match (ins, del) {
                    (Some(n), Some(o)) if o < n => return Some(Some(Skip::Old(o))),
                    (Some(n), _) => return Some(Some(Skip::New(n))),
                    (None, Some(o)) => return Some(Some(Skip::Old(o))),
                    (None, None) => {}
                }
            }
            Some(None)
        }
        let anchor_old = self.anchor.filter(|&a| a < old_ids.len());
        let mut out = Vec::new();
        let mut run = None;
        let mut anchor = None;
        let mut ri = 0; // first range that could still contain index `i`
        let mut deleted = HashSet::new();
        let mut inserted = HashSet::new();
        // A reorder declares nearly everything deleted+inserted; stop early and use the set path.
        let diff_cap = (old_ids.len() / 8).max(1024);
        // Lookahead scans are charged here so a reorder can't cost O(n * window).
        let mut budget = 16 * (old_ids.len() + new_ids.len()) + 4 * WALK_WINDOW;
        let (mut i, mut j) = (0, 0);
        while j < new_ids.len() {
            if i >= old_ids.len() {
                inserted.insert(new_ids[j]);
                emit(&mut out, &mut run, j, false);
                j += 1;
            } else if old_ids[i] == new_ids[j] {
                while ri < self.ranges.len() && self.ranges[ri].end <= i {
                    ri += 1;
                }
                let hit = ri < self.ranges.len() && self.ranges[ri].start <= i;
                if Some(i) == anchor_old {
                    anchor = Some(j);
                }
                emit(&mut out, &mut run, j, hit);
                i += 1;
                j += 1;
            } else if let Some(skip) = resync(&old_ids[i..], &new_ids[j..], &mut budget)? {
                match skip {
                    Skip::New(d) => {
                        inserted.extend(&new_ids[j..j + d]);
                        for k in j..j + d {
                            emit(&mut out, &mut run, k, false);
                        }
                        j += d;
                    }
                    Skip::Old(d) => {
                        deleted.extend(&old_ids[i..i + d]);
                        i += d;
                    }
                }
            } else {
                // Neither side resyncs: call both a removal and an insertion. The disjointness
                // check below keeps that exact if it was really a move.
                deleted.insert(old_ids[i]);
                inserted.insert(new_ids[j]);
                emit(&mut out, &mut run, j, false);
                i += 1;
                j += 1;
            }
            if deleted.len() + inserted.len() > diff_cap {
                return None;
            }
        }
        let tail = &old_ids[i.min(old_ids.len())..];
        if deleted.len() + inserted.len() + tail.len() > diff_cap {
            return None;
        }
        deleted.extend(tail);
        if !deleted.is_disjoint(&inserted) {
            return None;
        }
        if let Some(s) = run {
            out.push(s..new_ids.len());
        }
        Some((out, anchor))
    }

    fn remap_by_set(&mut self, old_ids: &[i64], new_ids: &[i64]) {
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
    fn remap_against_an_unchanged_snapshot_changes_nothing() {
        let mut s = sel(&[0..2, 5..6]);
        s.set_anchor(Some(5));
        let before = s.clone();
        s.remap(&[1, 2, 3, 4, 5, 6], &[1, 2, 3, 4, 5, 6]);
        assert_eq!(s, before);
    }

    /// The merge-walk must agree with the set path on every shape of change, including the ones
    /// it has to refuse (moved photos) -- compared over pseudo-random snapshots.
    #[test]
    fn remap_walk_matches_the_set_path_on_random_snapshots() {
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let mut walked = 0;
        for _ in 0..2000 {
            let old: Vec<i64> = (0..40).collect();
            let mut new: Vec<i64> = old.iter().copied().filter(|_| next(5) != 0).collect();
            for k in 0..next(8) {
                let at = next(new.len() as u64 + 1) as usize;
                new.insert(at, 100 + k as i64);
            }
            if next(4) == 0 && new.len() > 3 {
                let (a, b) = (
                    next(new.len() as u64) as usize,
                    next(new.len() as u64) as usize,
                );
                new.swap(a, b); // a moved photo: the walk must notice
            }
            let mut s = Selection::default();
            for _ in 0..next(6) {
                let a = next(40) as usize;
                let b = a + next(8) as usize;
                for i in a..=b.min(39) {
                    s.insert(i);
                }
            }
            s.anchor = Some(next(40) as usize);
            let mut by_set = s.clone();
            by_set.remap_by_set(&old, &new);
            let mut by_walk = s.clone();
            by_walk.remap(&old, &new);
            assert_eq!(by_walk, by_set, "old={old:?} new={new:?} sel={s:?}");
            walked += s.remap_walk(&old, &new).is_some() as u32;
        }
        assert!(
            walked > 500,
            "the fast path should handle plain inserts/removals ({walked})"
        );
    }

    #[test]
    fn remap_walk_handles_long_contiguous_runs_at_and_past_the_old_window() {
        for run in [63usize, 64, 65, 100, 5000, 70_000] {
            let old: Vec<i64> = (0..200_000).collect();
            // A bulk import lands before index 10; then a bulk removal of the same size.
            let mut grown = old.clone();
            grown.splice(10..10, (0..run as i64).map(|k| 1_000_000 + k));
            let mut shrunk = old.clone();
            shrunk.drain(10..10 + run);
            for new in [&grown, &shrunk] {
                let mut s = sel(&[5..20, 150_000..150_010]);
                s.anchor = Some(7);
                // Past the lookahead the walk may refuse; the result must still match.
                assert!(
                    run >= WALK_WINDOW || s.remap_walk(&old, new).is_some(),
                    "run {run} fell back"
                );
                let mut by_set = s.clone();
                by_set.remap_by_set(&old, new);
                s.remap(&old, new);
                assert_eq!(s, by_set, "run {run}");
            }
        }
    }

    #[test]
    fn remap_walk_stays_fast_for_scattered_removals_and_bounds_its_work() {
        let old: Vec<i64> = (0..200_000).collect();
        let sparse: Vec<i64> = old.iter().copied().filter(|i| i % 1000 != 7).collect();
        let mut s = sel(&[0..200_000]);
        assert!(
            s.remap_walk(&old, &sparse).is_some(),
            "200 scattered removals"
        );

        // Dropping the tail of a big library is a big diff: refuse, don't build a huge set.
        assert!(s.remap_walk(&old, &old[..10]).is_none());

        // Scattered substitutions resync on neither side; each costs two wide failed scans, so
        // the work budget (not the diff cap, which 200 entries never reach) ends the walk.
        let mut subst = old.clone();
        for k in 0..100 {
            subst[k * 1900 + 5] = 1_000_000 + k as i64;
        }
        assert!(s.remap_walk(&old, &subst).is_none(), "budget exhausted");
        let mut by_set = s.clone();
        by_set.remap_by_set(&old, &subst);
        s.remap(&old, &subst);
        assert_eq!(s, by_set);
    }

    #[test]
    fn remap_walk_refuses_a_reversed_snapshot_and_copes_with_odd_anchors() {
        let old: Vec<i64> = (0..5000).collect();
        let rev: Vec<i64> = old.iter().rev().copied().collect();
        let mut s = sel(&[0..2500]);
        assert!(s.remap_walk(&old, &rev).is_none());
        let mut by_set = s.clone();
        by_set.remap_by_set(&old, &rev);
        s.remap(&old, &rev);
        assert_eq!(s, by_set);

        // Anchor with no ranges, an anchor and ranges past the old snapshot, and empty ends.
        for (ranges, anchor) in [
            (vec![], Some(2)),
            (vec![8..12], Some(99)),
            (vec![0..3], None),
        ] {
            for (o, n) in [
                (vec![1, 2, 3], vec![]),
                (vec![], vec![4, 5]),
                (vec![1, 2, 3], vec![3, 9]),
            ] {
                let mut a = Selection {
                    ranges: ranges.clone(),
                    anchor,
                };
                let mut b = a.clone();
                a.remap(&o, &n);
                b.remap_by_set(&o, &n);
                assert_eq!(a, b, "{ranges:?} {anchor:?} {o:?} -> {n:?}");
            }
        }
    }

    #[test]
    fn remap_with_nothing_selected_is_free_and_leaves_it_empty() {
        let mut s = Selection::default();
        s.remap(&[1, 2], &[3, 4]);
        assert!(s.is_empty() && s.anchor.is_none());
    }
}

/// #335 scale check: `remap` over a select-all of 1M photos, as an import's live snapshot reload
/// does. Run by hand: `cargo test -p nicti-pelt --release remap_at_one_million -- --ignored --nocapture`.
#[cfg(test)]
mod scale {
    use super::*;

    #[test]
    #[ignore = "1M-photo timing; run manually with --ignored --nocapture"]
    fn remap_at_one_million_photos() {
        const N: usize = 1_000_000;
        let old: Vec<i64> = (0..N as i64).collect();
        // The new snapshot has 1% more photos interleaved, as a mid-import reload would.
        let new: Vec<i64> = (0..N as i64 + N as i64 / 100)
            .map(|i| {
                if i % 100 == 99 {
                    N as i64 + i / 100
                } else {
                    i - i / 100
                }
            })
            .collect();
        let mut s = Selection::default();
        s.select_all(N);
        let start = std::time::Instant::now();
        s.remap(&old, &new);
        println!("remap, select-all over 1M: {:.2?}", start.elapsed());
        let start = std::time::Instant::now();
        s.remap(&new, &new);
        println!("remap, same snapshot: {:.2?}", start.elapsed());
    }
}
