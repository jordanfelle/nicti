//! The interim sky mask: a non-AI heuristic, registered as an ordinary segmentation provider.
//!
//! No sky model with clean provenance is adopted yet (ADR-0048: RapidRAW's U-2-Net `skyseg`
//! fine-tune has unverified training data), so v1 ships this deliberately simple fallback, labelled
//! *beta* in the UI: a pixel is sky if it is bright and blue-dominant **and** connected to the top
//! row through other such pixels (4-connected flood fill), which keeps a white wall or a blue car
//! lower in the frame out of the mask. The tapetum guided-filter refine then snaps its blocky,
//! binary edge to the photo's real edges. A real sky model is a follow-up ticket; when it lands it
//! is one more registration under a new `model_id`, and existing edits keep this one.
//!
//! Ported from `spikes/siamese/src/sky.rs`, retargeted at the neutral (display-referred) image.

use nicti_claw::Module;
use nicti_stalk::{
    AlphaMap, LoadContext, ModelImage, ModelProvider, SegmentError, SegmentTarget,
    SegmentationProvider, Segmenter,
};
use serde_json::Value;

pub const SKY_ID: &str = "nicti.mask.sky_heuristic";
/// Bump when the rule changes: an edit pins the version it was made with.
pub const SKY_VERSION: &str = "1";

/// Minimum luminance (of the display-referred image) for a pixel to count as sky.
pub const MIN_LUMINANCE: f32 = 0.35;
/// Blue must exceed red by this factor...
pub const BLUE_OVER_RED: f32 = 1.02;
/// ...and be at least this fraction of green.
pub const BLUE_OVER_GREEN: f32 = 0.95;

fn looks_like_sky(px: &[f32]) -> bool {
    let (r, g, b) = (px[0], px[1], px[2]);
    let luminance = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    luminance > MIN_LUMINANCE && b > r * BLUE_OVER_RED && b >= g * BLUE_OVER_GREEN
}

/// The sky alpha of `image`: 1 for sky, 0 elsewhere, at the image's own resolution.
pub fn sky_alpha(image: &ModelImage) -> Result<AlphaMap, SegmentError> {
    image.validate()?;
    let (w, h) = (image.width, image.height);
    let px = |x: usize, y: usize| &image.rgb[(y * w + x) * 3..(y * w + x) * 3 + 3];
    let mut out = vec![0.0f32; w * h];
    let mut visited = vec![false; w * h];
    let mut stack: Vec<(usize, usize)> = (0..w)
        .filter(|&x| looks_like_sky(px(x, 0)))
        .map(|x| (x, 0))
        .collect();
    while let Some((x, y)) = stack.pop() {
        let i = y * w + x;
        if visited[i] {
            continue;
        }
        visited[i] = true;
        out[i] = 1.0;
        for (nx, ny) in [
            (x.wrapping_sub(1), y),
            (x + 1, y),
            (x, y.wrapping_sub(1)),
            (x, y + 1),
        ] {
            if nx < w && ny < h && !visited[ny * w + nx] && looks_like_sky(px(nx, ny)) {
                stack.push((nx, ny));
            }
        }
    }
    AlphaMap::new(w, h, out)
}

pub struct SkyProvider;

impl Module for SkyProvider {
    fn id(&self) -> &str {
        SKY_ID
    }
    fn schema_version(&self) -> u32 {
        1
    }
    fn migrate_params(&self, _from_version: u32, params: Value) -> Option<Value> {
        Some(params)
    }
}

impl ModelProvider for SkyProvider {
    fn label(&self) -> &str {
        "Sky (heuristic, beta)"
    }
}

impl SegmentationProvider for SkyProvider {
    fn model_version(&self) -> &str {
        SKY_VERSION
    }

    fn targets(&self) -> &[SegmentTarget] {
        &[SegmentTarget::Sky]
    }

    fn load(&self, _ctx: &LoadContext) -> Result<Box<dyn Segmenter>, SegmentError> {
        Ok(Box::new(SkySegmenter))
    }
}

struct SkySegmenter;

impl Segmenter for SkySegmenter {
    fn segment(&mut self, image: &ModelImage, params: &Value) -> Result<AlphaMap, SegmentError> {
        match SegmentTarget::from_params(params)? {
            SegmentTarget::Sky => sky_alpha(image),
            other => Err(SegmentError::UnsupportedTarget(other.as_str().to_owned())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(w: usize, h: usize, f: impl Fn(usize, usize) -> [f32; 3]) -> Vec<f32> {
        (0..w * h).flat_map(|i| f(i % w, i / w)).collect()
    }

    fn view(w: usize, h: usize, rgb: &[f32]) -> ModelImage<'_> {
        ModelImage {
            width: w,
            height: h,
            rgb,
            image_key: 1,
        }
    }

    #[test]
    fn a_uniformly_blue_image_is_entirely_sky_and_a_brown_one_has_none() {
        let blue = image(8, 8, |_, _| [0.4, 0.5, 0.6]);
        assert!(sky_alpha(&view(8, 8, &blue))
            .unwrap()
            .alpha
            .iter()
            .all(|&v| v == 1.0));
        let brown = image(8, 8, |_, _| [0.5, 0.3, 0.1]);
        assert!(sky_alpha(&view(8, 8, &brown))
            .unwrap()
            .alpha
            .iter()
            .all(|&v| v == 0.0));
    }

    #[test]
    fn sky_is_connected_to_the_top_so_a_bright_blue_patch_below_is_not_picked_up() {
        let (w, h) = (10, 10);
        let rgb = image(w, h, |x, y| {
            let sky = y < 2 || (y == 8 && (3..=4).contains(&x));
            if sky {
                [0.4, 0.5, 0.6]
            } else {
                [0.5, 0.3, 0.1]
            }
        });
        let a = sky_alpha(&view(w, h, &rgb)).unwrap();
        assert_eq!(a.alpha[3], 1.0);
        assert_eq!(
            a.alpha[8 * w + 3],
            0.0,
            "disconnected patch must not leak in"
        );
    }

    #[test]
    fn the_sky_boundary_follows_the_horizon() {
        let (w, h) = (16, 12);
        let rgb = image(w, h, |_, y| {
            if y < 5 {
                [0.5, 0.6, 0.8]
            } else {
                [0.2, 0.3, 0.1]
            }
        });
        let a = sky_alpha(&view(w, h, &rgb)).unwrap();
        for y in 0..h {
            let want = if y < 5 { 1.0 } else { 0.0 };
            assert_eq!(a.alpha[y * w + 7], want, "row {y}");
        }
    }

    #[test]
    fn a_bad_buffer_is_an_error_not_a_panic() {
        let short = vec![0.5f32; 5];
        assert!(matches!(
            sky_alpha(&view(4, 4, &short)),
            Err(SegmentError::BadInput(_))
        ));
    }

    #[test]
    fn the_provider_only_serves_the_sky_target() {
        let p = SkyProvider;
        assert_eq!(p.targets(), &[SegmentTarget::Sky]);
        assert_eq!(p.model_version(), SKY_VERSION);
        assert!(p.artifacts().is_empty(), "nothing to download");
        let store = nicti_stalk::models::ModelStore::new(std::env::temp_dir().join("nicti-sky"));
        let mut seg = p
            .load(&LoadContext {
                store: &store,
                ort_dylib: None,
            })
            .unwrap();
        let rgb = image(4, 4, |_, _| [0.4, 0.5, 0.6]);
        assert!(seg
            .segment(&view(4, 4, &rgb), &serde_json::json!({ "target": "sky" }))
            .is_ok());
        assert_eq!(
            seg.segment(
                &view(4, 4, &rgb),
                &serde_json::json!({ "target": "subject" })
            ),
            Err(SegmentError::UnsupportedTarget("subject".into()))
        );
    }
}
