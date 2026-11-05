//! The justified grid's layout build (#656, ADR-0437 section 3): a Pounce CPU job that looks up the
//! sizes/orientations/capture days of **the session's own id list** ([`CatalogStore::layout_items`],
//! a few thousand ids per step) and streams them into a [`RowIndex`]. A 1M-photo library never
//! blocks the UI thread, a cancel lands within one chunk, and because the list is the session's
//! (not a second query) the layout can never describe a different list or order than the grid
//! indexes it with. The UI thread then only does binary searches into the finished index.

use std::sync::{Arc, Mutex};

use nicti_lair::pounce_jobs::ReportSlot;
use nicti_lair::{CatalogStore, LayoutItem};
use nicti_pounce::{ChunkedJob, JobError, JobKind, JobSpec, Lane, Priority, Progress, Step};

use super::justified::{oriented_aspect, IndexKey, RowIndex, RowParams};

/// Ids looked up and pushed per `step()`: a few ms, so a cancel (a newer width bucket, a new
/// list) lands quickly and a superseded build releases its memory promptly.
const IDS_PER_STEP: usize = 4096;

/// The label of the header over photos with no capture time.
pub const NO_DATE_LABEL: &str = "No date";

/// A finished layout: the row table plus the per-cell data it was built from.
pub struct BuiltLayout {
    /// What this was built for. The generation inside is the session's id-list generation: the
    /// layout describes the session's list exactly while the generation still matches.
    pub key: IndexKey,
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

/// Why a build failed, and for which list generation (a failure that lands after the list moved
/// on must not be pinned to the new list).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutError {
    pub generation: u64,
    pub message: String,
}

pub type LayoutResult = Result<BuiltLayout, LayoutError>;

/// The chunked build. `step()` never returns `Err`: a failure goes in the slot (the scheduler
/// drops an erroring job without touching its slot).
pub struct LayoutJob {
    store: Arc<dyn CatalogStore + Send + Sync>,
    ids: Vec<i64>,
    next: usize,
    key: IndexKey,
    build: Option<Build>,
    done: bool,
    result: ReportSlot<LayoutResult>,
}

impl LayoutJob {
    /// `ids` is the session's list at `key.generation`.
    pub fn new(
        store: Arc<dyn CatalogStore + Send + Sync>,
        ids: Vec<i64>,
        key: IndexKey,
        params: RowParams,
    ) -> (Self, ReportSlot<LayoutResult>) {
        let result = Arc::new(Mutex::new(None));
        let job = LayoutJob {
            store,
            ids,
            next: 0,
            key,
            build: Some(Build::new(IndexKey::bucketed(params))),
            done: false,
            result: result.clone(),
        };
        (job, result)
    }

    fn fail(&mut self, message: String) {
        let generation = self.key.generation;
        self.finish(Err(LayoutError {
            generation,
            message,
        }));
    }

    fn finish(&mut self, outcome: LayoutResult) {
        *self.result.lock().unwrap_or_else(|e| e.into_inner()) = Some(outcome);
        self.done = true;
        self.build = None;
        self.ids = Vec::new();
    }
}

/// The streaming half: takes items in list order, in any chunking, with the same result.
struct Build {
    aspects: Vec<f32>,
    index: RowIndex,
    groups: Vec<String>,
    /// The day of the open group (`None` = no group opened yet; `Some(None)` = the undated group).
    day: Option<Option<String>>,
    /// First cell of the aspects not yet pushed to the index.
    pushed: usize,
}

impl Build {
    fn new(params: RowParams) -> Self {
        Build {
            aspects: Vec::new(),
            index: RowIndex::new(params),
            groups: Vec::new(),
            day: None,
            pushed: 0,
        }
    }

    fn feed(&mut self, items: &[LayoutItem], grouped: bool) -> Result<(), String> {
        for item in items {
            if grouped && self.day.as_ref() != Some(&item.day) {
                self.flush()?;
                let label = item
                    .day
                    .clone()
                    .unwrap_or_else(|| NO_DATE_LABEL.to_string());
                self.index.begin_group(Some(self.groups.len() as u32));
                self.groups.push(label);
                self.day = Some(item.day.clone());
            }
            self.aspects
                .push(oriented_aspect(item.width, item.height, item.orientation));
        }
        self.flush()
    }

    /// Pushes the aspects seen but not yet pushed.
    fn flush(&mut self) -> Result<(), String> {
        if self.pushed < self.aspects.len() {
            self.index
                .push_cells(self.pushed, &self.aspects[self.pushed..])
                .map_err(|e| format!("layout push failed: {e:?}"))?;
            self.pushed = self.aspects.len();
        }
        Ok(())
    }

    fn finish(mut self) -> (Vec<f32>, RowIndex, Vec<String>) {
        self.index.finish();
        (self.aspects, self.index, self.groups)
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
        Progress {
            done: self.next as u64,
            total: Some(self.ids.len().max(self.next) as u64),
        }
    }

    fn step(&mut self) -> Result<Step, JobError> {
        let Some(build) = self.build.as_mut() else {
            return Ok(Step::Done);
        };
        let end = (self.next + IDS_PER_STEP).min(self.ids.len());
        let items = match self.store.layout_items(&self.ids[self.next..end]) {
            Ok(items) => items,
            Err(e) => {
                self.fail(e.to_string());
                return Ok(Step::Done);
            }
        };
        if let Err(msg) = build.feed(&items, self.key.grouped) {
            self.fail(msg);
            return Ok(Step::Done);
        }
        self.next = end;
        if self.next < self.ids.len() {
            return Ok(Step::Yield);
        }
        let Some(build) = self.build.take() else {
            return Ok(Step::Done);
        };
        let (aspects, index, groups) = build.finish();
        let key = self.key;
        self.finish(Ok(BuiltLayout {
            key,
            aspects,
            index,
            groups,
        }));
        Ok(Step::Done)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nicti_lair::SqliteCatalog;

    use crate::test_support::seed_assets;

    fn params() -> RowParams {
        RowParams {
            width: 800.0,
            target_height: 100.0,
            gap: 4.0,
            header_height: 24.0,
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

    fn built(items: &[LayoutItem], grouped: bool, chunk: usize) -> Build {
        let mut b = Build::new(IndexKey::bucketed(params()));
        for c in items.chunks(chunk.max(1)) {
            b.feed(c, grouped).unwrap();
        }
        b.index.finish();
        b
    }

    #[test]
    fn a_job_over_the_session_ids_builds_a_complete_index_in_several_steps() {
        let catalog = Arc::new(SqliteCatalog::open_in_memory().unwrap());
        let n = IDS_PER_STEP * 2 + 100;
        let ids = seed_assets(&catalog, n);
        let store: Arc<dyn CatalogStore + Send + Sync> = catalog;
        let key = IndexKey::new(7, false, params());
        let (mut job, slot) = LayoutJob::new(store, ids, key, params());
        assert_eq!(run(&mut job), 2, "three steps: two yields and the finish");
        let built = slot.lock().unwrap().take().unwrap().unwrap();
        assert_eq!(built.key.generation, 7);
        assert!(built.index.is_complete());
        assert_eq!(built.index.cells(), n);
        assert_eq!(built.aspects.len(), n);
        assert!(built.groups.is_empty(), "no headers when grouping is off");
    }

    #[test]
    fn the_layout_follows_the_given_order_not_the_catalogs() {
        let catalog = Arc::new(SqliteCatalog::open_in_memory().unwrap());
        let mut ids = seed_assets(&catalog, 50);
        ids.reverse(); // the session's list, however it was sorted
        let store: Arc<dyn CatalogStore + Send + Sync> = catalog;
        let key = IndexKey::new(1, false, params());
        let (mut job, slot) = LayoutJob::new(store.clone(), ids.clone(), key, params());
        run(&mut job);
        assert_eq!(
            slot.lock().unwrap().take().unwrap().unwrap().index.cells(),
            50
        );
        // Deleted photos still occupy a (square) cell, so cell numbers keep matching the list.
        let (mut job, slot) = LayoutJob::new(store, vec![ids[0], 9_999_999], key, params());
        run(&mut job);
        assert_eq!(
            slot.lock().unwrap().take().unwrap().unwrap().index.cells(),
            2
        );
    }

    #[test]
    fn a_cancelled_or_finished_job_holds_no_data_and_a_failure_lands_in_the_slot() {
        let catalog = Arc::new(SqliteCatalog::open_in_memory().unwrap());
        let ids = seed_assets(&catalog, 10);
        let store: Arc<dyn CatalogStore + Send + Sync> = catalog;
        let key = IndexKey::new(1, false, params());
        let (mut job, slot) = LayoutJob::new(store, ids, key, params());
        run(&mut job);
        assert!(job.ids.is_empty() && job.build.is_none());
        assert!(slot.lock().unwrap().take().unwrap().is_ok());
        // Stepping a finished job is harmless.
        assert_eq!(job.step().unwrap(), Step::Done);
        assert!(slot.lock().unwrap().is_none(), "no second result");
    }

    #[test]
    fn chunking_does_not_change_the_result() {
        let n = 70_000usize;
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
        let (one, many) = (built(&items, true, n), built(&items, true, 997));
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
        let b = built(&items, true, 100);
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
        let b = built(&items, false, 100);
        assert!(b.groups.is_empty());
        assert_eq!(b.index.rows(), 1, "both photos share a row");
        assert_eq!(b.index.header_of_row(0), None);
    }

    #[test]
    fn a_rotated_photo_is_laid_out_by_its_displayed_shape() {
        let mut portrait = item(1, None);
        portrait.orientation = Some(6); // stored 300x200, displayed 200x300
        let b = built(&[portrait], false, 10);
        assert!(
            (b.aspects[0] - 200.0 / 300.0).abs() < 1e-6,
            "{:?}",
            b.aspects
        );
    }

    #[test]
    fn an_empty_list_finishes_with_an_empty_complete_index() {
        let b = built(&[], true, 10);
        assert!(b.index.is_complete());
        assert_eq!(b.index.rows(), 0);
    }
}
