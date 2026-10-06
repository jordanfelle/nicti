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
use std::collections::HashMap;
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
    /// `None` key = untagged (treated as sRGB).
    cache: Mutex<HashMap<Option<Vec<u8>>, Entry>>,
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
        let key = icc.map(<[u8]>::to_vec);
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
        if let (Some(s), Some(d)) = (equivalent_space(src), self.display_space) {
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
