use std::fs;
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand, ValueEnum};
use rosette::cluster::dbscan_with_eps_sweep;
use rosette::crop::{bbox_from_alpha, crop_image, pad_bbox, Alpha, BBox};
use rosette::decode::decode_jpeg;
use rosette::embed::{cosine_distance, Dinov2Embedder, Dinov3Embedder, Embedder, OpenClipEmbedder};
use rosette::label::{read_labels, write_draft, DraftPhoto};
use rosette::metrics;
use rosette::nef::NefReader;
use rosette::source::FileSource;

#[derive(Parser)]
#[command(name = "rosette", about = "face/subject grouping spike (#35/ADR-0035)")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Backbone {
    Dinov2,
    Dinov3,
    Openclip,
}

#[derive(Subcommand)]
enum Command {
    /// Extracts NEF T0 previews from `nef_dir`, embeds + clusters them, and writes a labelling
    /// contact sheet (`label.html`) plus `draft.json` into `work_dir`.
    Draft {
        nef_dir: PathBuf,
        #[arg(long)]
        work: PathBuf,
        #[arg(long, value_enum, default_value = "dinov2")]
        backbone: Backbone,
        /// Ablate: embed a padded subject-crop instead of the full frame. Needs a precomputed
        /// alpha mask directory (see this module's doc comment) -- without one, full-frame is
        /// used regardless of this flag, with a warning.
        #[arg(long)]
        crop: bool,
        #[arg(long)]
        model_onnx: Option<PathBuf>,
        #[arg(long)]
        ort_dylib: Option<PathBuf>,
        /// OpenCLIP's own output width -- varies by checkpoint, unlike the two DINO variants.
        #[arg(long, default_value_t = 512)]
        openclip_dim: usize,
        #[arg(long, default_value_t = 2)]
        min_samples: usize,
    },
    /// Scores a candidate's clustering against human-corrected `labels.json`.
    Eval {
        #[arg(long)]
        nef_dir: PathBuf,
        #[arg(long)]
        labels: PathBuf,
        #[arg(long, value_enum, default_value = "dinov2")]
        backbone: Backbone,
        #[arg(long)]
        model_onnx: Option<PathBuf>,
        #[arg(long)]
        ort_dylib: Option<PathBuf>,
        #[arg(long, default_value_t = 512)]
        openclip_dim: usize,
        #[arg(long, default_value_t = 2)]
        min_samples: usize,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Draft {
            nef_dir,
            work,
            backbone,
            crop,
            model_onnx,
            ort_dylib,
            openclip_dim,
            min_samples,
        } => draft(
            &nef_dir,
            &work,
            backbone,
            crop,
            model_onnx,
            ort_dylib,
            openclip_dim,
            min_samples,
        ),
        Command::Eval {
            nef_dir,
            labels,
            backbone,
            model_onnx,
            ort_dylib,
            openclip_dim,
            min_samples,
        } => eval(
            &nef_dir,
            &labels,
            backbone,
            model_onnx,
            ort_dylib,
            openclip_dim,
            min_samples,
        ),
    }
}

struct LoadedPhoto {
    filename: String,
    sha256: String,
    t0: image::RgbImage,
}

fn list_nef_files(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("nef"))
                .unwrap_or(false)
        })
        .collect();
    files.sort();
    Ok(files)
}

fn sha256_file(path: &Path) -> anyhow::Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    Ok(digest.iter().map(|b| format!("{b:02x}")).collect())
}

/// Reads every `.NEF` under `dir` and decodes its T0 preview -- same skip-bad-file-with-a-warning
/// posture as `spikes/litter/src/main.rs::load_frames` (a research CLI, not a production ingest
/// path). Sorted by filename (subject grouping has no sequence dependency, unlike litter's
/// capture-time sort).
fn load_photos(dir: &Path) -> anyhow::Result<Vec<LoadedPhoto>> {
    let mut out = Vec::new();
    for path in list_nef_files(dir)? {
        let filename = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        let opened = match FileSource::open(&path).and_then(|s| {
            let mut reader = NefReader::new(s)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
            reader
                .read_meta()
                .map(|m| (m, reader))
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))
        }) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("skipping {filename}: {e}");
                continue;
            }
        };
        let (meta, mut reader) = opened;
        let Some(preview) = meta.preview.clone() else {
            eprintln!("skipping {filename}: no Nikon PreviewIFD (T0) found");
            continue;
        };
        let jpeg_bytes = reader.read_range(preview.file_offset, preview.byte_len as usize)?;
        let t0 = match decode_jpeg(&jpeg_bytes) {
            Ok(img) => img,
            Err(e) => {
                eprintln!("skipping {filename}: T0 decode failed: {e}");
                continue;
            }
        };
        let sha256 = sha256_file(&path)?;
        out.push(LoadedPhoto {
            filename,
            sha256,
            t0,
        });
    }
    out.sort_by(|a, b| a.filename.cmp(&b.filename));
    Ok(out)
}

fn build_embedder(
    backbone: Backbone,
    model_onnx: &Option<PathBuf>,
    ort_dylib: &Option<PathBuf>,
    openclip_dim: usize,
) -> anyhow::Result<Box<dyn Embedder>> {
    let (Some(onnx), Some(dylib)) = (model_onnx, ort_dylib) else {
        anyhow::bail!("--model-onnx and --ort-dylib are required");
    };
    Ok(match backbone {
        Backbone::Dinov2 => Box::new(Dinov2Embedder::new(onnx.clone(), dylib.clone())),
        Backbone::Dinov3 => Box::new(Dinov3Embedder::new(onnx.clone(), dylib.clone())),
        Backbone::Openclip => Box::new(OpenClipEmbedder::new(
            onnx.clone(),
            dylib.clone(),
            openclip_dim,
        )),
    })
}

/// Loads a precomputed alpha mask for `filename` from `<nef_dir>/masks/<stem>.mask.json` (a
/// `crop::Alpha` serialized as `{width, height, data}`) -- no BiRefNet inference is wired into
/// this CLI directly (no real weights available this pass, see `crop.rs`'s doc comment), so the
/// crop ablation is exercised against externally-supplied masks. Returns `None` (full-frame
/// fallback) if no mask file exists, rather than erroring -- this keeps `--crop` safe to pass even
/// when no masks were precomputed.
fn load_precomputed_mask(nef_dir: &Path, filename: &str) -> Option<Alpha> {
    let stem = Path::new(filename)
        .file_stem()?
        .to_string_lossy()
        .to_string();
    let mask_path = nef_dir.join("masks").join(format!("{stem}.mask.json"));
    let contents = fs::read_to_string(mask_path).ok()?;
    #[derive(serde::Deserialize)]
    struct RawAlpha {
        width: usize,
        height: usize,
        data: Vec<f32>,
    }
    let raw: RawAlpha = serde_json::from_str(&contents).ok()?;
    Some(Alpha {
        width: raw.width,
        height: raw.height,
        data: raw.data,
    })
}

/// Scales a bbox found in a mask's own coordinate space into the T0 image's coordinate space --
/// a precomputed mask (e.g. from a segmentation model run at a fixed input resolution) has no
/// reason to share the T0 preview's own dimensions. Caught by CodeRabbit: applying mask-space
/// coordinates directly as image-space coordinates (the original code) silently produces a wrong
/// -- possibly empty -- crop whenever the two resolutions differ, rather than an error or panic
/// (`image::imageops::crop_imm` clamps out-of-bounds requests rather than panicking, which is
/// exactly what let this go unnoticed). Returns `None` if the scaled bbox is empty/inverted, so
/// the caller falls back to full-frame instead of cropping to nothing.
fn scale_bbox_to_image(
    bbox: BBox,
    mask_dims: (usize, usize),
    img_dims: (usize, usize),
) -> Option<BBox> {
    let (mask_w, mask_h) = mask_dims;
    let (img_w, img_h) = img_dims;
    if mask_w == 0 || mask_h == 0 {
        return None;
    }
    let sx = img_w as f64 / mask_w as f64;
    let sy = img_h as f64 / mask_h as f64;
    let x0 = ((bbox.x0 as f64 * sx).floor() as usize).min(img_w);
    let y0 = ((bbox.y0 as f64 * sy).floor() as usize).min(img_h);
    let x1 = ((bbox.x1 as f64 * sx).ceil() as usize).min(img_w);
    let y1 = ((bbox.y1 as f64 * sy).ceil() as usize).min(img_h);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some(BBox { x0, y0, x1, y1 })
}

fn maybe_crop(
    img: &image::RgbImage,
    nef_dir: &Path,
    filename: &str,
    do_crop: bool,
) -> image::RgbImage {
    if !do_crop {
        return img.clone();
    }
    let Some(alpha) = load_precomputed_mask(nef_dir, filename) else {
        eprintln!("--crop requested but no precomputed mask for {filename}; using full frame");
        return img.clone();
    };
    let Some(bbox) = bbox_from_alpha(&alpha, 0.5) else {
        return img.clone();
    };
    let (w, h) = img.dimensions();
    let Some(scaled) =
        scale_bbox_to_image(bbox, (alpha.width, alpha.height), (w as usize, h as usize))
    else {
        eprintln!(
            "--crop: mask for {filename} scaled to an empty/inverted bbox in image space; using full frame"
        );
        return img.clone();
    };
    let padded = pad_bbox(scaled, 0.2, w as usize, h as usize);
    crop_image(img, padded)
}

fn embed_all(
    photos: &[LoadedPhoto],
    embedder: &dyn Embedder,
    nef_dir: &Path,
    do_crop: bool,
) -> anyhow::Result<Vec<Vec<f32>>> {
    photos
        .iter()
        .map(|p| {
            let cropped = maybe_crop(&p.t0, nef_dir, &p.filename, do_crop);
            embedder
                .embed(&cropped)
                .map_err(|e| anyhow::anyhow!("{}: {e}", p.filename))
        })
        .collect()
}

fn eps_candidates() -> Vec<f64> {
    // Cosine distance in [0.0, 2.0]; a fairly fine sweep across the useful range.
    (1..=40).map(|i| i as f64 * 0.05).collect()
}

#[allow(clippy::too_many_arguments)]
fn draft(
    nef_dir: &Path,
    work: &Path,
    backbone: Backbone,
    do_crop: bool,
    model_onnx: Option<PathBuf>,
    ort_dylib: Option<PathBuf>,
    openclip_dim: usize,
    min_samples: usize,
) -> anyhow::Result<()> {
    let photos = load_photos(nef_dir)?;
    if photos.is_empty() {
        anyhow::bail!("no usable NEF files found in {}", nef_dir.display());
    }
    let embedder = build_embedder(backbone, &model_onnx, &ort_dylib, openclip_dim)?;
    let embeddings = embed_all(&photos, embedder.as_ref(), nef_dir, do_crop)?;

    let dist = |i: usize, j: usize| cosine_distance(&embeddings[i], &embeddings[j]);
    let (assignments, chosen_eps) =
        dbscan_with_eps_sweep(photos.len(), dist, min_samples, &eps_candidates());

    let thumbs_dir = work.join("thumbs");
    fs::create_dir_all(&thumbs_dir)?;
    let mut draft_photos = Vec::with_capacity(photos.len());
    for (i, p) in photos.iter().enumerate() {
        let thumb_name = format!("{i:04}.jpg");
        p.t0.save(thumbs_dir.join(&thumb_name))
            .map_err(|e| anyhow::anyhow!("writing thumbnail: {e}"))?;
        draft_photos.push(DraftPhoto {
            index: i,
            filename: p.filename.clone(),
            sha256: p.sha256.clone(),
            cluster: assignments[i],
            thumb: format!("thumbs/{thumb_name}"),
        });
    }
    write_draft(work, &draft_photos)?;

    let n_clusters = assignments
        .iter()
        .filter_map(|a| *a)
        .max()
        .map(|m| m + 1)
        .unwrap_or(0);
    let n_noise = assignments.iter().filter(|a| a.is_none()).count();
    println!(
        "{} photos -> {} clusters, {} noise ({} backbone, crop={do_crop}, eps={chosen_eps:.2}). Wrote {}",
        photos.len(),
        n_clusters,
        n_noise,
        embedder.name(),
        work.join("label.html").display(),
    );
    Ok(())
}

fn eval(
    nef_dir: &Path,
    labels_path: &Path,
    backbone: Backbone,
    model_onnx: Option<PathBuf>,
    ort_dylib: Option<PathBuf>,
    openclip_dim: usize,
    min_samples: usize,
) -> anyhow::Result<()> {
    let photos = load_photos(nef_dir)?;
    let labels = read_labels(labels_path)?;
    let by_filename: std::collections::HashMap<&str, usize> = labels
        .iter()
        .map(|l| (l.filename.as_str(), l.subject_id))
        .collect();

    let mut truth = Vec::with_capacity(photos.len());
    for p in &photos {
        let subject_id = *by_filename.get(p.filename.as_str()).ok_or_else(|| {
            anyhow::anyhow!(
                "no label row for {} -- labels.json is out of date with nef_dir",
                p.filename
            )
        })?;
        truth.push(subject_id);
    }

    let embedder = build_embedder(backbone, &model_onnx, &ort_dylib, openclip_dim)?;

    for do_crop in [false, true] {
        let embeddings = embed_all(&photos, embedder.as_ref(), nef_dir, do_crop)?;
        let dist = |i: usize, j: usize| cosine_distance(&embeddings[i], &embeddings[j]);
        let (assignments, chosen_eps) =
            dbscan_with_eps_sweep(photos.len(), dist, min_samples, &eps_candidates());
        // Noise (None) scored as its own singleton-per-photo group id, distinct from every real
        // cluster id and from every other noise point -- an unfair "everyone agrees this is
        // unclustered" score would otherwise inflate precision.
        let mut next_singleton = assignments
            .iter()
            .filter_map(|a| *a)
            .max()
            .map(|m| m + 1)
            .unwrap_or(0);
        let predicted: Vec<usize> = assignments
            .iter()
            .map(|a| match a {
                Some(c) => *c,
                None => {
                    let id = next_singleton;
                    next_singleton += 1;
                    id
                }
            })
            .collect();
        let scores = metrics::score(&predicted, &truth);
        println!(
            "{:<10} crop={:<5} eps={:>6.2} precision={:.4} recall={:.4} f1={:.4} ari={:.4} over={} under={}",
            embedder.name(),
            do_crop,
            chosen_eps,
            scores.bcubed_precision,
            scores.bcubed_recall,
            scores.bcubed_f1,
            scores.adjusted_rand_index,
            scores.over_merge_pairs,
            scores.under_merge_pairs,
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_bbox_to_image_scales_up_from_a_smaller_mask() {
        // Mask computed at half the T0 preview's resolution -- a bbox spanning [10,10)-(30,30) in
        // mask space should double into [20,20)-(60,60) in image space.
        let bbox = BBox {
            x0: 10,
            y0: 10,
            x1: 30,
            y1: 30,
        };
        let scaled = scale_bbox_to_image(bbox, (100, 100), (200, 200)).expect("valid bbox");
        assert_eq!(
            scaled,
            BBox {
                x0: 20,
                y0: 20,
                x1: 60,
                y1: 60
            }
        );
    }

    #[test]
    fn scale_bbox_to_image_scales_down_from_a_larger_mask() {
        // Regression test for the CodeRabbit-caught bug: mask computed at a fixed model
        // resolution (e.g. 1024x1024) much larger than a small T0 preview -- coordinates must be
        // scaled down, not applied directly (which would silently clamp to a meaningless region).
        let bbox = BBox {
            x0: 512,
            y0: 512,
            x1: 768,
            y1: 768,
        };
        let scaled = scale_bbox_to_image(bbox, (1024, 1024), (160, 120)).expect("valid bbox");
        assert_eq!(
            scaled,
            BBox {
                x0: 80,
                y0: 60,
                x1: 120,
                y1: 90
            }
        );
    }

    #[test]
    fn scale_bbox_to_image_identity_when_dimensions_match() {
        let bbox = BBox {
            x0: 5,
            y0: 5,
            x1: 15,
            y1: 15,
        };
        let scaled = scale_bbox_to_image(bbox, (50, 50), (50, 50)).expect("valid bbox");
        assert_eq!(scaled, bbox);
    }

    #[test]
    fn scale_bbox_to_image_returns_none_for_zero_mask_dimensions() {
        let bbox = BBox {
            x0: 0,
            y0: 0,
            x1: 10,
            y1: 10,
        };
        assert!(scale_bbox_to_image(bbox, (0, 0), (100, 100)).is_none());
    }

    #[test]
    fn scale_bbox_to_image_returns_none_when_image_has_a_zero_dimension() {
        // A T0 image with a zero width/height (a degenerate decode) forces the scale factor to
        // 0.0 on that axis, collapsing any bbox to zero width/height in image space -- must
        // return None (full-frame fallback upstream), not an empty-but-`Some` bbox.
        let bbox = BBox {
            x0: 10,
            y0: 10,
            x1: 20,
            y1: 20,
        };
        assert!(scale_bbox_to_image(bbox, (100, 100), (0, 50)).is_none());
    }
}
