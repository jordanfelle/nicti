//! Directional N+-k bake prioritization: #44's own ticket body: "Precompute after bulk paste:
//! background scheduler bakes denoise/mask stages for all affected images, nearest-to-cursor
//! first" and "Directional prefetch: render N+-k fully while viewing N." This module defines only
//! the *ordering* Pounce (#54, the actual scheduler) would consume -- it doesn't run jobs itself,
//! `sim.rs` does that for the hero-scenario simulation.

use std::collections::BTreeSet;

/// Orders `pending` (image indices with an outstanding bake job) by distance from `cursor`,
/// nearest first, ties broken by index for determinism. This is a pure function, not a running
/// queue, so "cancellable or reprioritized when the cursor moves" (#44's own scheduler contract)
/// is just: call this again with the new cursor and re-submit in the new order -- a job already
/// in flight finishes (bake stages aren't preemptible mid-dispatch on a GPU), but the *next* job
/// picked reflects the new cursor immediately.
pub fn priority_order(pending: &BTreeSet<usize>, cursor: usize) -> Vec<usize> {
    let mut order: Vec<usize> = pending.iter().copied().collect();
    order.sort_by_key(|&i| (i.abs_diff(cursor), i));
    order
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders_by_distance_from_cursor() {
        let pending: BTreeSet<usize> = [0, 1, 2, 3, 4, 5].into_iter().collect();
        let order = priority_order(&pending, 3);
        assert_eq!(order, vec![3, 2, 4, 1, 5, 0]);
    }

    #[test]
    fn ties_break_by_index() {
        let pending: BTreeSet<usize> = [1, 5].into_iter().collect();
        // Both are distance 2 from cursor 3.
        let order = priority_order(&pending, 3);
        assert_eq!(order, vec![1, 5]);
    }

    #[test]
    fn reprioritizes_immediately_when_cursor_moves() {
        let pending: BTreeSet<usize> = [0, 1, 2, 3, 4].into_iter().collect();
        let before = priority_order(&pending, 0);
        let after = priority_order(&pending, 4);
        assert_eq!(before[0], 0);
        assert_eq!(after[0], 4);
    }
}
