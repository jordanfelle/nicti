//! The first full-resolution render of a photo, off the UI thread (#778).
//!
//! Opening a photo used to run `DevelopView::render` inline: decode the frame on the GPU, then
//! every baked stage, on the UI thread (1.6 s on a 24 MP JPEG under software Vulkan, seconds on a
//! big RAW). The window froze. [`FirstRenderJob`] takes the whole `DevelopView` (document and
//! engine travel together: a render mutates both), runs that render on Pounce's GPU lane, and
//! hands the view back through a slot. The renderer's baked cache is then warm, so the UI
//! thread's own `render()` for painting is the cheap live pass; slider drags keep using exactly
//! that path and never come through here. The Loupe keeps showing the T2/T0 preview until the
//! view is back.
//!
//! The view is never lost: it goes back into the slot after a render that panicked too (the UI
//! thread then meets the same panic it always would have) and when the job is dropped unrun.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex};

use nicti_pounce::{ChunkedJob, JobError, JobKind, JobSpec, Lane, Priority, Progress, Step};

use crate::render::DevelopView;

/// What the job hands back.
pub struct RenderReturn {
    pub view: DevelopView,
    /// The render panicked (its message); the view is returned regardless.
    pub panicked: Option<String>,
}

pub type RenderSlot = Arc<Mutex<Option<RenderReturn>>>;

pub struct FirstRenderJob {
    view: Option<DevelopView>,
    out: RenderSlot,
    label: String,
}

impl FirstRenderJob {
    pub fn new(view: DevelopView, label: String) -> (Self, RenderSlot) {
        let out: RenderSlot = Arc::new(Mutex::new(None));
        (
            Self {
                view: Some(view),
                out: out.clone(),
                label,
            },
            out,
        )
    }

    fn give_back(&mut self, panicked: Option<String>) {
        if let Some(view) = self.view.take() {
            *self.out.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(RenderReturn { view, panicked });
        }
    }
}

impl ChunkedJob for FirstRenderJob {
    fn spec(&self) -> JobSpec {
        JobSpec {
            // The user is waiting on this photo: the interactive class, not a background bake.
            priority: Priority::Foreground,
            kind: JobKind::LiveRender,
            lane: Lane::Gpu,
            vram_bytes: 0,
            image_index: None,
        }
    }

    fn label(&self) -> String {
        self.label.clone()
    }

    fn progress(&self) -> Progress {
        Progress {
            done: u64::from(self.view.is_none()),
            total: Some(1),
        }
    }

    fn step(&mut self) -> Result<Step, JobError> {
        let mut panicked = None;
        if let Some(view) = self.view.as_mut() {
            // Caught here (not just by Pounce) so the view still goes back to the UI.
            if let Err(payload) = catch_unwind(AssertUnwindSafe(|| {
                let _ = view.render();
            })) {
                panicked = Some(
                    payload
                        .downcast_ref::<&str>()
                        .map(|s| (*s).to_string())
                        .or_else(|| payload.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "render panicked".to_string()),
                );
            }
        }
        self.give_back(panicked);
        Ok(Step::Done)
    }
}

impl Drop for FirstRenderJob {
    /// A job dropped before it ran (cancelled, scheduler shut down) still returns the view.
    fn drop(&mut self) {
        self.give_back(None);
    }
}

/// What to do when a batch edit touched photos while `develop` may be out with a render job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshPlan {
    /// The loaded photo was not touched (or none is loaded): nothing to do.
    Skip,
    /// Re-read its document from the catalog now.
    Now,
    /// The view is out with the render job: remember it and re-read when it comes back, or the
    /// returned view would hold the old document and a later autosave would overwrite the batch.
    Defer,
}

pub fn refresh_plan(loaded: Option<i64>, job_in_flight: bool, touched: &[i64]) -> RefreshPlan {
    match loaded {
        Some(id) if touched.contains(&id) => {
            if job_in_flight {
                RefreshPlan::Defer
            } else {
                RefreshPlan::Now
            }
        }
        _ => RefreshPlan::Skip,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nicti_pawprint::EditDocument;

    fn loaded_view() -> Option<DevelopView> {
        let gpu = crate::test_gpu::shared()?;
        let mut view = DevelopView::new(gpu);
        view.load_real_frame(
            view.frame_arc(),
            blake3::hash(b"photo"),
            EditDocument::default(),
        );
        Some(view)
    }

    #[test]
    fn the_job_returns_the_view_with_a_warm_cache_so_the_ui_render_is_only_the_live_pass() {
        let Some(view) = loaded_view() else { return };
        let (mut job, slot) = FirstRenderJob::new(view, "test".into());
        assert_eq!(job.step().unwrap(), Step::Done);
        let back = slot.lock().unwrap().take().expect("the view comes back");
        assert!(back.panicked.is_none());
        let mut view = back.view;
        // What the UI thread does next, once the view is back.
        let _ = view.render();
        let stats = view.last_render_stats();
        assert_eq!(
            stats.bake_dispatches, 0,
            "the baked stages were rendered by the job: {stats:?}"
        );
    }

    #[test]
    fn without_the_job_the_first_ui_render_does_the_baking() {
        // The control for the test above: a cold view bakes on its first render.
        let Some(mut view) = loaded_view() else {
            return;
        };
        let _ = view.render();
        assert!(view.last_render_stats().bake_dispatches > 0);
    }

    #[test]
    fn a_job_dropped_unrun_still_returns_the_view() {
        let Some(view) = loaded_view() else { return };
        let (job, slot) = FirstRenderJob::new(view, "test".into());
        drop(job);
        assert!(slot.lock().unwrap().is_some(), "the view is not lost");
    }

    #[test]
    fn it_is_an_interactive_gpu_lane_job_and_reports_done() {
        let Some(view) = loaded_view() else { return };
        let (mut job, _slot) = FirstRenderJob::new(view, "x".into());
        let spec = job.spec();
        assert_eq!(
            (spec.priority, spec.lane),
            (Priority::Foreground, Lane::Gpu)
        );
        assert_eq!(job.progress().done, 0);
        job.step().unwrap();
        assert_eq!(job.progress().done, 1);
    }

    #[test]
    fn a_batch_edit_during_the_render_is_deferred_not_lost() {
        assert_eq!(refresh_plan(Some(7), true, &[3, 7]), RefreshPlan::Defer);
        assert_eq!(refresh_plan(Some(7), false, &[3, 7]), RefreshPlan::Now);
        assert_eq!(refresh_plan(Some(7), true, &[3]), RefreshPlan::Skip);
        assert_eq!(refresh_plan(None, true, &[7]), RefreshPlan::Skip);
    }
}
