//! #31: decodes one real NEF/DNG on Pounce's CPU lane. Lives here, not `nicti-lair::pounce_jobs`,
//! because `nicti-lair` must stay free of the `libraw` Cargo feature (see its own `Cargo.toml`/
//! `nicti-cornea`'s doc comment) -- this crate is the one place in the app that actually depends
//! on the real decoder. Generic over `RawDecoder` (not hard-coded to `LibRawDecoder`) so tests
//! don't need a real NEF file or the `libraw` feature enabled to exercise the job/cache plumbing
//! -- `nicti-cornea`'s own tests already established this pattern (a fake `RawDecoder` impl,
//! since it too had no real fixture file available in this sandbox).
//!
//! A single-chunk job: `decode_linear` isn't usefully interruptible mid-call (same "neither
//! `wgpu::Queue::submit` nor `ort::Session::run` support cancelling a call in flight" reasoning
//! `nicti_pounce::job`'s own doc comment gives for GPU work applies here too -- LibRaw's C decode
//! call has no yield points Rust could hook), so the whole decode runs in one `step()`.

use std::path::PathBuf;
use std::sync::Arc;

use nicti_cornea::{LinearFrame, RawDecoder};
use nicti_lair::pounce_jobs::ReportSlot;
use nicti_pounce::{ChunkedJob, JobError, JobKind, JobSpec, Lane, Priority, Progress, Step};

/// A decode's outcome -- `Err` (the message from `DecodeError::to_string()`) is a normal,
/// expected result to store, not a job failure: a corrupt/unsupported file must not leave a
/// caller's `ReportSlot` waiting forever. See [`DecodeJob::step`]'s own doc comment for why this
/// job's `step()` itself always returns `Ok`.
pub type DecodeResult = Result<Arc<LinearFrame>, String>;

pub struct DecodeJob {
    decoder: Arc<dyn RawDecoder + Send + Sync>,
    path: PathBuf,
    label: String,
    image_index: usize,
    done: bool,
    result: ReportSlot<DecodeResult>,
}

impl DecodeJob {
    /// `image_index` is this asset's absolute position in the loupe's current ordered list --
    /// what `JobSpec::image_index` needs for `Pounce::reprioritize`'s nearest-to-cursor ordering
    /// (`nicti_tapetum::prefetch::priority_order`'s same distance-from-cursor key, computed by
    /// the caller that owns the cursor -- this crate's own `loupe` module).
    pub fn new(
        decoder: Arc<dyn RawDecoder + Send + Sync>,
        path: PathBuf,
        image_index: usize,
    ) -> (Self, ReportSlot<DecodeResult>) {
        let result = Arc::new(std::sync::Mutex::new(None));
        let label = format!("Decode: {}", path.display());
        let job = DecodeJob {
            decoder,
            path,
            label,
            image_index,
            done: false,
            result: result.clone(),
        };
        (job, result)
    }
}

impl ChunkedJob for DecodeJob {
    fn spec(&self) -> JobSpec {
        JobSpec {
            // Always Background, even for the image currently on screen: nothing about a decode
            // is a foreground-preempting operation the way a live slider render is (ADR-0054's
            // Foreground class is for that) -- the T0 embedded-preview fallback is what keeps
            // navigation feeling instant while this runs (loupe module, once wired in #31 phase
            // 3), and `image_index` here is exactly what lets the *current* image's decode still
            // sort ahead of its neighbors' via Pounce's nearest-to-cursor reprioritization.
            priority: Priority::Background,
            kind: JobKind::Decode,
            lane: Lane::Cpu,
            vram_bytes: 0,
            image_index: Some(self.image_index),
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

    /// Never returns `Err` -- a decode failure (corrupt file, unsupported format) is stored as
    /// `Err` *inside* the resolved `ReportSlot`, exactly like `BackupJob`'s own fix (see its doc
    /// comment in `nicti-lair::pounce_jobs`): the scheduler drops a job that errors out of
    /// `step()` and reports it `Failed` without ever touching the slot, which would leave a
    /// poller (the loupe's prefetch cache) waiting on a slot that never resolves. A real decode
    /// error is exactly the case a caller most needs to observe, not lose.
    fn step(&mut self) -> Result<Step, JobError> {
        let outcome = self
            .decoder
            .decode_linear(&self.path)
            .map(Arc::new)
            .map_err(|e| e.to_string());
        *self.result.lock().unwrap() = Some(outcome);
        self.done = true;
        Ok(Step::Done)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nicti_claw::Module;
    use nicti_cornea::DecodeError;
    use std::path::Path;

    struct FakeDecoder {
        result: Result<LinearFrame, String>,
    }

    impl Module for FakeDecoder {
        fn id(&self) -> &str {
            "test.decoder.fake"
        }

        fn schema_version(&self) -> u32 {
            1
        }

        fn migrate_params(
            &self,
            _from_version: u32,
            _params: serde_json::Value,
        ) -> Option<serde_json::Value> {
            None
        }
    }

    impl RawDecoder for FakeDecoder {
        fn decode_linear(&self, _path: &Path) -> Result<LinearFrame, DecodeError> {
            self.result.clone().map_err(|msg| DecodeError::Io {
                path: PathBuf::new(),
                source: std::io::Error::other(msg),
            })
        }
    }

    fn tiny_frame() -> LinearFrame {
        LinearFrame {
            make: "Test".to_string(),
            model: "Fake".to_string(),
            width: 2,
            height: 2,
            black: 0,
            maximum: 4095,
            cam_mul: [1.0, 1.0, 1.0, 1.0],
            pre_mul: [1.0, 1.0, 1.0, 1.0],
            cam_xyz: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0],
            cblack: [0, 0, 0, 0],
            pixels: vec![0; 2 * 2 * 3],
            dng_opcode_list3: None,
            nikon_lens_info: None,
        }
    }

    #[test]
    fn step_resolves_the_slot_on_success() {
        let decoder = Arc::new(FakeDecoder {
            result: Ok(tiny_frame()),
        });
        let (mut job, slot) = DecodeJob::new(decoder, PathBuf::from("photo.NEF"), 5);
        assert_eq!(job.step().unwrap(), Step::Done);
        let resolved = slot.lock().unwrap().take().expect("slot must resolve");
        assert!(resolved.is_ok());
        assert_eq!(resolved.unwrap().width, 2);
    }

    #[test]
    fn step_resolves_the_slot_with_err_on_failure_never_propagates_job_error() {
        let decoder = Arc::new(FakeDecoder {
            result: Err("corrupt file".to_string()),
        });
        let (mut job, slot) = DecodeJob::new(decoder, PathBuf::from("bad.NEF"), 0);
        // The job itself must report Done, not Err -- see step()'s own doc comment for why.
        assert_eq!(job.step().unwrap(), Step::Done);
        let resolved = slot
            .lock()
            .unwrap()
            .take()
            .expect("slot must still resolve");
        assert!(resolved.is_err());
    }

    #[test]
    fn spec_carries_the_given_image_index_for_reprioritization() {
        let decoder = Arc::new(FakeDecoder {
            result: Ok(tiny_frame()),
        });
        let (job, _slot) = DecodeJob::new(decoder, PathBuf::from("photo.NEF"), 42);
        assert_eq!(job.spec().image_index, Some(42));
        assert_eq!(job.spec().lane, Lane::Cpu);
        assert_eq!(job.spec().priority, Priority::Background);
    }
}
