use std::fs;
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand, ValueEnum};
use rosette::cluster::dbscan_with_eps_sweep;
use rosette::crop::{bbox_from_alpha, crop_image, pad_bbox, Alpha};
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
    let padded = pad_bbox(bbox, 0.2, w as usize, h as usize);
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
