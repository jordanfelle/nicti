//! Pure virtualized-grid windowing math: given a scroll offset and viewport size, which cell
//! range actually needs a tile fetched/painted. egui and GPUI have their own built-in virtualized
//! list/grid primitives (`ScrollArea::show_rows`, `uniform_list`); Iced has no built-in
//! equivalent as of 0.14, so `spikes/pelt-iced` drives its own `Shader` widget off this module
//! directly. Kept toolkit-agnostic and unit-tested here regardless, so every candidate's grid
//! logic is checked against the same math.

/// A row/column layout for a fixed-size square-tile grid.
#[derive(Debug, Clone, Copy)]
pub struct GridLayout {
    pub cell_count: usize,
    pub tile_size: f32,
    pub columns: usize,
}

impl GridLayout {
    pub fn new(cell_count: usize, tile_size: f32, viewport_width: f32) -> Self {
        let columns = ((viewport_width / tile_size).floor() as usize).max(1);
        Self {
            cell_count,
            tile_size,
            columns,
        }
    }

    pub fn rows(&self) -> usize {
        self.cell_count.div_ceil(self.columns)
    }

    pub fn total_height(&self) -> f32 {
        self.rows() as f32 * self.tile_size
    }

    /// The half-open cell range `[start, end)` visible for a viewport of `viewport_height`
    /// starting at vertical scroll offset `scroll_y`, with `overscan_rows` extra rows rendered on
    /// each side so a fast scroll doesn't show a blank frame before the next paint catches up.
    pub fn visible_range(
        &self,
        scroll_y: f32,
        viewport_height: f32,
        overscan_rows: usize,
    ) -> std::ops::Range<usize> {
        if self.cell_count == 0 {
            return 0..0;
        }
        let first_row = (scroll_y / self.tile_size).floor().max(0.0) as usize;
        let visible_rows = (viewport_height / self.tile_size).ceil() as usize + 1;
        let start_row = first_row.saturating_sub(overscan_rows);
        let end_row = (first_row + visible_rows + overscan_rows).min(self.rows());

        let start = (start_row * self.columns).min(self.cell_count);
        let end = (end_row * self.columns).min(self.cell_count);
        start..end
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_fit_viewport_width() {
        let layout = GridLayout::new(1000, 256.0, 1000.0);
        assert_eq!(layout.columns, 3);
    }

    #[test]
    fn at_least_one_column() {
        let layout = GridLayout::new(1000, 256.0, 10.0);
        assert_eq!(layout.columns, 1);
    }

    #[test]
    fn visible_range_at_top_starts_at_zero() {
        let layout = GridLayout::new(2_000_000, 256.0, 1280.0);
        let range = layout.visible_range(0.0, 800.0, 0);
        assert_eq!(range.start, 0);
        assert!(range.end > 0);
    }

    #[test]
    fn visible_range_excludes_far_offscreen_cells() {
        let layout = GridLayout::new(2_000_000, 256.0, 1280.0);
        let range = layout.visible_range(0.0, 800.0, 2);
        // 2M cells at a handful of columns is a huge grid; a correct virtualization must not
        // return anywhere near the full 2M-cell range for a single small viewport.
        assert!(range.end - range.start < 1000, "range too large: {range:?}");
    }

    #[test]
    fn visible_range_advances_with_scroll() {
        let layout = GridLayout::new(2_000_000, 256.0, 1280.0);
        let top = layout.visible_range(0.0, 800.0, 0);
        let scrolled = layout.visible_range(10_000.0, 800.0, 0);
        assert!(scrolled.start > top.start);
    }

    #[test]
    fn visible_range_clamped_to_cell_count() {
        let layout = GridLayout::new(10, 256.0, 1280.0);
        let range = layout.visible_range(0.0, 800.0, 0);
        assert!(range.end <= 10);
    }

    #[test]
    fn overscan_extends_range_both_directions() {
        let layout = GridLayout::new(2_000_000, 256.0, 1280.0);
        let plain = layout.visible_range(50_000.0, 800.0, 0);
        let overscanned = layout.visible_range(50_000.0, 800.0, 3);
        assert!(overscanned.start <= plain.start);
        assert!(overscanned.end >= plain.end);
    }
}
