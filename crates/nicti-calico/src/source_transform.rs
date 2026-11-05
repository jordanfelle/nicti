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
use crate::transform::{
    equivalent_space, has_lifted_black, shaper_table_u16, DisplayKind, DisplayProfile,
    DisplayTransform, Lut3d,
};
use moxcms::{ColorProfile, DataColorSpace, Layout, RenderingIntent, TransformOptions};
use moxcms::{Transform16BitExecutor, Transform8BitExecutor};
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
    /// Black-point compensated (#624): source -> linear ProPhoto -> the display's BPC-baked LUT,
    /// the same LUT the GPU applies to the render, so previews and the edit agree on shadows.
    Compensated(Arc<Transform16BitExecutor>, Arc<Lut3d>),
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
    /// The display's black-point-compensated LUT, when compensation applies (a LUT-based monitor
    /// profile and BPC on). `None`: the plain direct transform.
    bpc_lut: Option<Arc<Lut3d>>,
}

impl SourceTransforms {
    pub fn new(display: &DisplayProfile) -> Self {
        Self::new_with(display, false)
    }

    /// Like [`Self::new`], with black-point compensation (#320/#624) when `bpc` is set. It takes
    /// effect exactly where the monitor transform's does -- a LUT-based monitor profile; sRGB,
    /// built-in spaces and matrix/TRC profiles are converted as before -- so a thumbnail's shadows
    /// match the same photo in the Develop view.
    pub fn new_with(display: &DisplayProfile, bpc: bool) -> Self {
        let bpc_lut = if bpc {
            // Same panic guard as every other `moxcms` use on an untrusted profile. Only a monitor
            // whose black really is lifted gets the (slower, LUT-based) compensated path.
            std::panic::catch_unwind(|| {
                if let DisplayProfile::Icc(p) = display {
                    if !has_lifted_black(p) {
                        return None;
                    }
                }
                match DisplayTransform::build_with(display, None, true) {
                    Ok(DisplayTransform {
                        kind: DisplayKind::Lut(lut),
                        ..
                    }) => Some(Arc::new(lut)),
                    _ => None,
                }
            })
            .ok()
            .flatten()
        } else {
            None
        };
        Self::build_new(display, bpc_lut)
    }

    /// Forces the LUT path (even for a profile the monitor transform would evaluate analytically),
    /// with compensation when `bpc` is set. For tests and tools.
    pub fn with_lut_bpc(profile: &ColorProfile, bpc: bool) -> Result<Self, moxcms::CmsError> {
        let t = DisplayTransform::with_lut_bpc(profile, None, bpc)?;
        let lut = match t.kind {
            DisplayKind::Lut(lut) if bpc && has_lifted_black(profile) => Some(Arc::new(lut)),
            _ => None,
        };
        Ok(Self::build_new(
            &DisplayProfile::Icc(Arc::new(profile.clone())),
            lut,
        ))
    }

    /// A converter for `display` that compensates through `bpc_lut` -- the BPC-baked LUT the
    /// caller's display transform already holds, so the 33^3 bake is not repeated. `None` = the
    /// plain direct transform. Pass a LUT only when compensation applies
    /// ([`has_lifted_black`]): it makes every preview pixel take the slower two-stage path.
    pub fn from_bpc_lut(display: &DisplayProfile, bpc_lut: Option<Arc<Lut3d>>) -> Self {
        Self::build_new(display, bpc_lut)
    }

    /// Whether this converter compensates (and so differs from a plain one for the same display).
    pub fn bpc_active(&self) -> bool {
        self.bpc_lut.is_some()
    }

    fn build_new(display: &DisplayProfile, bpc_lut: Option<Arc<Lut3d>>) -> Self {
        let (display, display_space) = match display {
            DisplayProfile::Space(s) => (Arc::new(color_profile(*s)), Some(*s)),
            DisplayProfile::Icc(p) => (p.clone(), equivalent_space(p)),
        };
        Self {
            display,
            display_space,
            cache: Mutex::new(HashMap::new()),
            hasher: RandomState::new(),
            bpc_lut,
        }
    }

    /// Converts `rgba` (8-bit, straight alpha untouched) in place from the space `icc` describes
    /// (`None`, or a profile that is unusable for RGB, means sRGB) to the display. Never fails:
    /// anything unconvertible leaves the pixels as they were, which is what happened before #319.
    pub fn convert_rgba8(&self, icc: Option<&[u8]>, rgba: &mut [u8]) {
        let exec = match self.entry_for(icc) {
            Entry::Identity => return,
            Entry::Compensated(to_working, lut) => {
                convert_compensated(&*to_working, &lut, rgba);
                return;
            }
            Entry::Convert(exec) => exec,
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
        // A poisoned lock still holds a valid cache (an entry is inserted whole): use it.
        if let Some(e) = self
            .cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
        {
            return e.clone();
        }
        // Built outside the lock: a profile build can take milliseconds and other workers
        // converting already-cached profiles shouldn't wait on it. A duplicate build on a race
        // is harmless.
        let built = self.build(icc);
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
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
        if let Some(lut) = &self.bpc_lut {
            // No "same space as the display" shortcut here: the display is a LUT profile, and even
            // an sRGB source must go through the compensated LUT.
            let options = TransformOptions {
                rendering_intent: RenderingIntent::RelativeColorimetric,
                allow_use_cicp_transfer: false,
                ..TransformOptions::default()
            };
            return std::panic::catch_unwind(|| {
                src.create_transform_16bit(
                    Layout::Rgba,
                    &linear_working_profile(),
                    Layout::Rgb,
                    options,
                )
                .ok()
            })
            .ok()
            .flatten()
            .map(|t| Entry::Compensated(t, lut.clone()));
        }
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

/// Converts `rgba` in place through `to_working` (source -> 16-bit linear ProPhoto) and the
/// compensated display `lut`. Alpha is left as it was. A chunk the engine rejects is left as it was.
fn convert_compensated(
    to_working: &(dyn moxcms::TransformExecutor<u16> + Send + Sync),
    lut: &Lut3d,
    rgba: &mut [u8],
) {
    let shaper = shaper_table_u16();
    let mut wide = vec![0u16; CHUNK_PIXELS * 4];
    let mut linear = vec![0u16; CHUNK_PIXELS * 3];
    for chunk in rgba.chunks_mut(CHUNK_PIXELS * 4) {
        let n = chunk.len() / 4;
        for (w, &b) in wide.iter_mut().zip(&chunk[..n * 4]) {
            *w = u16::from(b) * 257;
        }
        if to_working
            .transform(&wide[..n * 4], &mut linear[..n * 3])
            .is_err()
        {
            continue;
        }
        for (px, lin) in chunk
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(linear.as_chunks::<3>().0)
        {
            let out = lut.sample_shaped([
                shaper[usize::from(lin[0])],
                shaper[usize::from(lin[1])],
                shaper[usize::from(lin[2])],
            ]);
            for c in 0..3 {
                px[c] = (out[c].clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
            }
        }
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

    /// sRGB-primaries monitor whose device black shows `floor` linear light (a lifted black).
    fn lifted_black_monitor(floor: f64) -> ColorProfile {
        let mut p = ColorProfile::new_srgb();
        let table = || -> moxcms::ToneReprCurve {
            moxcms::ToneReprCurve::Lut(
                (0..1024)
                    .map(|i| {
                        let e = f64::from(i) / 1023.0;
                        let lin = if e <= 0.04045 {
                            e / 12.92
                        } else {
                            ((e + 0.055) / 1.055).powf(2.4)
                        };
                        ((floor + lin * (1.0 - floor)) * 65535.0).round() as u16
                    })
                    .collect(),
            )
        };
        p.red_trc = Some(table());
        p.green_trc = Some(table());
        p.blue_trc = Some(table());
        p
    }

    fn gray(t: &SourceTransforms, icc: Option<&[u8]>, v: u8) -> u8 {
        let mut px = vec![v, v, v, 200];
        t.convert_rgba8(icc, &mut px);
        assert_eq!(px[3], 200, "alpha is untouched");
        px[1]
    }

    #[test]
    fn bpc_keeps_jpeg_shadow_detail_a_lifted_black_monitor_would_crush() {
        let monitor = lifted_black_monitor(0.02);
        let off = SourceTransforms::with_lut_bpc(&monitor, false).unwrap();
        let on = SourceTransforms::with_lut_bpc(&monitor, true).unwrap();
        // Without compensation every source shadow below the monitor's black collapses...
        let (a, b) = (gray(&off, None, 6), gray(&off, None, 14));
        assert_eq!(a, b, "off: {a} vs {b}");
        // ...with it they stay distinct and ordered, black stays the darkest the monitor can
        // show, and white stays white.
        let (a, b, c) = (gray(&on, None, 6), gray(&on, None, 14), gray(&on, None, 40));
        assert!(a + 2 <= b && b + 2 <= c, "on: {a} {b} {c}");
        assert!(gray(&on, None, 0) < 40, "black");
        assert!(gray(&on, None, 255) >= 253, "white");
    }

    #[test]
    fn a_compensated_preview_matches_the_display_transform_the_render_gets() {
        // Same LUT, same input: a thumbnail's pixel equals what the GPU path gives the render at
        // that colour (to the 8-bit rounding and the 16-bit linear intermediate).
        let monitor = lifted_black_monitor(0.02);
        let on = SourceTransforms::with_lut_bpc(&monitor, true).unwrap();
        let display = DisplayTransform::with_lut_bpc(&monitor, None, true).unwrap();
        let srgb = OutputSpace::Srgb;
        for v in [4u8, 12, 30, 90, 200] {
            let enc = f32::from(v) / 255.0;
            let lin = srgb.decode(enc);
            let inv = crate::math::mat_invert(&srgb.from_working());
            let w = crate::math::mat_vec_mul(&inv, [f64::from(lin); 3]).map(|c| c as f32);
            let (want, _) = display.apply(w);
            let got = gray(&on, None, v);
            assert!(
                (f32::from(got) / 255.0 - want[1]).abs() < 2.0 / 255.0,
                "v={v}: got {got}, display gives {}",
                want[1] * 255.0
            );
        }
    }

    #[test]
    fn a_true_black_monitor_and_a_builtin_display_keep_the_plain_path_byte_for_byte() {
        // A true-black LUT monitor has nothing to compensate: it must not pay for the two-stage
        // path (or lose accuracy to the LUT), whatever the flag says.
        let srgb_monitor = ColorProfile::new_srgb();
        let on = SourceTransforms::with_lut_bpc(&srgb_monitor, true).unwrap();
        assert!(!on.bpc_active());
        let plain = SourceTransforms::new(&DisplayProfile::Icc(Arc::new(srgb_monitor)));
        let aw = profile_bytes(OutputSpace::AdobeRgb).unwrap();
        let mut a: Vec<u8> = (0..=255u8).flat_map(|v| [v, 255 - v, v / 2, 255]).collect();
        let mut b = a.clone();
        on.convert_rgba8(Some(&aw), &mut a);
        plain.convert_rgba8(Some(&aw), &mut b);
        assert_eq!(a, b);
        // A built-in display ignores the flag too, and `new_with(.., false)` is `new`.
        let p3 = DisplayProfile::Space(OutputSpace::DisplayP3);
        let (x, y) = (
            SourceTransforms::new(&p3),
            SourceTransforms::new_with(&p3, true),
        );
        assert!(!y.bpc_active());
        let mut a = vec![200, 40, 90, 255, 10, 220, 30, 255];
        let mut b = a.clone();
        x.convert_rgba8(Some(&aw), &mut a);
        y.convert_rgba8(Some(&aw), &mut b);
        assert_eq!(a, b);
        // A lifted-black monitor with the option on is the compensated converter; off, it is not.
        let lifted = lifted_black_monitor(0.02);
        assert!(SourceTransforms::with_lut_bpc(&lifted, true)
            .unwrap()
            .bpc_active());
        assert!(!SourceTransforms::with_lut_bpc(&lifted, false)
            .unwrap()
            .bpc_active());
    }

    #[test]
    fn compensation_only_moves_the_shadows_midtones_and_highlights_stay_close_to_the_direct_transform(
    ) {
        // The compensated path goes through the 33^3 LUT, so away from the shadows it must stay
        // within the LUT's own error of the exact direct transform (which has no compensation).
        let monitor = lifted_black_monitor(0.02);
        let on = SourceTransforms::with_lut_bpc(&monitor, true).unwrap();
        let direct = SourceTransforms::new(&DisplayProfile::Icc(Arc::new(monitor)));
        for v in (96..=255u16).step_by(8) {
            let (a, b) = (gray(&on, None, v as u8), gray(&direct, None, v as u8));
            assert!(a.abs_diff(b) <= 8, "v={v}: compensated {a} vs direct {b}");
        }
    }

    #[test]
    fn a_tagged_wide_gamut_source_still_converts_through_the_compensated_path() {
        let monitor = lifted_black_monitor(0.02);
        let on = SourceTransforms::with_lut_bpc(&monitor, true).unwrap();
        let aw = profile_bytes(OutputSpace::AdobeRgb).unwrap();
        let mut tagged = px(0, 200, 0);
        let mut untagged = px(0, 200, 0);
        on.convert_rgba8(Some(&aw), &mut tagged);
        on.convert_rgba8(None, &mut untagged);
        assert_ne!(tagged, untagged, "the profile must be honoured");
        // A garbage profile degrades to sRGB, never panics.
        let mut garbage = px(0, 200, 0);
        on.convert_rgba8(Some(&[0xAB; 300]), &mut garbage);
        assert_eq!(garbage, untagged);
        // A trailing partial pixel leaves the original bytes.
        let mut odd = vec![10, 20, 30, 255, 1, 2];
        on.convert_rgba8(None, &mut odd);
        assert_eq!(&odd[4..], &[1, 2]);
    }
}
