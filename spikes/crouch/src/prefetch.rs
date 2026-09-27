//! Nearest-to-cursor bake prioritization -- a copy of `spikes/loaf/src/prefetch.rs`, not a shared
//! dependency (spikes don't depend on each other). ADR-0044 already validated this as Tapetum's
//! own scheduling contract to Pounce ("bake jobs prioritized by |image_index - cursor|,
//! reprioritized immediately whenever the cursor moves"); this crate consumes it as the background
//! queue's ordering key, it doesn't re-derive it.

use std::collections::BTreeSet;

/// Orders `pending` (image indices with an outstanding bake job) by distance from `cursor`,
/// nearest first, ties broken by index for determinism.
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
        assert_eq!(priority_order(&pending, 3), vec![3, 2, 4, 1, 5, 0]);
    }

    #[test]
    fn reprioritizes_immediately_when_cursor_moves() {
        let pending: BTreeSet<usize> = [0, 1, 2, 3, 4].into_iter().collect();
        assert_eq!(priority_order(&pending, 0)[0], 0);
        assert_eq!(priority_order(&pending, 4)[0], 4);
    }
}
