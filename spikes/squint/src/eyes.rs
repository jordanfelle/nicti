//! Eye-state candidates (#34/ADR-0034): human closed-eye detection, and fursuit "eyes obscured"
//! detection (per the user's 2026-09-27 scope decision: fixed/painted fursuit-head eyes can't
//! blink, so the fursuit check is "can the eyes be seen at all," not "are they open").
//!
//! **Deliberately no bundled runnable candidate this pass** -- unlike `sharp`/`synth` (pure Rust,
//! nothing to source), every real candidate here needs a downloaded model, and this spike didn't
//! get far enough to run one end-to-end against real weights the way `spikes/litter` did for
//! DINOv2. Shipping a stub that *looks* like a working detector but was never run against a real
//! model would be exactly the kind of fabricated-looking-real result this project's ADRs go out of
//! their way to avoid (see ADR-0033's own "everything not gated on X is real, tested, measured
//! evidence" framing) -- so this module states the research findings and a candidate interface
//! only, honestly unmeasured, rather than faking a result.
//!
//! ## Human closed-eye candidates (license-checked, not yet run)
//!
//! - **MediaPipe Face Landmarker** (Apache-2.0, Google) -- ships blendshape outputs including
//!   `eyeBlinkLeft`/`eyeBlinkRight` directly, no separate eye-aspect-ratio geometry needed. Ships
//!   as a `.task` bundle (TFLite-based), not ONNX -- would need either a TFLite runtime (a new
//!   dependency class this project hasn't taken on anywhere else, see `docs/adr/0018`) or a
//!   community ONNX re-export (re-verify that re-export's own license separately from MediaPipe's,
//!   same "re-exporter's license isn't the upstream model's license" caveat `docs/licensing.md`'s
//!   NAFNet/SCUNet rows already document for `deepghs/image_restoration`).
//! - **YuNet** (OpenCV Zoo, Apache-2.0) -- a small (~340KB) ONNX face detector with 5-point
//!   landmarks (eye centers among them). No blink/openness signal on its own; would need eye-
//!   aspect-ratio computed from a denser landmark model layered on top, or a small dedicated
//!   classifier crop-fed from YuNet's face box.
//! - Explicitly **not** InsightFace/RetinaFace -- `docs/adr/0018` already excludes these
//!   (non-commercial-only license restriction), independent of #34's own scope.
//!
//! ## Fursuit "eyes obscured" candidates (license-checked, not yet run)
//!
//! - **Open-vocabulary detection** (OWLv2, Apache-2.0 per Google's model card; Grounding DINO,
//!   license mixed across forks -- the original IDEA-Research release is Apache-2.0 but re-checked
//!   per-checkpoint before use) prompted with text queries like `"fursuit head"`/`"animal
//!   head"`/`"eyes"` -- localizes a head box, then a second query for `"visible eyes"` inside it as
//!   a coarse presence/absence signal. Never independently verified against a real fursuit photo in
//!   this pass.
//! - **DINOv2 patch-feature linear probe** -- reusing `spikes/litter`'s own DINOv2 wrapper shape
//!   (same embedding, ADR-0033's Decision already flags DINOv2 as the natural shared default for
//!   #33/#35), trained on real labelled crops from `label.rs`'s eventual output. Needs real labels
//!   to train on before it's anything but an unfitted architecture choice.
//!
//! ## What's real in this module
//!
//! Just the plumbing every future candidate needs to plug into the same eval harness `sharp`'s
//! candidates use: an [`EyeTag`] enum matching `label.rs`'s labelling categories, and an
//! [`EyeCandidate`] trait so `metrics::Confusion` scoring doesn't care whether a candidate is a
//! pure-Rust heuristic or a real model wrapper once one exists.

use image::RgbImage;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EyeTag {
    /// A human face is visible and both eyes appear open.
    HumanEyesOpen,
    /// A human face is visible and at least one eye appears closed (blink/squint).
    HumanEyesClosed,
    /// A fursuit head is visible and its eyes are visible (open by construction -- fixed/painted
    /// eyes don't blink, see this module's own doc comment).
    FursuitEyesVisible,
    /// A fursuit head is visible but its eyes are not (turned away, hair/hand/prop occlusion).
    FursuitEyesObscured,
    /// No face or fursuit head detected in the frame at all.
    NotApplicable,
}

/// A pluggable eye-state candidate over a cropped head/face region. No implementation of this
/// trait ships in this pass -- see the module doc comment.
pub trait EyeCandidate {
    fn name(&self) -> &'static str;
    fn classify(&self, head_crop: &RgbImage) -> EyeTag;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct AlwaysNotApplicable;
    impl EyeCandidate for AlwaysNotApplicable {
        fn name(&self) -> &'static str {
            "always_na"
        }
        fn classify(&self, _head_crop: &RgbImage) -> EyeTag {
            EyeTag::NotApplicable
        }
    }

    #[test]
    fn candidate_trait_is_object_safe_and_callable() {
        let candidates: Vec<Box<dyn EyeCandidate>> = vec![Box::new(AlwaysNotApplicable)];
        let img = RgbImage::new(4, 4);
        for c in &candidates {
            assert_eq!(c.classify(&img), EyeTag::NotApplicable);
            assert_eq!(c.name(), "always_na");
        }
    }
}
