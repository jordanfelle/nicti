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
            image_index: None,
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
