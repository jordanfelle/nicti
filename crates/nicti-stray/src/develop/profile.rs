//! `CameraProfile` (#381): the name of the camera profile LRC renders the photo with ("Adobe
//! Standard", "Camera Landscape", an Adobe Raw Look such as "Adobe Vivid", ...).
//!
//! The translator only extracts the name. Turning it into a `.dcp`/Look the user actually has
//! installed needs the filesystem and the photo's camera make/model, so that happens in the import
//! job through its `ProfileResolver` (see `job.rs`) -- keeping `translate` pure.
//!
//! Skipped on purpose (no profile is requested):
//! - `Embedded`: the profile lives inside the file (a DNG's), nothing to look up.
//! - `Adobe Standard`: what LRC writes into virtually every untouched raw. nicti's own default (no
//!   profile selected) is the plain camera matrix, and taking this one would mark every imported
//!   photo as edited -- the same reason a zero-amount grain slider is ignored (`effects.rs`).

use super::Tx;

/// Profile names LRC writes for an untouched photo or one that carries its own profile.
const NOT_A_CHOICE: &[&str] = &["adobe standard", "embedded"];

pub(super) fn apply(tx: &mut Tx) {
    let name = tx.text("CameraProfile").map(|n| n.trim().to_string());
    tx.camera_profile =
        name.filter(|n| !n.is_empty() && !NOT_A_CHOICE.iter().any(|d| n.eq_ignore_ascii_case(d)));
}

#[cfg(test)]
mod tests {
    use super::super::{translate, Context};

    fn name(text: &str) -> Option<String> {
        translate(text, &Context::default()).unwrap().camera_profile
    }

    #[test]
    fn a_chosen_profile_name_is_extracted() {
        assert_eq!(
            name(r#"s = { CameraProfile = "Camera Landscape" }"#).as_deref(),
            Some("Camera Landscape")
        );
        assert_eq!(
            name(r#"s = { CameraProfile = " Adobe Vivid " }"#).as_deref(),
            Some("Adobe Vivid")
        );
    }

    #[test]
    fn the_untouched_default_and_embedded_request_nothing() {
        for n in ["Adobe Standard", "adobe standard", "Embedded", ""] {
            assert_eq!(
                name(&format!(r#"s = {{ CameraProfile = "{n}" }}"#)),
                None,
                "{n:?}"
            );
        }
        assert_eq!(name("s = { Exposure2012 = 1 }"), None);
    }

    #[test]
    fn the_key_and_its_digest_are_never_listed_as_untranslated() {
        let t = translate(
            r#"s = { CameraProfile = "Camera Flat", CameraProfileDigest = "ABC123" }"#,
            &Context::default(),
        )
        .unwrap();
        assert!(t.untranslated.is_empty(), "{:?}", t.untranslated);
    }
}
