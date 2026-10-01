//! Off-thread sidecar reads for the UI-thread thumbnail readers (#72).
//!
//! An archived folder's T0 lives in a `.thumb.jpg` next to the RAW, on a drive that can be
//! unmounted or a dead network share, where `fs::read` can block for the OS timeout. The cull
//! tiles and the loupe fallback draw on the UI thread, so they look in the catalog (a local
//! query) and, on a miss, queue a [`SidecarJob`] and poll its slot on later frames.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use nicti_lair::pounce_jobs::ReportSlot;
use nicti_lair::CatalogStore;
use nicti_pounce::{
    ChunkedJob, JobError, JobKind, JobSpec, Lane, Pounce, Priority, Progress, Step,
};

/// `Some(bytes)` = the sidecar's JPEG; `None` = no readable sidecar.
/// Most sidecar reads in flight from one reader at a time. Each can block a pool worker for the
/// OS timeout on a dead network drive; the cap keeps a page of archived tiles from parking them all.
pub const MAX_IN_FLIGHT: usize = 4;

pub type FetchSlot = ReportSlot<Option<Vec<u8>>>;

pub struct SidecarJob {
    raw: PathBuf,
    label: String,
    done: bool,
    result: FetchSlot,
}

impl ChunkedJob for SidecarJob {
    fn spec(&self) -> JobSpec {
        JobSpec {
            priority: Priority::Background,
            kind: JobKind::Preview,
            lane: Lane::Cpu,
            vram_bytes: 0,
            // A thumbnail someone is looking at right now: front of the background queue (a
            // `None` index sorts behind every finite one).
            image_index: Some(0),
        }
    }

    fn label(&self) -> String {
        self.label.clone()
    }

    fn progress(&self) -> Progress {
        Progress {
            done: u64::from(self.done),
            total: Some(1),
        }
    }

    fn step(&mut self) -> Result<Step, JobError> {
        let bytes = std::panic::catch_unwind(|| nicti_lair::tier::read_sidecar(&self.raw))
            .ok()
            .flatten()
            .map(|p| p.bytes);
        *self.result.lock().unwrap() = Some(bytes);
        self.done = true;
        Ok(Step::Done)
    }
}

/// A cancelled (dropped-unrun) job still fills its slot, so a poller never waits forever.
impl Drop for SidecarJob {
    fn drop(&mut self) {
        let mut slot = self.result.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            *slot = Some(None);
        }
    }
}

/// Queues a sidecar read for `asset_id`. `None` if the catalog doesn't know where its RAW is.
pub fn request(pounce: &Pounce, store: &dyn CatalogStore, asset_id: i64) -> Option<FetchSlot> {
    let raw = nicti_lair::tier::raw_path_for(store, asset_id)
        .ok()
        .flatten()?;
    let result: FetchSlot = Arc::new(Mutex::new(None));
    let label = format!("Thumbnail: {}", raw.display());
    pounce.submit(Box::new(SidecarJob {
        raw,
        label,
        done: false,
        result: result.clone(),
    }));
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(raw: PathBuf) -> (SidecarJob, FetchSlot) {
        let result: FetchSlot = Arc::new(Mutex::new(None));
        (
            SidecarJob {
                raw,
                label: String::new(),
                done: false,
                result: result.clone(),
            },
            result,
        )
    }

    #[test]
    fn a_missing_sidecar_resolves_to_none_and_a_dropped_job_still_fills_its_slot() {
        let d = tempfile::tempdir().unwrap();
        let (mut j, slot) = job(d.path().join("a.NEF"));
        assert!(matches!(j.step(), Ok(Step::Done)));
        assert_eq!(*slot.lock().unwrap(), Some(None));

        let (j, slot) = job(d.path().join("b.NEF"));
        drop(j);
        assert_eq!(*slot.lock().unwrap(), Some(None), "never left pending");
    }

    #[test]
    fn request_runs_through_pounce_and_a_catalog() {
        use nicti_lair::{NewAsset, SqliteCatalog};
        let d = tempfile::tempdir().unwrap();
        let store = SqliteCatalog::open_in_memory().unwrap();
        let vol = store.upsert_volume("v", None, None, 0).unwrap();
        let root = store.ensure_root(vol, &d.path().to_string_lossy()).unwrap();
        let asset = store
            .insert_asset(
                root,
                &NewAsset {
                    rel_path: "a.NEF".into(),
                    rel_path_fold: "a.nef".into(),
                    size_bytes: 0,
                    mtime_unix: 0,
                    fingerprint: None,
                    natural_key: None,
                    make: None,
                    model: None,
                    captured_at: None,
                    width: None,
                    height: None,
                    imported_at: 0,
                },
                None,
            )
            .unwrap();
        let jpeg = vec![
            0xFF, 0xD8, 0xFF, 0xC0, 0, 0x0B, 8, 0, 3, 0, 4, 1, 1, 0x11, 0, 0xFF, 0xD9,
        ];
        std::fs::write(d.path().join("a.NEF.thumb.jpg"), &jpeg).unwrap();
        let pounce = Pounce::new(0, 2, 2, || {});
        let slot = request(&pounce, &store, asset).expect("asset has a known RAW path");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let got = loop {
            if let Some(v) = slot.lock().unwrap().take() {
                break v;
            }
            assert!(std::time::Instant::now() < deadline, "job never reported");
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        assert_eq!(got, Some(jpeg));
        assert!(request(&pounce, &store, 99_999).is_none(), "unknown asset");
    }

    #[test]
    fn reads_a_present_sidecar_off_the_calling_thread_path() {
        let d = tempfile::tempdir().unwrap();
        let raw = d.path().join("a.NEF");
        let jpeg = vec![
            0xFF, 0xD8, 0xFF, 0xC0, 0, 0x0B, 8, 0, 3, 0, 4, 1, 1, 0x11, 0, 0xFF, 0xD9,
        ];
        std::fs::write(d.path().join("a.NEF.thumb.jpg"), &jpeg).unwrap();
        let (mut j, slot) = job(raw);
        j.step().unwrap();
        assert_eq!(slot.lock().unwrap().clone(), Some(Some(jpeg)));
    }
}
