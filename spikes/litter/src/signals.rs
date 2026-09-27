//! Candidate similarity signals, each a pure function over a pair of frames. `group.rs`'s
//! segmentation is signal-agnostic -- it just wants gap-in-seconds and a `[0,1]`-ish similarity
//! score per pair -- so every candidate here is measured under the identical grouping algorithm,
//! only the similarity function (and its own threshold) changes.

use crate::embed::{cosine_similarity, Dinov2Embedder};
use crate::nef::CaptureTime;
use image::RgbImage;
use image_hasher::{HashAlg, Hasher, HasherConfig, ImageHash};

/// One frame's data needed by every candidate signal. `serial` scopes time-based linking to the
/// same camera body, so an interleaved two-shooter event never cross-links frames from different
/// cameras purely because their timestamps happen to interleave.
#[derive(Clone)]
pub struct Frame {
    pub capture_time: CaptureTime,
    pub serial: Option<String>,
    pub t0: RgbImage,
}

pub fn gap_secs(frames: &[Frame], i: usize, j: usize) -> f64 {
    frames[i].capture_time.gap_seconds(&frames[j].capture_time)
}

/// `true` when `i` and `j` are from different, known camera serials -- a hard veto a caller can
/// check before consulting any visual-similarity signal, independent of the chosen candidate.
pub fn different_known_camera(frames: &[Frame], i: usize, j: usize) -> bool {
    match (&frames[i].serial, &frames[j].serial) {
        (Some(a), Some(b)) => a != b,
        _ => false,
    }
}

fn dhash_hasher() -> Hasher {
    HasherConfig::new().hash_alg(HashAlg::Gradient).to_hasher()
}

fn phash_hasher() -> Hasher {
    HasherConfig::new()
        .hash_alg(HashAlg::Mean)
        .preproc_dct()
        .to_hasher()
}

/// Precomputed per-frame data a candidate signal needs beyond the raw `Frame` -- computed once
/// per frame, not once per pair, since a con-day-scale eval compares each frame against a handful
/// of neighbors, not every other frame.
pub struct Fingerprint {
    pub dhash: ImageHash,
    pub phash: ImageHash,
    /// `None` when no DINOv2 embedder was configured for this run (the `time`/`dhash`/`phash`/
    /// `ssim` candidates don't need one).
    pub embedding: Option<Vec<f32>>,
}

pub fn fingerprint(
    frame: &Frame,
    embedder: Option<&Dinov2Embedder>,
) -> anyhow::Result<Fingerprint> {
    let dhasher = dhash_hasher();
    let phasher = phash_hasher();
    let dyn_img = image::DynamicImage::ImageRgb8(frame.t0.clone());
    let dhash = dhasher.hash_image(&dyn_img);
    let phash = phasher.hash_image(&dyn_img);
    let embedding = match embedder {
        Some(e) => Some(e.embed(&frame.t0)?),
        None => None,
    };
    Ok(Fingerprint {
        dhash,
        phash,
        embedding,
    })
}

/// Similarity in `[0,1]` from a hash's own bit count and Hamming distance -- `1.0` == identical.
fn hash_similarity(a: &ImageHash, b: &ImageHash) -> f64 {
    let bits = a.as_bytes().len() as f64 * 8.0;
    if bits == 0.0 {
        return 1.0;
    }
    1.0 - (a.dist(b) as f64 / bits)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Candidate {
    Dhash,
    Phash,
    Ssim,
    Dino,
}

impl Candidate {
    pub fn name(&self) -> &'static str {
        match self {
            Candidate::Dhash => "time+dhash",
            Candidate::Phash => "time+phash",
            Candidate::Ssim => "time+ssim",
            Candidate::Dino => "time+dino",
        }
    }

    /// Whether this candidate needs a `Dinov2Embedder` to have been configured for `fingerprint`.
    pub fn needs_embedding(&self) -> bool {
        matches!(self, Candidate::Dino)
    }

    pub fn similarity(&self, frames: &[Frame], fps: &[Fingerprint], i: usize, j: usize) -> f64 {
        match self {
            Candidate::Dhash => hash_similarity(&fps[i].dhash, &fps[j].dhash),
            Candidate::Phash => hash_similarity(&fps[i].phash, &fps[j].phash),
            Candidate::Ssim => ssim_similarity(&frames[i].t0, &frames[j].t0),
            Candidate::Dino => {
                let (Some(a), Some(b)) = (&fps[i].embedding, &fps[j].embedding) else {
                    return 0.0;
                };
                cosine_similarity(a, b)
            }
        }
    }

    pub const ALL: [Candidate; 4] = [
        Candidate::Dhash,
        Candidate::Phash,
        Candidate::Ssim,
        Candidate::Dino,
    ];
}

/// `nicti_prowl::golden::ssim` requires equal dimensions -- T0 previews from the same camera are
/// already the same size, but resize defensively (matching the smaller of the two) rather than
/// panic on a mixed-camera pair.
fn ssim_similarity(a: &RgbImage, b: &RgbImage) -> f64 {
    if a.dimensions() == b.dimensions() {
        return nicti_prowl::golden::ssim(a, b).clamp(-1.0, 1.0);
    }
    let (tw, th) = (a.width().min(b.width()), a.height().min(b.height()));
    let ra = image::imageops::resize(a, tw, th, image::imageops::FilterType::Triangle);
    let rb = image::imageops::resize(b, tw, th, image::imageops::FilterType::Triangle);
    nicti_prowl::golden::ssim(&ra, &rb).clamp(-1.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(color: [u8; 3]) -> RgbImage {
        RgbImage::from_pixel(32, 32, image::Rgb(color))
    }

    /// A gradient image (unlike `solid`, dHash needs adjacent-pixel *differences* to produce a
    /// non-trivial hash -- two uniform-color images always hash identically regardless of their
    /// absolute color, since a flat image has zero gradient everywhere).
    fn gradient(invert: bool) -> RgbImage {
        RgbImage::from_fn(32, 32, |x, _y| {
            let v = if invert {
                255 - (x * 8) as u8
            } else {
                (x * 8) as u8
            };
            image::Rgb([v, v, v])
        })
    }

    fn frame(t0: RgbImage, secs_offset: f64, serial: Option<&str>) -> Frame {
        Frame {
            capture_time: CaptureTime {
                year: 2026,
                month: 1,
                day: 1,
                hour: 0,
                minute: 0,
                second: secs_offset as u8,
                millis: 0,
            },
            serial: serial.map(str::to_string),
            t0,
        }
    }

    #[test]
    fn dhash_similarity_is_one_for_identical_images() {
        let frames = vec![
            frame(solid([100, 100, 100]), 0.0, None),
            frame(solid([100, 100, 100]), 1.0, None),
        ];
        let fps = vec![
            fingerprint(&frames[0], None).unwrap(),
            fingerprint(&frames[1], None).unwrap(),
        ];
        let sim = Candidate::Dhash.similarity(&frames, &fps, 0, 1);
        assert!((sim - 1.0).abs() < 1e-9);
    }

    #[test]
    fn dhash_similarity_is_lower_for_very_different_images() {
        let frames = vec![
            frame(gradient(false), 0.0, None),
            frame(gradient(true), 1.0, None),
        ];
        let fps = vec![
            fingerprint(&frames[0], None).unwrap(),
            fingerprint(&frames[1], None).unwrap(),
        ];
        let sim = Candidate::Dhash.similarity(&frames, &fps, 0, 1);
        assert!(
            sim < 1.0,
            "opposite gradients should not hash identically, got {sim}"
        );
    }

    #[test]
    fn ssim_similarity_is_one_for_identical_images() {
        let frames = vec![
            frame(solid([50, 60, 70]), 0.0, None),
            frame(solid([50, 60, 70]), 1.0, None),
        ];
        let fps = vec![
            fingerprint(&frames[0], None).unwrap(),
            fingerprint(&frames[1], None).unwrap(),
        ];
        let sim = Candidate::Ssim.similarity(&frames, &fps, 0, 1);
        assert!(sim > 0.99);
    }

    #[test]
    fn dino_similarity_is_zero_without_an_embedder() {
        let frames = vec![
            frame(solid([1, 2, 3]), 0.0, None),
            frame(solid([1, 2, 3]), 1.0, None),
        ];
        let fps = vec![
            fingerprint(&frames[0], None).unwrap(),
            fingerprint(&frames[1], None).unwrap(),
        ];
        assert_eq!(Candidate::Dino.similarity(&frames, &fps, 0, 1), 0.0);
    }

    #[test]
    fn different_known_camera_vetoes_cross_serial_pairs() {
        let frames = vec![
            frame(solid([1, 1, 1]), 0.0, Some("111")),
            frame(solid([1, 1, 1]), 0.1, Some("222")),
        ];
        assert!(different_known_camera(&frames, 0, 1));
    }

    #[test]
    fn same_or_unknown_serial_is_not_vetoed() {
        let frames = vec![
            frame(solid([1, 1, 1]), 0.0, Some("111")),
            frame(solid([1, 1, 1]), 0.1, Some("111")),
        ];
        assert!(!different_known_camera(&frames, 0, 1));
        let unknown = vec![
            frame(solid([1, 1, 1]), 0.0, None),
            frame(solid([1, 1, 1]), 0.1, None),
        ];
        assert!(!different_known_camera(&unknown, 0, 1));
    }
}
