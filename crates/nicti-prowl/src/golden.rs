//! Golden-image regression tests: render a reference NEF, perceptually diff it against a stored
//! golden, and fail past a per-render threshold.
//!
//! Nothing in this repo can render a NEF yet (#37/#38 are still open research, every
//! `nicti-decode`/`nicti-render` crate is a trait boundary only -- see CLAUDE.md's package map),
//! so this module is deliberately built against a pluggable [`Render`] trait rather than a real
//! decoder, and is only tested with synthetic images. The real goldens land once #41 (RAW ->
//! linear -> working-space pipeline) exists -- see the follow-up issue this PR files.
//!
//! The perceptual diff is a hand-rolled single-scale SSIM (Wang et al., the standard formula,
//! computed over 8x8 windows on each of R/G/B and averaged) rather than pulling in `dssim-core`: that crate's own
//! published license string (`AGPL-3.0`, no `-only`/`-or-later` suffix) doesn't match any SPDX id
//! `deny.toml` allows, and its `imgref`/`rgb`-based API would add two more dependencies for a
//! comparison this small a formula doesn't need.

use std::fs;
use std::path::{Path, PathBuf};

use image::RgbImage;
use serde::{Deserialize, Serialize};

/// Something that can turn a source file (a NEF, or a synthetic stand-in in tests) into a
/// rendered RGB image. The real implementation arrives with #41; test code implements this
/// trait directly against in-memory synthetic images.
pub trait Render {
    fn render(&self, source_path: &Path) -> anyhow::Result<RgbImage>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Meta {
    threshold: f64,
    source_sha256: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CompareOutcome {
    /// No golden existed yet (or `NICTI_BLESS` was set) -- this render was saved as the new
    /// golden rather than compared.
    Blessed,
    /// The rendered image matched the golden within `threshold`.
    Matched { score: f64 },
    /// The rendered image diverged from the golden past `threshold`.
    Diverged { score: f64, threshold: f64 },
}

impl CompareOutcome {
    pub fn passed(&self) -> bool {
        !matches!(self, CompareOutcome::Diverged { .. })
    }
}

/// One [`GoldenStore::compare`] request. Grouped into a struct rather than a long parameter list.
pub struct CompareRequest<'a> {
    pub render_name: &'a str,
    pub ref_id: &'a str,
    pub source_path: &'a Path,
    pub source_sha256: &'a str,
    pub threshold: f64,
    /// Overwrite (or create) the golden with this render's output instead of comparing.
    pub bless: bool,
}

/// Returns true if `NICTI_BLESS=1` is set in the environment -- the escape hatch for
/// intentionally rewriting goldens after a real rendering change.
pub fn bless_requested() -> bool {
    std::env::var("NICTI_BLESS")
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// A directory of golden PNGs plus their metadata, one subdirectory per render name (so the same
/// ref-10k id can have a separate golden for each pipeline stage/renderer under test).
pub struct GoldenStore {
    root: PathBuf,
}

impl GoldenStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        GoldenStore { root: root.into() }
    }

    fn image_path(&self, render_name: &str, ref_id: &str) -> PathBuf {
        self.root.join(render_name).join(format!("{ref_id}.png"))
    }

    fn meta_path(&self, render_name: &str, ref_id: &str) -> PathBuf {
        self.root
            .join(render_name)
            .join(format!("{ref_id}.meta.json"))
    }

    /// Renders `request.source_path` with `render`, then compares it against the stored golden
    /// for `(request.render_name, request.ref_id)`. Blesses (saves) a new golden if none exists
    /// yet or `request.bless` is true.
    pub fn compare<R: Render>(
        &self,
        render: &R,
        request: &CompareRequest,
    ) -> anyhow::Result<CompareOutcome> {
        let CompareRequest {
            render_name,
            ref_id,
            source_path,
            source_sha256,
            threshold,
            bless,
        } = *request;

        let rendered = render.render(source_path)?;
        let image_path = self.image_path(render_name, ref_id);

        if bless || !image_path.exists() {
            self.save(render_name, ref_id, &rendered, threshold, source_sha256)?;
            return Ok(CompareOutcome::Blessed);
        }

        let golden = image::open(&image_path)
            .map_err(|e| anyhow::anyhow!("reading golden {}: {e}", image_path.display()))?
            .to_rgb8();

        if golden.dimensions() != rendered.dimensions() {
            anyhow::bail!(
                "golden {} is {:?}, rendered image is {:?} -- dimensions must match",
                image_path.display(),
                golden.dimensions(),
                rendered.dimensions(),
            );
        }

        let score = ssim(&golden, &rendered);
        if score < threshold {
            Ok(CompareOutcome::Diverged { score, threshold })
        } else {
            Ok(CompareOutcome::Matched { score })
        }
    }

    fn save(
        &self,
        render_name: &str,
        ref_id: &str,
        image: &RgbImage,
        threshold: f64,
        source_sha256: &str,
    ) -> anyhow::Result<()> {
        let image_path = self.image_path(render_name, ref_id);
        if let Some(parent) = image_path.parent() {
            fs::create_dir_all(parent)?;
        }
        image
            .save(&image_path)
            .map_err(|e| anyhow::anyhow!("writing golden {}: {e}", image_path.display()))?;

        let meta = Meta {
            threshold,
            source_sha256: source_sha256.to_string(),
        };
        let meta_path = self.meta_path(render_name, ref_id);
        fs::write(&meta_path, serde_json::to_string_pretty(&meta)?)?;
        Ok(())
    }
}

/// Single-scale SSIM over 8x8 non-overlapping windows, standard Wang et al. constants for 8-bit
/// dynamic range, averaged across R/G/B channels. Returns a score in roughly `[-1.0, 1.0]`; 1.0
/// is identical. Panics if the two images differ in size -- callers (`GoldenStore::compare`)
/// check dimensions first.
///
/// Averaging over the three color channels (rather than luma alone, an earlier version of this
/// function's mistake, caught in review) matters specifically for this project: a color-space or
/// white-balance regression -- exactly the class of bug a RAW-pipeline golden-image harness
/// exists to catch -- can hold luma constant while shifting hue, which a luma-only comparison
/// would score as a perfect match. See `tests::ssim_detects_color_only_shift_luma_constant` for a
/// worked example.
fn ssim(a: &RgbImage, b: &RgbImage) -> f64 {
    assert_eq!(
        a.dimensions(),
        b.dimensions(),
        "ssim requires equal-sized images"
    );
    let scores: [f64; 3] = std::array::from_fn(|channel| ssim_channel(a, b, channel));
    scores.iter().sum::<f64>() / 3.0
}

fn ssim_channel(a: &RgbImage, b: &RgbImage, channel: usize) -> f64 {
    let (width, height) = a.dimensions();
    let luma_a = channel_values(a, channel);
    let luma_b = channel_values(b, channel);

    const WINDOW: u32 = 8;
    const L: f64 = 255.0;
    const C1: f64 = (0.01 * L) * (0.01 * L);
    const C2: f64 = (0.03 * L) * (0.03 * L);

    let mut total = 0.0;
    let mut windows = 0usize;

    let mut y = 0;
    while y < height {
        let win_h = WINDOW.min(height - y);
        let mut x = 0;
        while x < width {
            let win_w = WINDOW.min(width - x);
            let (mut sum_a, mut sum_b) = (0.0, 0.0);
            let n = (win_w * win_h) as f64;

            for wy in y..y + win_h {
                for wx in x..x + win_w {
                    let idx = (wy * width + wx) as usize;
                    sum_a += luma_a[idx];
                    sum_b += luma_b[idx];
                }
            }
            let mean_a = sum_a / n;
            let mean_b = sum_b / n;

            let (mut var_a, mut var_b, mut covar) = (0.0, 0.0, 0.0);
            for wy in y..y + win_h {
                for wx in x..x + win_w {
                    let idx = (wy * width + wx) as usize;
                    let da = luma_a[idx] - mean_a;
                    let db = luma_b[idx] - mean_b;
                    var_a += da * da;
                    var_b += db * db;
                    covar += da * db;
                }
            }
            var_a /= n;
            var_b /= n;
            covar /= n;

            let numerator = (2.0 * mean_a * mean_b + C1) * (2.0 * covar + C2);
            let denominator = (mean_a * mean_a + mean_b * mean_b + C1) * (var_a + var_b + C2);
            total += numerator / denominator;
            windows += 1;

            x += WINDOW;
        }
        y += WINDOW;
    }

    if windows == 0 {
        1.0
    } else {
        total / windows as f64
    }
}

fn channel_values(img: &RgbImage, channel: usize) -> Vec<f64> {
    img.pixels().map(|p| p[channel] as f64).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ConstantRender(RgbImage);

    impl Render for ConstantRender {
        fn render(&self, _source_path: &Path) -> anyhow::Result<RgbImage> {
            Ok(self.0.clone())
        }
    }

    fn checkerboard(width: u32, height: u32) -> RgbImage {
        RgbImage::from_fn(width, height, |x, y| {
            if (x / 4 + y / 4) % 2 == 0 {
                image::Rgb([230, 40, 40])
            } else {
                image::Rgb([20, 20, 200])
            }
        })
    }

    #[test]
    fn ssim_identical_images_scores_near_one() {
        let img = checkerboard(32, 32);
        let score = ssim(&img, &img);
        assert!(score > 0.999, "expected near-1.0, got {score}");
    }

    #[test]
    fn ssim_detects_color_only_shift_luma_constant() {
        // Regression test for a review-caught bug: an earlier version of `ssim` compared luma
        // only (0.299R + 0.587G + 0.114B), so two images that differ entirely in hue but happen
        // to share the same luma value scored ~1.0 ("identical") -- exactly the class of bug
        // (white-balance/color-profile regression) a RAW-pipeline golden-image harness exists to
        // catch. Solid red (255,0,0), luma=76.245; solid green (0,130,0), luma=76.31 -- chosen so
        // the two are luma-equal to within rounding, but obviously not the same color.
        let red = RgbImage::from_pixel(16, 16, image::Rgb([255, 0, 0]));
        let green = RgbImage::from_pixel(16, 16, image::Rgb([0, 130, 0]));
        let score = ssim(&red, &green);
        assert!(
            score < 0.5,
            "expected a low score for a color-only (luma-equal) shift, got {score}"
        );
    }

    #[test]
    fn ssim_detects_heavy_perturbation() {
        let a = checkerboard(32, 32);
        let mut b = a.clone();
        for p in b.pixels_mut() {
            p[0] = p[0].wrapping_add(120);
            p[1] = p[1].wrapping_add(120);
            p[2] = p[2].wrapping_add(120);
        }
        let score = ssim(&a, &b);
        assert!(
            score < 0.5,
            "expected a low score for a heavily perturbed image, got {score}"
        );
    }

    fn req<'a>(render_name: &'a str, ref_id: &'a str, bless: bool) -> CompareRequest<'a> {
        CompareRequest {
            render_name,
            ref_id,
            source_path: Path::new("unused.nef"),
            source_sha256: "deadbeef",
            threshold: 0.95,
            bless,
        }
    }

    #[test]
    fn compare_blesses_when_no_golden_exists() {
        let dir = tempfile::tempdir().unwrap();
        let store = GoldenStore::new(dir.path());
        let render = ConstantRender(checkerboard(16, 16));

        let outcome = store
            .compare(&render, &req("test-render", "ref-00001", false))
            .unwrap();
        assert_eq!(outcome, CompareOutcome::Blessed);
        assert!(dir.path().join("test-render/ref-00001.png").exists());
        assert!(dir.path().join("test-render/ref-00001.meta.json").exists());
    }

    #[test]
    fn compare_matches_identical_render_against_existing_golden() {
        let dir = tempfile::tempdir().unwrap();
        let store = GoldenStore::new(dir.path());
        let render = ConstantRender(checkerboard(16, 16));

        store.compare(&render, &req("r", "id", false)).unwrap();
        let outcome = store.compare(&render, &req("r", "id", false)).unwrap();
        assert!(matches!(outcome, CompareOutcome::Matched { .. }));
        assert!(outcome.passed());
    }

    #[test]
    fn compare_flags_divergence_past_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let store = GoldenStore::new(dir.path());
        let golden_render = ConstantRender(checkerboard(16, 16));
        store
            .compare(&golden_render, &req("r", "id", false))
            .unwrap();

        let mut perturbed = checkerboard(16, 16);
        for p in perturbed.pixels_mut() {
            p[0] = p[0].wrapping_add(120);
        }
        let bad_render = ConstantRender(perturbed);
        let outcome = store.compare(&bad_render, &req("r", "id", false)).unwrap();
        assert!(!outcome.passed());
        assert!(matches!(outcome, CompareOutcome::Diverged { .. }));
    }

    #[test]
    fn compare_rejects_dimension_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let store = GoldenStore::new(dir.path());
        let golden_render = ConstantRender(checkerboard(16, 16));
        store
            .compare(&golden_render, &req("r", "id", false))
            .unwrap();

        let wrong_size = ConstantRender(checkerboard(8, 8));
        let result = store.compare(&wrong_size, &req("r", "id", false));
        assert!(result.is_err());
    }

    #[test]
    fn compare_forced_bless_overwrites_existing_golden() {
        let dir = tempfile::tempdir().unwrap();
        let store = GoldenStore::new(dir.path());
        let first = ConstantRender(checkerboard(16, 16));
        store.compare(&first, &req("r", "id", false)).unwrap();

        let mut different = checkerboard(16, 16);
        for p in different.pixels_mut() {
            p[0] = p[0].wrapping_add(50);
        }
        let second = ConstantRender(different);
        let outcome = store.compare(&second, &req("r", "id", true)).unwrap();
        assert_eq!(outcome, CompareOutcome::Blessed);

        // Now comparing the first render again should diverge from the newly blessed golden.
        let outcome2 = store.compare(&first, &req("r", "id", false)).unwrap();
        assert!(!outcome2.passed());
    }
}
