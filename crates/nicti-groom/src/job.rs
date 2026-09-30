//! `RemoveJob`: one AI removal on Pounce's GPU lane -- the first real job that lane has run.
//!
//! One chunk: `ort::Session::run` can't be interrupted mid-call (`nicti_pounce::job`'s own note),
//! so the whole segment-and-inpaint runs in a single `step()`. The GPU lane's one serial worker is
//! also what keeps the shared [`RemovalBackend`] (with its loaded sessions and cached embedding)
//! from ever being entered twice at once.
//!
//! Like `DecodeJob`, a *removal* failure (no object at the click, region too large, a model error)
//! is stored as the outcome and `step()` still returns `Ok`: an `Err` from `step()` makes the
//! scheduler drop the job without touching the slot, which would leave the UI waiting on a result
//! that never arrives.

use std::sync::{Arc, Mutex};

use nicti_pounce::{ChunkedJob, JobError, JobKind, JobSpec, Lane, Priority, Progress, Step};
use nicti_tapetum::heal::RemovalPatch;

use crate::remove::{RemovalBackend, RemovalRequest};
use crate::sam::Prompt;
use crate::PixelSource;

pub type Slot<T> = Arc<Mutex<Option<T>>>;

/// A finished removal, tagged with the spot it was for so the UI can file it under
/// `heal::spot_key` even if the user has moved on to another spot meanwhile.
#[derive(Debug, Clone)]
pub struct RemoveOutcome {
    pub spot_key: String,
    pub result: Result<Arc<RemovalPatch>, String>,
}

/// Shared handle to the (loaded-once) removal backend.
pub type SharedBackend = Arc<Mutex<dyn RemovalBackend + Send>>;

pub struct RemoveJob {
    backend: SharedBackend,
    source: Arc<dyn PixelSource>,
    image_key: u64,
    cam_mul: [f32; 4],
    prompt: Prompt,
    center: (f32, f32),
    radius: f32,
    spot_key: String,
    label: String,
    done: bool,
    slot: Slot<RemoveOutcome>,
}

impl RemoveJob {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        backend: SharedBackend,
        source: Arc<dyn PixelSource>,
        image_key: u64,
        cam_mul: [f32; 4],
        prompt: Prompt,
        center: (f32, f32),
        radius: f32,
        spot_key: String,
    ) -> (Self, Slot<RemoveOutcome>) {
        let slot: Slot<RemoveOutcome> = Arc::new(Mutex::new(None));
        let job = Self {
            backend,
            source,
            image_key,
            cam_mul,
            prompt,
            center,
            radius,
            spot_key,
            label: format!("Remove object at ({:.0}, {:.0})", center.0, center.1),
            done: false,
            slot: Arc::clone(&slot),
        };
        (job, slot)
    }
}

impl ChunkedJob for RemoveJob {
    fn spec(&self) -> JobSpec {
        JobSpec {
            // Foreground: the user is waiting on this result, unlike a background bake.
            priority: Priority::Foreground,
            kind: JobKind::Bake,
            lane: Lane::Gpu,
            // The default ONNX Runtime build is the CPU execution provider, so no VRAM is claimed.
            // A CUDA/DirectML build would declare the sessions' real footprint here.
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
        let result = match self.backend.lock() {
            Ok(mut backend) => backend
                .remove(&RemovalRequest {
                    image_key: self.image_key,
                    source: self.source.as_ref(),
                    cam_mul: self.cam_mul,
                    prompt: self.prompt,
                    center: self.center,
                    radius: self.radius,
                })
                .map(Arc::new)
                .map_err(|e| e.to_string()),
            // A previous removal panicked while holding the backend: its sessions may be in an
            // unknown state, so refuse rather than run on them.
            Err(_) => Err("the removal engine failed earlier; restart to reload it".to_owned()),
        };
        *self.slot.lock().unwrap() = Some(RemoveOutcome {
            spot_key: self.spot_key.clone(),
            result,
        });
        self.done = true;
        Ok(Step::Done)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RemovalError;
    use crate::RgbBuffer;

    struct Fake {
        result: Result<(), RemovalError>,
        calls: u32,
    }

    impl RemovalBackend for Fake {
        fn remove(&mut self, req: &RemovalRequest<'_>) -> Result<RemovalPatch, RemovalError> {
            self.calls += 1;
            match &self.result {
                Ok(()) => RemovalPatch::new(
                    (req.center.0 as i32, req.center.1 as i32),
                    3,
                    vec![[0.5, 0.5, 0.5, 1.0]; 9],
                )
                .map_err(|e| RemovalError::BadInput(e.to_string())),
                Err(RemovalError::NoObject) => Err(RemovalError::NoObject),
                Err(_) => Err(RemovalError::Ort("boom".into())),
            }
        }
    }

    fn job_with(backend: Fake) -> (RemoveJob, Slot<RemoveOutcome>, Arc<Mutex<Fake>>) {
        let concrete = Arc::new(Mutex::new(backend));
        let shared: SharedBackend = concrete.clone();
        let source: Arc<dyn PixelSource> = Arc::new(RgbBuffer {
            width: 4,
            height: 4,
            data: vec![[0.1; 3]; 16],
        });
        let (job, slot) = RemoveJob::new(
            shared,
            source,
            1,
            [1.0; 4],
            Prompt::Click { x: 2.0, y: 2.0 },
            (2.0, 2.0),
            5.0,
            "spot-key".into(),
        );
        (job, slot, concrete)
    }

    #[test]
    fn runs_on_the_gpu_lane_as_a_foreground_bake() {
        let (job, _, _) = job_with(Fake {
            result: Ok(()),
            calls: 0,
        });
        let spec = job.spec();
        assert_eq!(spec.lane, Lane::Gpu);
        assert_eq!(spec.kind, JobKind::Bake);
        assert_eq!(spec.priority, Priority::Foreground);
        assert_eq!(spec.vram_bytes, 0);
    }

    #[test]
    fn a_successful_removal_fills_the_slot_and_finishes() {
        let (mut job, slot, backend) = job_with(Fake {
            result: Ok(()),
            calls: 0,
        });
        assert_eq!(job.progress().done, 0);
        assert_eq!(job.step().unwrap(), Step::Done);
        assert_eq!(job.progress().done, 1);
        let outcome = slot.lock().unwrap().take().expect("slot resolved");
        assert_eq!(outcome.spot_key, "spot-key");
        assert_eq!(outcome.result.unwrap().side, 3);
        assert_eq!(backend.lock().unwrap().calls, 1);
    }

    #[test]
    fn a_removal_failure_is_an_outcome_not_a_job_error() {
        let (mut job, slot, _) = job_with(Fake {
            result: Err(RemovalError::NoObject),
            calls: 0,
        });
        // step() must succeed so the scheduler doesn't drop the job before the slot is written.
        assert_eq!(job.step().unwrap(), Step::Done);
        let outcome = slot
            .lock()
            .unwrap()
            .take()
            .expect("slot resolved even on failure");
        assert!(outcome.result.unwrap_err().contains("no object"));
    }

    #[test]
    fn a_poisoned_backend_resolves_the_slot_with_an_error() {
        let (mut job, slot, backend) = job_with(Fake {
            result: Ok(()),
            calls: 0,
        });
        let b = backend.clone();
        let _ = std::thread::spawn(move || {
            let _guard = b.lock().unwrap();
            panic!("simulated crash inside a removal");
        })
        .join();
        assert_eq!(job.step().unwrap(), Step::Done);
        let outcome = slot.lock().unwrap().take().expect("slot resolved");
        assert!(outcome.result.is_err());
    }
}
