//! The justified grid's layout build (#656, ADR-0437 section 3): a Pounce CPU job that reads the
//! visible list's sizes/orientations/capture days in one catalog scan ([`CatalogStore::hunt_layout`])
//! and streams them into a [`RowIndex`] in chunks, so a 1M-photo library never blocks the UI thread
//! and a cancel takes effect between chunks. The UI thread then only does binary searches into the
//! finished index; it never lays anything out.

use std::sync::{Arc, Mutex};

use nicti_lair::pounce_jobs::ReportSlot;
use nicti_lair::{CatalogStore, Filter, LayoutItem, Sort};
use nicti_pounce::{ChunkedJob, JobError, JobKind, JobSpec, Lane, Priority, Progress, Step};

use super::justified::{oriented_aspect, IndexKey, RowIndex, RowParams};

/// Cells pushed into the index per `step()` after the scan: a few ms at most, so a cancel (a newer
/// width bucket, a new query) lands quickly.
const CELLS_PER_STEP: usize = 65_536;

/// The label of the header over photos with no capture time.
pub const NO_DATE_LABEL: &str = "No date";

/// A finished layout: the row table plus the per-cell data it was built from.
pub struct BuiltLayout {
    /// What this was built for. The generation inside is the session's id-list generation.
    pub key: IndexKey,
    /// The ids in the order the cells were pushed; checked against the session's list once when
    /// the layout lands, so the view can trust `key.generation` afterwards.
    pub ids: Vec<i64>,
    /// Display aspect (width / height, orientation applied) per cell.
    pub aspects: Vec<f32>,
    pub index: RowIndex,
    /// Header text per group id (`RowIndex::header_of_row`); empty when grouping is off.
    pub groups: Vec<String>,
}

impl BuiltLayout {
    pub fn header_label(&self, group: u32) -> &str {
        self.groups.get(group as usize).map_or("", String::as_str)
    }
}

pub type LayoutResult = Result<BuiltLayout, String>;

/// One scan plus a chunked index build. `step()` never returns `Err`: a failure goes in the slot
/// (the scheduler drops an erroring job without touching its slot).
pub struct LayoutJob {
    store: Arc<dyn CatalogStore + Send + Sync>,
    filter: Filter,
    sort: Sort,
    key: IndexKey,
    params: RowParams,
    grouped: bool,
    state: State,
    result: ReportSlot<LayoutResult>,
}

enum State {
    Scan,
    Build(Box<Build>),
    Done,
}

struct Build {
    items: Vec<LayoutItem>,
    aspects: Vec<f32>,
    index: RowIndex,
    groups: Vec<String>,
    /// The next item to push.
    next: usize,
    /// The day of the open group (`None` = no group opened yet; `Some(None)` = the undated group).
    day: Option<Option<String>>,
    /// First cell of the aspects not yet pushed to the index.
    pushed: usize,
}

impl LayoutJob {
    pub fn new(
        store: Arc<dyn CatalogStore + Send + Sync>,
        filter: Filter,
        sort: Sort,
        key: IndexKey,
        params: RowParams,
    ) -> (Self, ReportSlot<LayoutResult>) {
        let result = Arc::new(Mutex::new(None));
        let job = LayoutJob {
            store,
            filter,
            sort,
            key,
            params: IndexKey::bucketed(params),
            grouped: key.grouped,
            state: State::Scan,
            result: result.clone(),
        };
        (job, result)
    }

    fn finish(&mut self, outcome: LayoutResult) {
        *self.result.lock().unwrap() = Some(outcome);
        self.state = State::Done;
    }
}

impl Build {
    fn new(items: Vec<LayoutItem>, params: RowParams) -> Self {
        let aspects = items
            .iter()
            .map(|i| oriented_aspect(i.width, i.height, i.orientation))
            .collect();
        Build {
            items,
            aspects,
            index: RowIndex::new(params),
            groups: Vec::new(),
            next: 0,
            day: None,
            pushed: 0,
        }
    }

    /// Pushes up to `limit` more cells; `true` when every item has been pushed.
    fn advance(&mut self, grouped: bool, limit: usize) -> Result<bool, String> {
        let end = (self.next + limit).min(self.items.len());
        while self.next < end {
            if grouped {
                let day = self.items[self.next].day.clone();
                if self.day.as_ref() != Some(&day) {
                    self.flush()?;
                    let label = day.clone().unwrap_or_else(|| NO_DATE_LABEL.to_string());
                    self.index.begin_group(Some(self.groups.len() as u32));
                    self.groups.push(label);
                    self.day = Some(day);
                }
            }
            self.next += 1;
        }
        if self.next == self.items.len() {
            self.flush()?;
            self.index.finish();
            return Ok(true);
        }
        // Push what is complete so far, so the open row never waits on the whole list.
        self.flush()?;
        Ok(false)
    }

    /// Pushes the aspects of the cells seen but not yet pushed.
    fn flush(&mut self) -> Result<(), String> {
        if self.pushed < self.next {
            self.index
                .push_cells(self.pushed, &self.aspects[self.pushed..self.next])
                .map_err(|e| format!("layout push failed: {e:?}"))?;
            self.pushed = self.next;
        }
        Ok(())
    }
}

impl ChunkedJob for LayoutJob {
    fn spec(&self) -> JobSpec {
        JobSpec {
            priority: Priority::Background,
            kind: JobKind::Snapshot,
            lane: Lane::Cpu,
            vram_bytes: 0,
            image_index: None,
        }
    }

    fn label(&self) -> String {
        "Library: laying out the grid".to_string()
    }

    fn progress(&self) -> Progress {
        match &self.state {
            State::Scan => Progress {
                done: 0,
                total: None,
            },
            State::Build(b) => Progress {
                done: b.next as u64,
                total: Some(b.items.len() as u64),
            },
            State::Done => Progress {
                done: 1,
                total: Some(1),
            },
        }
    }

    fn step(&mut self) -> Result<Step, JobError> {
        match std::mem::replace(&mut self.state, State::Done) {
            State::Scan => match self.store.hunt_layout(&self.filter, self.sort) {
                Ok(items) => {
                    self.state = State::Build(Box::new(Build::new(items, self.params)));
                    Ok(Step::Yield)
                }
                Err(e) => {
                    self.finish(Err(e.to_string()));
                    Ok(Step::Done)
                }
            },
            State::Build(mut b) => match b.advance(self.grouped, CELLS_PER_STEP) {
                Ok(true) => {
                    let ids = b.items.iter().map(|i| i.id).collect();
                    self.finish(Ok(BuiltLayout {
                        key: self.key,
                        ids,
                        aspects: b.aspects,
                        index: b.index,
                        groups: b.groups,
                    }));
                    Ok(Step::Done)
                }
                Ok(false) => {
                    self.state = State::Build(b);
                    Ok(Step::Yield)
                }
                Err(msg) => {
                    self.finish(Err(msg));
                    Ok(Step::Done)
                }
            },
            State::Done => Ok(Step::Done),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nicti_lair::{SortDirection, SortField, SqliteCatalog};

    use crate::test_support::seed_assets;

    fn params() -> RowParams {
        RowParams {
            width: 800.0,
            target_height: 100.0,
            gap: 4.0,
            header_height: 24.0,
        }
    }

    fn sort() -> Sort {
        Sort {
            field: SortField::Captured,
            direction: SortDirection::Asc,
        }
    }

    fn run(job: &mut LayoutJob) -> usize {
        let mut steps = 0;
        while job.step().unwrap() == Step::Yield {
            steps += 1;
        }
        steps
    }

    fn item(id: i64, day: Option<&str>) -> LayoutItem {
        LayoutItem {
            id,
            width: Some(300),
            height: Some(200),
            orientation: None,
            day: day.map(str::to_string),
        }
    }

    #[test]
    fn a_catalog_scan_builds_a_complete_index_over_every_id_in_order() {
        let catalog = Arc::new(SqliteCatalog::open_in_memory().unwrap());
        seed_assets(&catalog, 300);
        let store: Arc<dyn CatalogStore + Send + Sync> = catalog;
        let expected = store.hunt_ids(&Filter::default(), sort()).unwrap();
        let key = IndexKey::new(7, false, params());
        let (mut job, slot) = LayoutJob::new(store, Filter::default(), sort(), key, params());
        run(&mut job);
        let built = slot.lock().unwrap().take().unwrap().unwrap();
        assert_eq!(built.ids, expected);
        assert_eq!(built.key.generation, 7);
        assert!(built.index.is_complete());
        assert_eq!(built.index.cells(), 300);
        assert_eq!(built.aspects.len(), 300);
        assert!(built.groups.is_empty(), "no headers when grouping is off");
    }

    #[test]
    fn chunking_does_not_change_the_result() {
        // More cells than one step handles, so the build crosses a step boundary mid-group.
        let n = CELLS_PER_STEP + 1000;
        let items: Vec<LayoutItem> = (0..n)
            .map(|i| {
                item(
                    i as i64,
                    Some(if i < n / 2 {
                        "2026-01-01"
                    } else {
                        "2026-01-02"
                    }),
                )
            })
            .collect();
        let build = |chunk: usize| {
            let mut b = Build::new(items.clone(), IndexKey::bucketed(params()));
            while !b.advance(true, chunk).unwrap() {}
            b
        };
        let (one, many) = (build(n), build(997));
        assert_eq!(one.index.rows(), many.index.rows());
        assert_eq!(one.index.content_height(), many.index.content_height());
        assert_eq!(one.groups, many.groups);
        assert_eq!(one.groups, ["2026-01-01", "2026-01-02"]);
        for row in [0, 1, one.index.rows() / 2, one.index.rows() - 1] {
            assert_eq!(one.index.first_cell(row), many.index.first_cell(row));
            assert_eq!(one.index.header_of_row(row), many.index.header_of_row(row));
        }
    }

    #[test]
    fn days_become_groups_with_a_header_and_undated_photos_get_their_own() {
        let items = vec![
            item(1, None),
            item(2, None),
            item(3, Some("2026-01-01")),
            item(4, Some("2026-01-01")),
            item(5, Some("2026-01-02")),
        ];
        let mut b = Build::new(items, IndexKey::bucketed(params()));
        assert!(b.advance(true, 100).unwrap());
        assert_eq!(b.groups, [NO_DATE_LABEL, "2026-01-01", "2026-01-02"]);
        let idx = &b.index;
        // A group never shares a row with the next one: three groups, three rows here.
        assert_eq!(idx.rows(), 3);
        assert_eq!(
            (0..3).map(|r| idx.header_of_row(r)).collect::<Vec<_>>(),
            [Some(0), Some(1), Some(2)]
        );
        assert_eq!(idx.cells(), 5);
    }

    #[test]
    fn ungrouped_has_no_headers_even_when_the_days_differ() {
        let items = vec![item(1, Some("2026-01-01")), item(2, Some("2026-01-02"))];
        let mut b = Build::new(items, IndexKey::bucketed(params()));
        assert!(b.advance(false, 100).unwrap());
        assert!(b.groups.is_empty());
        assert_eq!(b.index.rows(), 1, "both photos share a row");
        assert_eq!(b.index.header_of_row(0), None);
    }

    #[test]
    fn a_rotated_photo_is_laid_out_by_its_displayed_shape() {
        let mut portrait = item(1, None);
        portrait.orientation = Some(6); // stored 300x200, displayed 200x300
        let b = Build::new(vec![portrait], IndexKey::bucketed(params()));
        assert!(
            (b.aspects[0] - 200.0 / 300.0).abs() < 1e-6,
            "{:?}",
            b.aspects
        );
    }

    #[test]
    fn an_empty_list_finishes_with_an_empty_complete_index() {
        let mut b = Build::new(Vec::new(), IndexKey::bucketed(params()));
        assert!(b.advance(true, 10).unwrap());
        assert!(b.index.is_complete());
        assert_eq!(b.index.rows(), 0);
    }
}
