//! Source -> display conversion for 8-bit JPEG-sourced pixels: grid thumbnails and the T0/T2
//! embedded-preview tiers (#319, ADR-0042 amendment).
//!
//! [`crate::transform::DisplayTransform`] starts from the linear ProPhoto working space, so it
//! can't take a JPEG's own colors. These surfaces never enter the render graph; they hold
//! display-referred 8-bit RGBA in whatever space the JPEG's embedded ICC profile names (sRGB when it
//! has none). [`SourceTransforms`] converts that to the monitor's profile on the CPU, building one
//! `moxcms` transform per distinct source profile and caching it.
//!
//! Soft-proofing is deliberately not applied here -- previews show the photo, not a proof.

use crate::icc::color_profile;
use crate::space::OutputSpace;
use crate::transform::{equivalent_space, DisplayProfile};
use moxcms::Transform8BitExecutor;
use moxcms::{ColorProfile, DataColorSpace, Layout, RenderingIntent, TransformOptions};
use std::collections::hash_map::RandomState;
use std::collections::HashMap;
use std::hash::BuildHasher;
use std::sync::{Arc, Mutex};

/// Distinct source profiles kept built. Cameras and editors emit a handful (sRGB, Adobe RGB,
/// Display P3, a vendor sRGB variant); the cap only bounds a pathological library.
const MAX_CACHED: usize = 16;

/// Pixels converted per `moxcms` call. `transform` wants separate in/out slices; chunking lets us
/// convert in place with a small scratch copy instead of cloning a whole 3840 px frame.
const CHUNK_PIXELS: usize = 16 * 1024;

#[derive(Clone)]
enum Entry {
    /// Source and display are the same space: nothing to do, zero cost.
    Identity,
    Convert(Arc<Transform8BitExecutor>),
}

/// Converts 8-bit RGBA from a JPEG's own color space to the display profile it was built for.
///
/// Cheap to share (`Arc`) across Pounce worker threads and the UI thread.
pub struct SourceTransforms {
    display: Arc<ColorProfile>,
    display_space: Option<OutputSpace>,
    /// Keyed by (length, keyed hash) of the embedded ICC bytes rather than the bytes themselves:
    /// a JPEG can carry a profile of many MB and the cache must stay small whatever the library
    /// holds. `None` key = untagged (treated as sRGB).
    cache: Mutex<HashMap<Option<(usize, u64)>, Entry>>,
    hasher: RandomState,
}

impl SourceTransforms {
    pub fn new(display: &DisplayProfile) -> Self {
        let (display, display_space) = match display {
            DisplayProfile::Space(s) => (Arc::new(color_profile(*s)), Some(*s)),
            DisplayProfile::Icc(p) => (p.clone(), equivalent_space(p)),
        };
        Self {
            display,
            display_space,
            cache: Mutex::new(HashMap::new()),
            hasher: RandomState::new(),
        }
    }

    /// Converts `rgba` (8-bit, straight alpha untouched) in place from the space `icc` describes
    /// (`None`, or a profile that is unusable for RGB, means sRGB) to the display. Never fails:
    /// anything unconvertible leaves the pixels as they were, which is what happened before #319.
    pub fn convert_rgba8(&self, icc: Option<&[u8]>, rgba: &mut [u8]) {
        let Entry::Convert(exec) = self.entry_for(icc) else {
            return;
        };
        let mut scratch = vec![0u8; CHUNK_PIXELS * 4];
        for chunk in rgba.chunks_mut(CHUNK_PIXELS * 4) {
            let src = &mut scratch[..chunk.len()];
            src.copy_from_slice(chunk);
            if exec.transform(src, chunk).is_err() {
                // Length mismatch (a trailing partial pixel): put the original bytes back.
                chunk.copy_from_slice(src);
            }
        }
    }

    fn entry_for(&self, icc: Option<&[u8]>) -> Entry {
        let key = icc.map(|b| (b.len(), self.hasher.hash_one(b)));
        if let Some(e) = self.cache.lock().unwrap().get(&key) {
            return e.clone();
        }
        // Built outside the lock: a profile build can take milliseconds and other workers
        // converting already-cached profiles shouldn't wait on it. A duplicate build on a race
        // is harmless.
        let built = self.build(icc);
        let mut cache = self.cache.lock().unwrap();
        if cache.len() >= MAX_CACHED {
            cache.clear();
        }
        cache.insert(key, built.clone());
        built
    }

    fn build(&self, icc: Option<&[u8]>) -> Entry {
        // `moxcms` is third-party parsing code that has panicked on malformed profiles before
        // (see `display_profile::resolve`): a corrupt embedded ICC must degrade to sRGB, never take
        // down a worker.
        let tagged = icc.and_then(|bytes| {
            std::panic::catch_unwind(|| {
                ColorProfile::new_from_slice(bytes)
                    .ok()
                    .filter(|p| p.color_space == DataColorSpace::Rgb)
            })
            .ok()
            .flatten()
        });
        if let Some(src) = tagged {
            if let Some(entry) = self.build_from(&src) {
                return entry;
            }
        }
        self.build_from(&color_profile(OutputSpace::Srgb))
            .unwrap_or(Entry::Identity)
    }

    fn build_from(&self, src: &ColorProfile) -> Option<Entry> {
        // Same space on both sides (judged the way `DisplayTransform::build` judges a monitor
        // profile): converting would only add rounding.
        // The probe runs `moxcms` transforms too: a profile that parses but panics there must
        // degrade like one that fails to parse.
        let src_space = std::panic::catch_unwind(|| equivalent_space(src))
            .ok()
            .flatten();
        if let (Some(s), Some(d)) = (src_space, self.display_space) {
            if s == d {
                return Some(Entry::Identity);
            }
        }
        let options = TransformOptions {
            rendering_intent: RenderingIntent::RelativeColorimetric,
            allow_use_cicp_transfer: false,
            ..TransformOptions::default()
        };
        std::panic::catch_unwind(|| {
            src.create_transform_8bit(Layout::Rgba, &self.display, Layout::Rgba, options)
                .ok()
        })
        .ok()
        .flatten()
        .map(Entry::Convert)
    }
}

/// Pixels per `moxcms` call in [`rgba8_to_linear_prophoto16`]; bounds the u16 scratch.
const WIDE_CHUNK_PIXELS: usize = 16 * 1024;

/// ProPhoto (D50) primaries with a linear transfer curve: the working space as a target profile.
fn linear_working_profile() -> ColorProfile {
    let mut p = crate::icc::working_profile();
    let linear = moxcms::ToneReprCurve::Parametric(vec![1.0]);
    p.red_trc = Some(linear.clone());
    p.green_trc = Some(linear.clone());
    p.blue_trc = Some(linear);
    p
}

/// Converts 8-bit RGBA (alpha ignored) from the space `icc` names straight to **16-bit linear
/// ProPhoto (D50) RGB**, interleaved without alpha (#652). ProPhoto holds Adobe RGB and Display P3
/// without clipping, and the 8-bit source is widened before any curve is applied, so there is no
/// second 8-bit rounding.
///
/// `None` when the caller should use its plain sRGB path instead: no usable RGB profile, a
/// profile equivalent to sRGB (nothing wide to keep), or a transform that cannot be built or
/// panics inside `moxcms` (it parses untrusted profiles).
pub fn rgba8_to_linear_prophoto16(icc: &[u8], rgba: &[u8]) -> Option<Vec<u16>> {
    let exec = std::panic::catch_unwind(|| {
        let src = ColorProfile::new_from_slice(icc)
            .ok()
            .filter(|p| p.color_space == DataColorSpace::Rgb)?;
        if equivalent_space(&src) == Some(OutputSpace::Srgb) {
            return None;
        }
        let options = TransformOptions {
            rendering_intent: RenderingIntent::RelativeColorimetric,
            allow_use_cicp_transfer: false,
            ..TransformOptions::default()
        };
        src.create_transform_16bit(
            Layout::Rgba,
            &linear_working_profile(),
            Layout::Rgb,
            options,
        )
        .ok()
    })
    .ok()
    .flatten()?;
    let px = rgba.len() / 4;
    let mut out = vec![0u16; px * 3];
    let mut wide = vec![0u16; WIDE_CHUNK_PIXELS * 4];
    for (src, dst) in rgba
        .chunks(WIDE_CHUNK_PIXELS * 4)
        .zip(out.chunks_mut(WIDE_CHUNK_PIXELS * 3))
    {
        let n = src.len() / 4;
        for (w, &b) in wide.iter_mut().zip(&src[..n * 4]) {
            *w = u16::from(b) * 257;
        }
        exec.transform(&wide[..n * 4], &mut dst[..n * 3]).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::icc::profile_bytes;

    fn px(r: u8, g: u8, b: u8) -> Vec<u8> {
        vec![r, g, b, 255]
    }

    fn convert(display: OutputSpace, icc: Option<&[u8]>, rgba: Vec<u8>) -> Vec<u8> {
        let t = SourceTransforms::new(&DisplayProfile::Space(display));
        let mut v = rgba;
        t.convert_rgba8(icc, &mut v);
        v
    }

    #[test]
    fn wide_gamut_green_survives_as_linear_prophoto_without_clipping() {
        let icc = profile_bytes(OutputSpace::DisplayP3).unwrap();
        // Saturated P3 green is outside sRGB; in ProPhoto it is in range and distinct from the
        // sRGB-clipped result.
        let out = rgba8_to_linear_prophoto16(&icc, &px(0, 255, 0)).unwrap();
        assert_eq!(out.len(), 3);
        let back = OutputSpace::DisplayP3.from_working();
        let lin: Vec<f64> = (0..3)
            .map(|r| {
                (0..3)
                    .map(|c| back[r][c] * f64::from(out[c]) / 65535.0)
                    .sum::<f64>()
            })
            .collect();
        // Back in P3 linear it is (0, 1, 0) again, to 16-bit precision.
        assert!(
            lin[0].abs() < 0.002 && (lin[1] - 1.0).abs() < 0.002 && lin[2].abs() < 0.002,
            "{lin:?}"
        );
        let srgb = OutputSpace::Srgb.from_working();
        let r: f64 = (0..3)
            .map(|c| srgb[0][c] * f64::from(out[c]) / 65535.0)
            .sum();
        assert!(
            r < -0.05,
            "P3 green must be outside sRGB (negative red {r})"
        );
    }

    #[test]
    fn srgb_equivalent_and_garbage_profiles_defer_to_the_plain_path() {
        let srgb = profile_bytes(OutputSpace::Srgb).unwrap();
        assert!(rgba8_to_linear_prophoto16(&srgb, &px(1, 2, 3)).is_none());
        assert!(rgba8_to_linear_prophoto16(&[0xAB; 300], &px(1, 2, 3)).is_none());
    }

    #[test]
    fn neutral_stays_neutral_through_the_wide_path() {
        let icc = profile_bytes(OutputSpace::AdobeRgb).unwrap();
        let out = rgba8_to_linear_prophoto16(&icc, &px(128, 128, 128)).unwrap();
        let d = |a: u16, b: u16| (i32::from(a) - i32::from(b)).abs();
        assert!(d(out[0], out[1]) < 40 && d(out[1], out[2]) < 40, "{out:?}");
        // Adobe RGB 128 = (128/255)^2.2 ~= 0.2195 linear.
        assert!(
            (f64::from(out[1]) / 65535.0 - 0.2195).abs() < 0.01,
            "{out:?}"
        );
    }

    #[test]
    fn untagged_on_an_srgb_display_is_byte_identical() {
        let src = px(200, 30, 90);
        assert_eq!(convert(OutputSpace::Srgb, None, src.clone()), src);
    }

    #[test]
    fn a_tagged_srgb_profile_on_an_srgb_display_is_byte_identical() {
        let icc = profile_bytes(OutputSpace::Srgb).unwrap();
        let src = px(12, 240, 128);
        assert_eq!(convert(OutputSpace::Srgb, Some(&icc), src.clone()), src);
    }

    #[test]
    fn saturated_p3_red_is_clipped_into_an_srgb_display() {
        let icc = profile_bytes(OutputSpace::DisplayP3).unwrap();
        let out = convert(OutputSpace::Srgb, Some(&icc), px(255, 0, 0));
        // P3 red is outside sRGB: relative colorimetric clips to the sRGB red corner.
        assert_eq!(out, px(255, 0, 0));
        // A mid P3 color is inside sRGB and must come out with *different* numbers (P3 -> sRGB
        // lowers a saturated channel's value).
        let src = px(200, 120, 60);
        let out = convert(OutputSpace::Srgb, Some(&icc), src.clone());
        assert_ne!(out, src);
        assert!(out[0] >= src[0], "P3 red channel grows in sRGB: {out:?}");
        assert_eq!(out[3], 255, "alpha untouched");
    }

    #[test]
    fn untagged_srgb_on_a_p3_display_is_desaturated_in_p3_numbers() {
        let src = px(255, 0, 0);
        let out = convert(OutputSpace::DisplayP3, None, src.clone());
        // sRGB red is inside P3: encoded as less than full red, with some green.
        assert!(out[0] < 255 && out[1] > 0, "{out:?}");
    }

    #[test]
    fn p3_on_a_p3_display_is_identity() {
        let icc = profile_bytes(OutputSpace::DisplayP3).unwrap();
        let src = px(10, 200, 99);
        assert_eq!(
            convert(OutputSpace::DisplayP3, Some(&icc), src.clone()),
            src
        );
    }

    #[test]
    fn garbage_icc_degrades_to_srgb_without_panicking() {
        let junk = b"definitely not an icc profile".to_vec();
        let src = px(1, 2, 3);
        assert_eq!(
            convert(OutputSpace::Srgb, Some(&junk), src.clone()),
            src,
            "falls back to sRGB source, which is identity on an sRGB display"
        );
        let on_p3 = convert(OutputSpace::DisplayP3, Some(&junk), px(255, 0, 0));
        let untagged = convert(OutputSpace::DisplayP3, None, px(255, 0, 0));
        assert_eq!(on_p3, untagged);
    }

    #[test]
    fn a_profile_is_built_once_and_reused() {
        let icc = profile_bytes(OutputSpace::AdobeRgb).unwrap();
        let t = SourceTransforms::new(&DisplayProfile::Space(OutputSpace::Srgb));
        let mut a = px(50, 60, 70);
        t.convert_rgba8(Some(&icc), &mut a);
        let mut b = px(50, 60, 70);
        t.convert_rgba8(Some(&icc), &mut b);
        assert_eq!(a, b);
        assert_eq!(t.cache.lock().unwrap().len(), 1);
    }

    #[test]
    fn large_buffers_convert_across_chunk_boundaries() {
        let icc = profile_bytes(OutputSpace::DisplayP3).unwrap();
        let pixels = CHUNK_PIXELS * 2 + 7;
        let src: Vec<u8> = (0..pixels).flat_map(|_| [200u8, 120, 60, 255]).collect();
        let out = convert(OutputSpace::Srgb, Some(&icc), src);
        let first = out[..4].to_vec();
        assert!(out.chunks(4).all(|p| p == first.as_slice()));
        assert_ne!(first, px(200, 120, 60));
    }

    /// `cargo test -p nicti-calico --release -- --ignored --nocapture convert_cost` (#319).
    #[test]
    #[ignore = "timing measurement, run with --release"]
    fn convert_cost() {
        let p3 = profile_bytes(OutputSpace::DisplayP3).unwrap();
        for (label, w, h) in [("256px thumbnail", 256, 170), ("T2 3840px", 3840, 2560)] {
            let src: Vec<u8> = (0..w * h)
                .flat_map(|i| [(i % 251) as u8, (i % 241) as u8, (i % 239) as u8, 255])
                .collect();
            for (name, display, icc) in [
                ("untagged -> sRGB (identity)", OutputSpace::Srgb, None),
                ("P3 -> sRGB", OutputSpace::Srgb, Some(p3.as_slice())),
                ("untagged -> P3", OutputSpace::DisplayP3, None),
            ] {
                let t = SourceTransforms::new(&DisplayProfile::Space(display));
                let mut warm = src.clone();
                t.convert_rgba8(icc, &mut warm); // builds + caches the transform
                let runs = if w > 1000 { 5 } else { 200 };
                let start = std::time::Instant::now();
                for _ in 0..runs {
                    let mut px = src.clone();
                    t.convert_rgba8(icc, &mut px);
                    std::hint::black_box(&px);
                }
                println!(
                    "{label:>16} {name:<28} {:>8.3} ms",
                    start.elapsed().as_secs_f64() * 1e3 / runs as f64
                );
            }
        }
    }
}
