//! Justified rows with date-group headers: the pure core (#550, ADR-0437 section 3).
//!
//! The fixed square grid (`layout.rs`) is O(1) arithmetic; a justified layout is not: where row `r`
//! starts depends on every aspect ratio before it. So the layout is a [`RowIndex`], an append-only
//! table of rows (`first` cell, `top`, `height`, optional group header) that is *streamed* into by
//! a background job ([`RowIndex::begin_group`], [`RowIndex::push_cells`], [`RowIndex::finish`]) in
//! any chunking, and queried by binary search (`partition_point`) from the UI thread. A frame never
//! lays anything out: it asks which rows overlap the viewport and then positions only those rows'
//! cells ([`RowIndex::row_cells`]). At 1M photos with ~8 per row the table is ~1.3 MB, built once
//! per (generation, width bucket, target height) -- see [`LayoutGate`] -- not per frame or per pixel
//! of a live resize.
//!
//! No egui, no catalog: aspects come from the caller. **Catalog `width`/`height` are not
//! orientation-corrected** (a portrait NEF's preview is stored landscape with an EXIF rotation;
//! JPEG assets carry the file's own size), so the caller must feed [`oriented_aspect`], not raw
//! width/height; where the orientation comes from is the open follow-up (#604).

// Nothing calls this yet: `view.rs` still draws the square grid, and wiring this in (a custom viewport
// loop, the Pounce build job, the view toggle) is #604. The module is complete and tested on its own.
#![allow(dead_code)]

use std::ops::Range;

/// Width of the bucket a live resize is rounded down to, so dragging the window edge does not
/// rebuild the index per pixel.
pub const WIDTH_BUCKET: f32 = 16.0;
/// A justified row is stretched to the full width, but never taller than this many times the
/// target height (a row of one panorama would otherwise become enormous).
pub const MAX_STRETCH: f32 = 1.5;
const MIN_ASPECT: f32 = 0.1;
const MAX_ASPECT: f32 = 10.0;
const NO_HEADER: u32 = u32::MAX;

/// A width/height ratio the layout can trust: non-finite or non-positive becomes square, and the
/// extremes are clamped so one corrupt row cannot produce a 1-pixel-high or 100-row-high strip.
pub fn clean_aspect(a: f32) -> f32 {
    if a.is_finite() && a > 0.0 {
        a.clamp(MIN_ASPECT, MAX_ASPECT)
    } else {
        1.0
    }
}

/// The displayed aspect (width / height) of a photo whose stored size is `width` x `height` and whose
/// EXIF `orientation` (1-8) says how it is rotated for display: 5-8 are the 90-degree cases and swap
/// the axes. Missing or zero dimensions are square.
pub fn oriented_aspect(width: Option<u32>, height: Option<u32>, orientation: Option<u8>) -> f32 {
    let (Some(w), Some(h)) = (width, height) else {
        return 1.0;
    };
    if w == 0 || h == 0 {
        return 1.0;
    }
    let a = w as f32 / h as f32;
    clean_aspect(if matches!(orientation, Some(5..=8)) {
        1.0 / a
    } else {
        a
    })
}

/// `width` rounded down to a [`WIDTH_BUCKET`] multiple (at least one bucket), as the bucket count.
pub fn width_bucket(width: f32) -> u32 {
    if width.is_finite() && width > 0.0 {
        ((width / WIDTH_BUCKET).floor() as u32).max(1)
    } else {
        1
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RowParams {
    /// Row width in points (use `width_bucket(w) as f32 * WIDTH_BUCKET`).
    pub width: f32,
    /// The height a row aims for.
    pub target_height: f32,
    /// Space between cells and between rows.
    pub gap: f32,
    /// Extra height above the first row of a group that has a header.
    pub header_height: f32,
}

impl RowParams {
    fn sane(self) -> Self {
        let pos = |v: f32, d: f32| if v.is_finite() && v > 0.0 { v } else { d };
        Self {
            width: pos(self.width, WIDTH_BUCKET),
            target_height: pos(self.target_height, 1.0),
            gap: if self.gap.is_finite() && self.gap >= 0.0 {
                self.gap
            } else {
                0.0
            },
            header_height: if self.header_height.is_finite() && self.header_height >= 0.0 {
                self.header_height
            } else {
                0.0
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CellRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutOfOrder {
    pub expected: usize,
    pub got: usize,
}

#[derive(Debug, Clone)]
struct Open {
    start: usize,
    count: u32,
    sum: f32,
    header: u32,
}

/// The justified layout of one ordered list of photos.
#[derive(Debug, Clone)]
pub struct RowIndex {
    params: RowParams,
    first: Vec<u32>,
    top: Vec<f32>,
    height: Vec<f32>,
    header: Vec<u32>,
    open: Option<Open>,
    /// Header for the next row that opens (set by `begin_group`).
    pending_header: u32,
    /// Cells consumed so far (closed rows plus the open one).
    cells: usize,
    y: f32,
    complete: bool,
}

impl RowIndex {
    pub fn new(params: RowParams) -> Self {
        Self {
            params: params.sane(),
            first: Vec::new(),
            top: Vec::new(),
            height: Vec::new(),
            header: Vec::new(),
            open: None,
            pending_header: NO_HEADER,
            cells: 0,
            y: 0.0,
            complete: false,
        }
    }

    pub fn params(&self) -> RowParams {
        self.params
    }

    /// Starts a new group: the open row is closed (a group's last row is not stretched), and the
    /// next row gets a header above it when `header` is `Some(group id)`.
    pub fn begin_group(&mut self, header: Option<u32>) {
        self.close_open(false);
        self.pending_header = header.filter(|h| *h != NO_HEADER).unwrap_or(NO_HEADER);
    }

    /// Appends the aspects of cells `first_cell..first_cell + aspects.len()`. May be called any
    /// number of times with any chunking; the result is identical to one call.
    pub fn push_cells(&mut self, first_cell: usize, aspects: &[f32]) -> Result<(), OutOfOrder> {
        if first_cell != self.cells {
            return Err(OutOfOrder {
                expected: self.cells,
                got: first_cell,
            });
        }
        let RowParams {
            width,
            target_height,
            gap,
            ..
        } = self.params;
        for &raw in aspects {
            let a = clean_aspect(raw);
            if let Some(open) = &self.open {
                let n = open.count + 1;
                let needed = (open.sum + a) * target_height + gap * (n - 1) as f32;
                if needed > width {
                    self.close_open(true);
                }
            }
            let header = std::mem::replace(&mut self.pending_header, NO_HEADER);
            let open = self.open.get_or_insert(Open {
                start: self.cells,
                count: 0,
                sum: 0.0,
                header,
            });
            open.count += 1;
            open.sum += a;
            self.cells += 1;
        }
        Ok(())
    }

    /// Closes the last row. Until this is called, the final partial row is not addressable.
    pub fn finish(&mut self) {
        self.close_open(false);
        self.complete = true;
    }

    fn close_open(&mut self, justified: bool) {
        let Some(open) = self.open.take() else {
            return;
        };
        let RowParams {
            width,
            target_height,
            gap,
            header_height,
        } = self.params;
        let fit = (width - gap * (open.count - 1) as f32) / open.sum;
        let cap = if justified {
            target_height * MAX_STRETCH
        } else {
            target_height
        };
        let h = fit.min(cap).max(1.0);
        let head = if open.header == NO_HEADER {
            0.0
        } else {
            header_height
        };
        self.first.push(open.start as u32);
        self.top.push(self.y);
        self.height.push(h);
        self.header.push(open.header);
        self.y += head + h + gap;
    }

    /// Whether [`Self::finish`] has been called (the index covers every cell).
    pub fn is_complete(&self) -> bool {
        self.complete
    }

    /// Addressable rows (closed ones).
    pub fn rows(&self) -> usize {
        self.first.len()
    }

    /// Cells covered by the addressable rows.
    pub fn cells(&self) -> usize {
        self.open.as_ref().map_or(self.cells, |o| o.start)
    }

    /// Total height of the addressable rows.
    pub fn content_height(&self) -> f32 {
        self.y
    }

    pub fn row_top(&self, row: usize) -> f32 {
        self.top[row]
    }

    /// Height of the photos in `row` (without its header).
    pub fn row_height(&self, row: usize) -> f32 {
        self.height[row]
    }

    /// The group header above `row`, if it opens a group that has one.
    pub fn header_of_row(&self, row: usize) -> Option<u32> {
        Some(self.header[row]).filter(|h| *h != NO_HEADER)
    }

    fn header_offset(&self, row: usize) -> f32 {
        if self.header[row] == NO_HEADER {
            0.0
        } else {
            self.params.header_height
        }
    }

    /// The first cell of `row`.
    pub fn first_cell(&self, row: usize) -> usize {
        self.first[row] as usize
    }

    /// One past the last cell of `row`.
    pub fn end_cell(&self, row: usize) -> usize {
        self.first
            .get(row + 1)
            .map_or_else(|| self.cells(), |f| *f as usize)
    }

    /// The row whose block (header included) contains `y`, clamped to the first/last row.
    pub fn row_at_y(&self, y: f32) -> usize {
        if self.rows() == 0 {
            return 0;
        }
        self.top
            .partition_point(|t| *t <= y)
            .saturating_sub(1)
            .min(self.rows() - 1)
    }

    /// The rows overlapping `y0..y1` (a viewport), plus nothing else.
    pub fn rows_between(&self, y0: f32, y1: f32) -> Range<usize> {
        if self.rows() == 0 || y1 <= y0 {
            return 0..0;
        }
        let start = self.row_at_y(y0);
        let end = self.top.partition_point(|t| *t < y1).max(start + 1);
        start..end.min(self.rows())
    }

    /// The row holding cell `index`.
    pub fn row_of(&self, index: usize) -> Option<usize> {
        if index >= self.cells() {
            return None;
        }
        Some(self.first.partition_point(|f| (*f as usize) <= index) - 1)
    }

    /// The cell range covered by `rows`, clamped to the addressable cells.
    pub fn indices_for_rows(&self, rows: Range<usize>) -> Range<usize> {
        let rows_end = rows.end.min(self.rows());
        let rows_start = rows.start.min(rows_end);
        if rows_start == rows_end {
            return 0..0;
        }
        self.first_cell(rows_start)..self.end_cell(rows_end - 1)
    }

    /// The rectangles of `row`'s cells, relative to the content's top-left. `aspects` is indexed by
    /// cell, the same slice that was pushed.
    pub fn row_cells<'a>(
        &'a self,
        row: usize,
        aspects: &'a [f32],
    ) -> impl Iterator<Item = (usize, CellRect)> + 'a {
        let y = self.top[row] + self.header_offset(row);
        let h = self.height[row];
        let gap = self.params.gap;
        let mut x = 0.0;
        (self.first_cell(row)..self.end_cell(row)).map(move |i| {
            let w = clean_aspect(aspects.get(i).copied().unwrap_or(1.0)) * h;
            let rect = CellRect { x, y, w, h };
            x += w + gap;
            (i, rect)
        })
    }

    /// The cell of `row` whose horizontal span contains `x`, else the nearest one.
    fn cell_at_x(&self, row: usize, x: f32, aspects: &[f32]) -> usize {
        let mut best = (self.first_cell(row), f32::INFINITY);
        for (i, r) in self.row_cells(row, aspects) {
            let d = if x < r.x {
                r.x - x
            } else if x > r.x + r.w {
                x - (r.x + r.w)
            } else {
                0.0
            };
            if d < best.1 {
                best = (i, d);
            }
        }
        best.0
    }

    /// Arrow-key navigation by row: the cell in the row above/below nearest the current cell's
    /// horizontal centre. Stays put at the first/last row, and for an index not yet addressable.
    pub fn navigate(&self, index: usize, dir: Dir, aspects: &[f32]) -> usize {
        let Some(row) = self.row_of(index) else {
            return index;
        };
        let target = match dir {
            Dir::Up if row > 0 => row - 1,
            Dir::Down if row + 1 < self.rows() => row + 1,
            _ => return index,
        };
        self.cell_at_x(target, self.centre_x(row, index, aspects), aspects)
    }

    /// Page-key navigation: the row about `view_height` above/below, same horizontal position.
    pub fn navigate_page(
        &self,
        index: usize,
        dir: Dir,
        view_height: f32,
        aspects: &[f32],
    ) -> usize {
        let Some(row) = self.row_of(index) else {
            return index;
        };
        let y = self.top[row]
            + match dir {
                Dir::Up => -view_height,
                Dir::Down => view_height,
            };
        let target = self.row_at_y(y);
        if target == row {
            return index;
        }
        self.cell_at_x(target, self.centre_x(row, index, aspects), aspects)
    }

    fn centre_x(&self, row: usize, index: usize, aspects: &[f32]) -> f32 {
        self.row_cells(row, aspects)
            .find(|(i, _)| *i == index)
            .map_or(0.0, |(_, r)| r.x + r.w / 2.0)
    }
}

/// What a built [`RowIndex`] is for. A new key means a rebuild; the same key never does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexKey {
    /// The visible-list generation (changes with the query, sort or catalog contents).
    pub generation: u64,
    pub width_bucket: u32,
    /// `f32::to_bits` of the target height.
    pub target_bits: u32,
    /// Whether date headers are on.
    pub grouped: bool,
}

/// Decides when a layout build must start and counts them (`layout_builds`, ADR-0437): a frame with
/// an unchanged key must not build, which a test asserts stays flat.
#[derive(Debug, Default)]
pub struct LayoutGate {
    key: Option<IndexKey>,
    pub layout_builds: u64,
}

impl LayoutGate {
    /// `true` when `key` differs from the last one asked for, i.e. a build must be started now.
    pub fn request(&mut self, key: IndexKey) -> bool {
        if self.key == Some(key) {
            return false;
        }
        self.key = Some(key);
        self.layout_builds += 1;
        true
    }

    /// Forget the key (the build was abandoned), so the next request builds again.
    pub fn invalidate(&mut self) {
        self.key = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(width: f32) -> RowParams {
        RowParams {
            width,
            target_height: 100.0,
            gap: 4.0,
            header_height: 30.0,
        }
    }

    /// Deterministic aspects between 0.6 and 1.9, no rng dependency.
    fn aspects(n: usize) -> Vec<f32> {
        let mut s = 0x9E37_79B9u32;
        (0..n)
            .map(|_| {
                s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                0.6 + (s >> 8) as f32 / (1u32 << 24) as f32 * 1.3
            })
            .collect()
    }

    fn built(width: f32, a: &[f32]) -> RowIndex {
        let mut idx = RowIndex::new(params(width));
        idx.begin_group(None);
        idx.push_cells(0, a).unwrap();
        idx.finish();
        idx
    }

    #[test]
    fn full_rows_fill_the_width_and_the_last_row_is_not_stretched() {
        let a = aspects(500);
        let idx = built(800.0, &a);
        assert_eq!(idx.indices_for_rows(0..idx.rows()), 0..500);
        for row in 0..idx.rows() - 1 {
            let cells: Vec<_> = idx.row_cells(row, &a).collect();
            let last = cells.last().unwrap().1;
            let used = last.x + last.w;
            let stretched = idx.row_height(row) >= 100.0 * MAX_STRETCH - 0.01;
            assert!(
                stretched || (used - 800.0).abs() < 0.05,
                "row {row} ends at {used}, height {}",
                idx.row_height(row)
            );
            assert!(idx.row_height(row) <= 100.0 * MAX_STRETCH + 0.01);
        }
        let last = idx.rows() - 1;
        assert!(
            idx.row_height(last) <= 100.0 + 0.01,
            "the last row keeps the target"
        );
    }

    #[test]
    fn streaming_in_any_chunking_gives_the_same_index_as_one_call() {
        let a = aspects(1000);
        let whole = built(640.0, &a);
        for chunk in [1usize, 7, 64, 999] {
            let mut idx = RowIndex::new(params(640.0));
            idx.begin_group(None);
            let mut at = 0;
            for c in a.chunks(chunk) {
                idx.push_cells(at, c).unwrap();
                at += c.len();
            }
            idx.finish();
            assert_eq!(idx.rows(), whole.rows(), "chunk {chunk}");
            assert_eq!(idx.first, whole.first, "chunk {chunk}");
            assert_eq!(idx.top, whole.top, "chunk {chunk}");
            assert_eq!(idx.height, whole.height, "chunk {chunk}");
        }
    }

    #[test]
    fn a_single_very_wide_photo_shrinks_to_fit_and_a_corrupt_aspect_is_square() {
        let idx = built(300.0, &[10.0, 1.0]);
        // 10.0 is the clamp; alone in its row it must not overflow the width.
        let a = [10.0, 1.0];
        let (_, r) = idx.row_cells(0, &a).next().unwrap();
        assert!(r.x + r.w <= 300.0 + 0.01, "{r:?}");
        for bad in [f32::NAN, 0.0, -2.0, f32::INFINITY] {
            assert_eq!(clean_aspect(bad), 1.0, "{bad}");
        }
        assert_eq!(clean_aspect(1e9), MAX_ASPECT);
        assert_eq!(clean_aspect(1e-9), MIN_ASPECT);
    }

    #[test]
    fn orientation_swaps_the_axes_for_the_rotated_cases_only() {
        let landscape = |o| oriented_aspect(Some(6000), Some(4000), o);
        assert!((landscape(None) - 1.5).abs() < 1e-6);
        assert!((landscape(Some(1)) - 1.5).abs() < 1e-6);
        assert!(
            (landscape(Some(3)) - 1.5).abs() < 1e-6,
            "180 degrees keeps the aspect"
        );
        for o in 5..=8 {
            assert!(
                (landscape(Some(o)) - 1.0 / 1.5).abs() < 1e-6,
                "orientation {o}"
            );
        }
        assert_eq!(oriented_aspect(None, Some(10), Some(6)), 1.0);
        assert_eq!(oriented_aspect(Some(0), Some(10), None), 1.0);
    }

    #[test]
    fn groups_start_a_new_row_and_carry_their_header_space() {
        let a = vec![1.0f32; 6];
        let mut idx = RowIndex::new(params(1000.0)); // plenty of room: one row per group
        idx.begin_group(Some(7));
        idx.push_cells(0, &a[..2]).unwrap();
        idx.begin_group(Some(9));
        idx.push_cells(2, &a[2..5]).unwrap();
        idx.begin_group(None);
        idx.push_cells(5, &a[5..]).unwrap();
        idx.finish();

        assert_eq!(idx.rows(), 3);
        assert_eq!(idx.header_of_row(0), Some(7));
        assert_eq!(idx.header_of_row(1), Some(9));
        assert_eq!(idx.header_of_row(2), None);
        assert_eq!(idx.first_cell(1), 2);
        assert_eq!(idx.first_cell(2), 5);
        // Row 0 block = header 30 + height 100 + gap 4; row 1 starts there; row 2 (no header) after.
        assert_eq!(idx.row_top(0), 0.0);
        assert_eq!(idx.row_top(1), 134.0);
        assert_eq!(idx.row_top(2), 268.0);
        assert_eq!(idx.content_height(), 268.0 + 100.0 + 4.0);
        // Cells sit below their header.
        let (_, r) = idx.row_cells(1, &a).next().unwrap();
        assert_eq!(r.y, 134.0 + 30.0);
    }

    #[test]
    fn lookups_agree_with_brute_force_on_a_large_list() {
        let a = aspects(5000);
        let mut idx = RowIndex::new(params(900.0));
        // Date groups of uneven sizes, headers on.
        let mut at = 0;
        for (g, len) in (0..).zip([1usize, 40, 700, 3, 1500, 2756]) {
            idx.begin_group(Some(g));
            idx.push_cells(at, &a[at..at + len]).unwrap();
            at += len;
        }
        idx.finish();
        assert_eq!(at, 5000);
        assert_eq!(idx.cells(), 5000);

        // Rows tile the cells exactly and tops strictly increase.
        let mut next = 0;
        for row in 0..idx.rows() {
            assert_eq!(idx.first_cell(row), next);
            next = idx.end_cell(row);
            assert!(row == 0 || idx.row_top(row) > idx.row_top(row - 1));
        }
        assert_eq!(next, 5000);

        for i in (0..5000).step_by(37) {
            let row = idx.row_of(i).unwrap();
            assert!(
                (idx.first_cell(row)..idx.end_cell(row)).contains(&i),
                "cell {i}"
            );
        }
        assert_eq!(idx.row_of(5000), None);

        // rows_between returns exactly the rows whose blocks overlap the viewport.
        let h = idx.content_height();
        for k in 0..60 {
            let y0 = h * k as f32 / 61.0;
            let y1 = y0 + 700.0;
            let got = idx.rows_between(y0, y1);
            let want: Vec<usize> = (0..idx.rows())
                .filter(|&r| {
                    let top = idx.row_top(r);
                    let bottom = top + idx.header_offset(r) + idx.row_height(r) + idx.params().gap;
                    top < y1 && bottom > y0
                })
                .collect();
            assert_eq!(
                (got.start, got.end.saturating_sub(1)),
                (*want.first().unwrap(), *want.last().unwrap()),
                "viewport {y0}..{y1}"
            );
        }
        assert!(idx.rows_between(10.0, 10.0).is_empty());
        assert!(
            idx.rows_between(0.0, 100.0).len() < 6,
            "a viewport touches few rows"
        );
    }

    #[test]
    fn a_y_exactly_on_a_row_top_belongs_to_that_row_and_just_above_to_the_previous() {
        let a = aspects(400);
        let idx = built(700.0, &a);
        for row in 0..idx.rows() {
            let top = idx.row_top(row);
            assert_eq!(idx.row_at_y(top), row, "row {row}");
            if row > 0 {
                assert_eq!(idx.row_at_y(top - 0.01), row - 1, "just above row {row}");
            }
        }
        assert_eq!(idx.row_at_y(-50.0), 0, "above the content clamps");
        assert_eq!(
            idx.row_at_y(1e9),
            idx.rows() - 1,
            "below the content clamps"
        );
    }

    #[test]
    fn a_stretched_row_is_capped_so_a_lone_photo_does_not_become_huge() {
        // [1.0, 10.0]: the wide one overflows, so row 0 is the lone square, which would stretch to
        // 1000 high to fill the width. It is capped at MAX_STRETCH x target and left-aligned.
        let a = [1.0, 10.0];
        let idx = built(1000.0, &a);
        assert_eq!(idx.row_height(0), 100.0 * MAX_STRETCH);
        let (_, r) = idx.row_cells(0, &a).next().unwrap();
        assert!(r.x + r.w < 1000.0, "not stretched to the full width: {r:?}");
    }

    #[test]
    fn out_of_order_and_unfinished_rows() {
        let mut idx = RowIndex::new(params(500.0));
        idx.begin_group(None);
        assert_eq!(
            idx.push_cells(3, &[1.0]),
            Err(OutOfOrder {
                expected: 0,
                got: 3
            })
        );
        idx.push_cells(0, &[1.0, 1.0]).unwrap();
        // The open row is not addressable until it closes.
        assert_eq!(idx.rows(), 0);
        assert_eq!(idx.row_of(0), None);
        assert!(!idx.is_complete());
        idx.finish();
        assert!(idx.is_complete());
        assert_eq!(idx.rows(), 1);
        assert_eq!(idx.row_of(1), Some(0));
        // An empty index answers every query without panicking.
        let empty = RowIndex::new(params(500.0));
        assert_eq!(empty.row_at_y(10.0), 0);
        assert!(empty.rows_between(0.0, 100.0).is_empty());
        assert!(empty.indices_for_rows(0..5).is_empty());
        assert_eq!(empty.navigate(0, Dir::Down, &[]), 0);
    }

    #[test]
    fn arrow_navigation_lands_under_the_current_cell_and_stops_at_the_ends() {
        // Rows of different shapes so a naive index +/- columns would land wrong.
        let a = vec![
            2.0, 2.0, 2.0, // row 0: 3 wide cells
            0.5, 0.5, 0.5, 0.5, 0.5, 0.5, 0.5, 0.5, 0.5, 0.5, 0.5,
            0.5, // row 1: many narrow cells
            1.0, 1.0, // row 2 (last)
        ];
        let mut idx = RowIndex::new(RowParams {
            width: 640.0,
            target_height: 100.0,
            gap: 0.0,
            header_height: 0.0,
        });
        idx.begin_group(None);
        idx.push_cells(0, &a).unwrap();
        idx.finish();
        assert!(idx.rows() >= 3, "rows: {}", idx.rows());
        let first_of_row1 = idx.first_cell(1);
        // Down from the middle of row 0 lands in row 1, at about the same x.
        let down = idx.navigate(1, Dir::Down, &a);
        assert_eq!(idx.row_of(down), Some(1));
        let cx = |row: usize, i: usize| {
            let (_, r) = idx.row_cells(row, &a).find(|(c, _)| *c == i).unwrap();
            r.x + r.w / 2.0
        };
        let wanted = cx(0, 1);
        let got = idx.row_cells(1, &a).find(|(c, _)| *c == down).unwrap().1;
        assert!(
            wanted >= got.x - 0.01 && wanted <= got.x + got.w + 0.01,
            "{wanted} vs {got:?}"
        );
        // Up from there returns to row 0; the ends are sticky.
        assert_eq!(idx.row_of(idx.navigate(down, Dir::Up, &a)), Some(0));
        assert_eq!(idx.navigate(0, Dir::Up, &a), 0);
        let last = a.len() - 1;
        assert_eq!(idx.navigate(last, Dir::Down, &a), last);
        assert!(first_of_row1 > 0);
        // A page jump moves several rows and also stays inside.
        let far = idx.navigate_page(0, Dir::Down, 10_000.0, &a);
        assert_eq!(idx.row_of(far), Some(idx.rows() - 1));
        assert_eq!(idx.navigate_page(0, Dir::Up, 10_000.0, &a), 0);
    }

    #[test]
    fn a_resize_within_a_width_bucket_does_not_rebuild_and_unchanged_frames_never_do() {
        assert_eq!(width_bucket(799.0), 49);
        assert_eq!(width_bucket(800.0), 50);
        assert_eq!(width_bucket(815.9), 50);
        assert_eq!(width_bucket(0.0), 1);
        assert_eq!(width_bucket(f32::NAN), 1);

        let key = |w: f32, generation| IndexKey {
            generation,
            width_bucket: width_bucket(w),
            target_bits: 180.0f32.to_bits(),
            grouped: true,
        };
        let mut gate = LayoutGate::default();
        assert!(gate.request(key(800.0, 1)), "first frame builds");
        for w in [800.0, 803.0, 810.0, 815.0] {
            assert!(!gate.request(key(w, 1)), "width {w} is the same bucket");
        }
        assert_eq!(gate.layout_builds, 1, "unchanged frames never build");
        assert!(gate.request(key(816.0, 1)), "next bucket");
        assert!(
            gate.request(key(816.0, 2)),
            "new generation (query/sort changed)"
        );
        let mut other = key(816.0, 2);
        other.grouped = false;
        assert!(gate.request(other), "toggling headers");
        assert_eq!(gate.layout_builds, 4);
        gate.invalidate();
        assert!(gate.request(other), "an abandoned build is redone");
    }

    #[test]
    fn a_million_photos_index_is_small_and_queries_are_logarithmic() {
        let a = aspects(1_000_000);
        let mut idx = RowIndex::new(params(1600.0));
        idx.begin_group(None);
        for (n, c) in a.chunks(10_000).enumerate() {
            idx.push_cells(n * 10_000, c).unwrap();
        }
        idx.finish();
        assert_eq!(idx.cells(), 1_000_000);
        // ~13 bytes per row (u32 + three f32) and ~8-16 cells per row.
        let bytes = idx.rows() * 16;
        assert!(
            bytes < 4 * 1024 * 1024,
            "{bytes} bytes for {} rows",
            idx.rows()
        );
        let mid = idx.content_height() / 2.0;
        let view = idx.rows_between(mid, mid + 900.0);
        assert!(view.len() < 12, "{view:?}");
        let cells = idx.indices_for_rows(view);
        assert!(
            cells.len() < 200,
            "a screenful is a screenful, not the library"
        );
    }
}
