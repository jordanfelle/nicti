//! `MaskBakeJob`: one AI mask bake on Pounce's GPU lane.
//!
//! Same shape and same rules as `nicti_groom::job::RemoveJob`:
//! - **One chunk.** `ort::Session::run` can't be interrupted mid-call, so the whole
//!   neutral-image + inference runs in a single `step()`. The GPU lane's one serial worker also
//!   keeps the shared backend (with its loaded sessions) from ever being entered twice at once --
//!   and serializes bakes against removals, which share the ONNX Runtime environment.
//! - **A bake failure is an outcome, not a job error.** `step()` returns `Ok(Step::Done)` and the
//!   slot carries the `Err`: an `Err` from `step()` makes the scheduler drop the job without
//!   touching the slot, and the UI would wait forever on a result that never comes.
//! - **The result names the photo.** [`MaskBakeOutcome::image_key`] lets a result that lands after
//!   the user moved to another photo be recognised and dropped.

use std::sync::{Arc, Mutex};

use nicti_groom::PixelSource;
use nicti_pounce::{ChunkedJob, JobError, JobKind, JobSpec, Lane, Priority, Progress, Step};
use nicti_tapetum::coat::MaskRecipe;
use nicti_tapetum::mask::engine::AiAlpha;

use crate::backend::{BakeRequest, MaskBackend};

pub type Slot<T> = Arc<Mutex<Option<T>>>;

/// The error a bake resolves with when its job was cancelled before it ran. A poller treats it as
/// retryable, not as a failure of the model.
pub const CANCELLED: &str = "cancelled";

/// Shared handle to the (loaded-once) backend.
pub type SharedBackend = Arc<Mutex<dyn MaskBackend + Send>>;

/// A finished bake.
pub struct MaskBakeOutcome {
    /// The photo it was computed for.
    pub image_key: u64,
    /// `nicti_tapetum::mask::compose::ai_bake_key` of the recipe against the neutral render, so the
    /// UI files the alpha where the render graph will look for it.
    pub bake_key: blake3::Hash,
    pub result: Result<Arc<AiAlpha>, String>,
}

pub struct MaskBakeJob {
    backend: SharedBackend,
    source: Arc<dyn PixelSource>,
    image_key: u64,
    cam_mul: [f32; 4],
    recipe: MaskRecipe,
    bake_key: blake3::Hash,
    label: String,
    done: bool,
    slot: Slot<MaskBakeOutcome>,
}

impl MaskBakeJob {
    pub fn new(
        backend: SharedBackend,
        source: Arc<dyn PixelSource>,
        image_key: u64,
        cam_mul: [f32; 4],
        recipe: MaskRecipe,
        bake_key: blake3::Hash,
    ) -> (Self, Slot<MaskBakeOutcome>) {
        let slot: Slot<MaskBakeOutcome> = Arc::new(Mutex::new(None));
        let target = crate::providers::target_of(&recipe)
            .map(|t| t.as_str())
            .unwrap_or("mask");
        let job = Self {
            backend,
            source,
            image_key,
            cam_mul,
            label: format!("Select {target}"),
            recipe,
            bake_key,
            done: false,
            slot: Arc::clone(&slot),
        };
        (job, slot)
    }
}

impl ChunkedJob for MaskBakeJob {
    fn spec(&self) -> JobSpec {
        JobSpec {
            // Foreground: the user just asked for this mask and is looking at it.
            priority: Priority::Foreground,
            kind: JobKind::Bake,
            lane: Lane::Gpu,
            // The default ONNX Runtime build is the CPU execution provider, so no VRAM is claimed;
            // a GPU EP (a follow-up) would declare the session's real footprint here.
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
                .bake(&BakeRequest {
                    image_key: self.image_key,
                    source: self.source.as_ref(),
                    cam_mul: self.cam_mul,
                    recipe: &self.recipe,
                })
                .map_err(|e| e.to_string())
                .and_then(|a| {
                    AiAlpha::new(a.width, a.height, a.alpha)
                        .map(Arc::new)
                        .ok_or_else(|| "the model returned an empty alpha".to_owned())
                }),
            // A previous bake panicked while holding the backend: its sessions may be in an
            // unknown state, so refuse rather than run on them.
            Err(_) => Err("the mask engine failed earlier; restart to reload it".to_owned()),
        };
        *self.slot.lock().unwrap() = Some(MaskBakeOutcome {
            image_key: self.image_key,
            bake_key: self.bake_key,
            result,
        });
        self.done = true;
        Ok(Step::Done)
    }
}

impl Drop for MaskBakeJob {
    /// Pounce drops a job cancelled while still queued *without* ever calling `step()`, so the slot
    /// would stay empty and whoever holds it would wait forever (a stuck "Selecting..." badge and a
    /// permanent repaint timer). Resolve it here, unless `step()` already did.
    fn drop(&mut self) {
        if self.done {
            return;
        }
        if let Ok(mut slot) = self.slot.lock() {
            if slot.is_none() {
                *slot = Some(MaskBakeOutcome {
                    image_key: self.image_key,
                    bake_key: self.bake_key,
                    result: Err(CANCELLED.to_owned()),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nicti_groom::RgbBuffer;

    #[test]
    fn a_job_dropped_before_it_ran_resolves_its_slot_as_cancelled() {
        let (job, slot, _) = job_with(Fake { ok: true, calls: 0 });
        drop(job); // what Pounce does to a job cancelled while queued
        let out = slot
            .lock()
            .unwrap()
            .take()
            .expect("a cancelled job must not strand its slot");
        assert_eq!(out.image_key, 42);
        assert_eq!(out.result.err().as_deref(), Some(CANCELLED));
    }

    #[test]
    fn a_job_that_ran_is_not_overwritten_when_it_is_dropped_afterwards() {
        let (mut job, slot, _) = job_with(Fake { ok: true, calls: 0 });
        job.step().unwrap();
        let first = slot.lock().unwrap().take().expect("resolved by step()");
        assert!(first.result.is_ok());
        drop(job);
        assert!(
            slot.lock().unwrap().is_none(),
            "Drop must not refill a consumed slot"
        );
    }
    use nicti_stalk::{AlphaMap, SegmentError, SegmentTarget};

    struct Fake {
        ok: bool,
        calls: u32,
    }

    impl MaskBackend for Fake {
        fn bake(&mut self, _req: &BakeRequest<'_>) -> Result<AlphaMap, SegmentError> {
            self.calls += 1;
            if self.ok {
                AlphaMap::new(2, 2, vec![0.0, 1.0, 1.0, 0.0])
            } else {
                Err(SegmentError::NotInstalled("not downloaded yet".into()))
            }
        }
    }

    fn job_with(backend: Fake) -> (MaskBakeJob, Slot<MaskBakeOutcome>, Arc<Mutex<Fake>>) {
        let concrete = Arc::new(Mutex::new(backend));
        let shared: SharedBackend = concrete.clone();
        let source: Arc<dyn PixelSource> = Arc::new(RgbBuffer {
            width: 4,
            height: 4,
            data: vec![[0.1; 3]; 16],
        });
        let (job, slot) = MaskBakeJob::new(
            shared,
            source,
            42,
            [1.0; 4],
            crate::providers::recipe_for(SegmentTarget::Subject),
            blake3::hash(b"bake"),
        );
        (job, slot, concrete)
    }

    #[test]
    fn runs_on_the_gpu_lane_as_a_foreground_bake_and_says_what_it_selects() {
        let (job, _, _) = job_with(Fake { ok: true, calls: 0 });
        let spec = job.spec();
        assert_eq!(spec.lane, Lane::Gpu);
        assert_eq!(spec.kind, JobKind::Bake);
        assert_eq!(spec.priority, Priority::Foreground);
        assert_eq!(spec.vram_bytes, 0);
        assert_eq!(job.label(), "Select subject");
    }

    #[test]
    fn a_successful_bake_fills_the_slot_with_the_photo_and_bake_key() {
        let (mut job, slot, backend) = job_with(Fake { ok: true, calls: 0 });
        assert_eq!(job.progress().done, 0);
        assert_eq!(job.step().unwrap(), Step::Done);
        assert_eq!(job.progress().done, 1);
        let out = slot.lock().unwrap().take().expect("slot resolved");
        assert_eq!(out.image_key, 42);
        assert_eq!(out.bake_key, blake3::hash(b"bake"));
        let alpha = out.result.expect("a successful bake");
        assert_eq!((alpha.width, alpha.height), (2, 2));
        assert_eq!(backend.lock().unwrap().calls, 1);
    }

    #[test]
    fn a_bake_failure_is_an_outcome_not_a_job_error() {
        let (mut job, slot, _) = job_with(Fake {
            ok: false,
            calls: 0,
        });
        // step() must succeed so the scheduler doesn't drop the job before the slot is written.
        assert_eq!(job.step().unwrap(), Step::Done);
        let out = slot
            .lock()
            .unwrap()
            .take()
            .expect("slot resolved even on failure");
        assert!(out.result.err().unwrap().contains("not installed"));
    }

    #[test]
    fn a_poisoned_backend_resolves_the_slot_with_an_error() {
        let (mut job, slot, backend) = job_with(Fake { ok: true, calls: 0 });
        let b = backend.clone();
        let _ = std::thread::spawn(move || {
            let _guard = b.lock().unwrap();
            panic!("simulated crash inside a bake");
        })
        .join();
        assert_eq!(job.step().unwrap(), Step::Done);
        assert!(slot.lock().unwrap().take().unwrap().result.is_err());
    }
}
