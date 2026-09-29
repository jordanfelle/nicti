//! Pure grid geometry for the virtualized library view (#30): how many columns fit, which id
//! indices a range of visible rows covers, and how those map onto fixed-size thumbnail batches.
//! No egui, no catalog -- so the math (the part that breaks at 2M cells) is unit-testable alone.

use std::ops::Range;

/// Ids per `ThumbBatchJob`. Batches are fixed blocks of the id snapshot (batch `b` covers
/// `ids[b*BATCH .. (b+1)*BATCH]`), independent of the column count, so a window resize never
/// invalidates in-flight or finished work.
pub const BATCH: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GridLayout {
    pub cols: usize,
    /// Outer size of one cell (thumbnail + gap), in points. Cells are square.
    pub cell: f32,
}

impl GridLayout {
    /// `cell` is the outer cell size; as many columns as fit `available_width`, at least one.
    /// A non-finite or non-positive width/cell degrades to one column rather than dividing by
    /// zero (a zero-size panel happens for a frame while a window is being resized).
    pub fn new(available_width: f32, cell: f32) -> Self {
        let cell = if cell.is_finite() && cell > 0.0 {
            cell
        } else {
            1.0
        };
        let cols = if available_width.is_finite() && available_width > 0.0 {
            ((available_width / cell).floor() as usize).max(1)
        } else {
            1
        };
        Self { cols, cell }
    }

    pub fn rows(&self, total: usize) -> usize {
        total.div_ceil(self.cols)
    }

    pub fn row_of(&self, index: usize) -> usize {
        index / self.cols
    }

    /// The id indices covered by `rows`, clamped to `total`.
    pub fn indices_for_rows(&self, rows: Range<usize>, total: usize) -> Range<usize> {
        let start = rows.start.saturating_mul(self.cols).min(total);
        let end = rows.end.saturating_mul(self.cols).min(total);
        start..end
    }
}

/// The batches overlapping `indices`, extended by `overscan_batches` on both sides and clamped to
/// the batches that exist for `total` ids.
pub fn batches_for(indices: Range<usize>, total: usize, overscan_batches: usize) -> Range<usize> {
    if indices.is_empty() || total == 0 {
        return 0..0;
    }
    let batch_count = total.div_ceil(BATCH);
    let first = (indices.start / BATCH).saturating_sub(overscan_batches);
    let last = ((indices.end - 1) / BATCH)
        .saturating_add(overscan_batches)
        .min(batch_count - 1);
    first..last + 1
}

/// The id-index range batch `batch` covers, clamped to `total`.
pub fn batch_indices(batch: usize, total: usize) -> Range<usize> {
    let start = batch.saturating_mul(BATCH).min(total);
    let end = start.saturating_add(BATCH).min(total);
    start..end
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_fit_the_width_and_never_drop_below_one() {
        assert_eq!(GridLayout::new(1000.0, 200.0).cols, 5);
        assert_eq!(GridLayout::new(999.0, 200.0).cols, 4);
        assert_eq!(GridLayout::new(50.0, 200.0).cols, 1);
        assert_eq!(GridLayout::new(0.0, 200.0).cols, 1);
        assert_eq!(GridLayout::new(-3.0, 200.0).cols, 1);
        assert_eq!(GridLayout::new(f32::NAN, 200.0).cols, 1);
        assert_eq!(GridLayout::new(f32::INFINITY, 200.0).cols, 1);
        // A degenerate cell size must not divide by zero.
        assert_eq!(GridLayout::new(100.0, 0.0).cols, 100);
        assert_eq!(GridLayout::new(100.0, f32::NAN).cols, 100);
    }

    #[test]
    fn rows_round_up_and_handle_empty() {
        let l = GridLayout::new(600.0, 200.0); // 3 cols
        assert_eq!(l.rows(0), 0);
        assert_eq!(l.rows(1), 1);
        assert_eq!(l.rows(3), 1);
        assert_eq!(l.rows(4), 2);
        assert_eq!(l.row_of(0), 0);
        assert_eq!(l.row_of(2), 0);
        assert_eq!(l.row_of(3), 1);
    }

    #[test]
    fn two_million_cells_do_not_overflow_or_lose_the_tail() {
        let l = GridLayout::new(1800.0, 180.0); // 10 cols
        let total = 2_000_000;
        assert_eq!(l.rows(total), 200_000);
        let last = l.indices_for_rows(199_990..200_000, total);
        assert_eq!(last, 1_999_900..2_000_000);
        // Rows past the end clamp instead of producing an inverted or out-of-bounds range.
        assert_eq!(l.indices_for_rows(200_000..200_010, total), total..total);
        assert!(l.indices_for_rows(5..5, total).is_empty());
    }

    #[test]
    fn a_partial_last_row_is_clamped_to_total() {
        let l = GridLayout::new(600.0, 200.0); // 3 cols
        assert_eq!(l.indices_for_rows(1..2, 5), 3..5);
    }

    #[test]
    fn batches_cover_the_visible_indices_plus_overscan_and_clamp() {
        let total = 1000; // 16 batches (15.6 rounded up)
        assert_eq!(batches_for(0..10, total, 0), 0..1);
        assert_eq!(batches_for(60..70, total, 0), 0..2);
        assert_eq!(batches_for(64..128, total, 0), 1..2);
        assert_eq!(batches_for(64..128, total, 1), 0..3);
        // Overscan clamps at both ends.
        assert_eq!(batches_for(0..1, total, 3), 0..4);
        assert_eq!(batches_for(990..1000, total, 3), 12..16);
        assert_eq!(batches_for(0..0, total, 2), 0..0);
        assert_eq!(batches_for(0..10, 0, 2), 0..0);
    }

    #[test]
    fn batch_indices_partition_the_snapshot_exactly() {
        let total: usize = 200;
        let mut covered = Vec::new();
        for b in 0..total.div_ceil(BATCH) {
            covered.extend(batch_indices(b, total));
        }
        assert_eq!(covered, (0..total).collect::<Vec<_>>());
        assert!(batch_indices(99, total).is_empty());
    }
}
