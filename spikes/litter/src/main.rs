use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use litter::decode::decode_jpeg;
use litter::embed::Dinov2Embedder;
use litter::group::{group_sets, group_tight, LevelParams};
use litter::label::{read_labels, write_draft, DraftFrame};
use litter::metrics;
use litter::nef::{CaptureTime, NefReader};
use litter::signals::{
    different_known_camera, fingerprint, gap_secs, Candidate, Fingerprint, Frame,
};
use litter::source::FileSource;

#[derive(Parser)]
#[command(
    name = "litter",
    about = "burst/duplicate grouping spike (#33/ADR-0025)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Extracts NEF metadata + T0 previews from `nef_dir`, runs a default candidate, and writes
    /// a labelling contact sheet (`label.html`) plus `draft.json` into `work_dir`.
    Draft {
        nef_dir: PathBuf,
        #[arg(long)]
        work: PathBuf,
        #[arg(long, value_enum, default_value = "dhash")]
        candidate: Candidate,
        #[arg(long)]
        dino_onnx: Option<PathBuf>,
        #[arg(long)]
        ort_dylib: Option<PathBuf>,
    },
    /// Scores one or every candidate against human-corrected `labels.json`, over `nef_dir`.
    Eval {
        #[arg(long)]
        nef_dir: PathBuf,
        #[arg(long)]
        labels: PathBuf,
        #[arg(long, value_enum)]
        candidate: Option<Candidate>,
        /// Sweep a small (max_gap_secs, min_similarity) grid per level instead of one fixed
        /// operating point, reporting the best-F1 point found.
        #[arg(long)]
        sweep: bool,
        #[arg(long)]
        dino_onnx: Option<PathBuf>,
        #[arg(long)]
        ort_dylib: Option<PathBuf>,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Draft {
            nef_dir,
            work,
            candidate,
            dino_onnx,
            ort_dylib,
        } => draft(&nef_dir, &work, candidate, dino_onnx, ort_dylib),
        Command::Eval {
            nef_dir,
            labels,
            candidate,
            sweep,
            dino_onnx,
            ort_dylib,
        } => eval(&nef_dir, &labels, candidate, sweep, dino_onnx, ort_dylib),
    }
}

struct LoadedFrame {
    filename: String,
    sha256: String,
    frame: Frame,
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

/// Reads every `.NEF` under `dir`, decodes its T0 preview, and sorts by capture time. Files with
/// no `DateTimeOriginal` or no Nikon PreviewIFD are skipped with a warning printed to stderr --
/// this is a research CLI, not a production ingest path, so a bad file shouldn't abort the run.
fn load_frames(dir: &Path) -> anyhow::Result<Vec<LoadedFrame>> {
    let mut out = Vec::new();
    for path in list_nef_files(dir)? {
        let filename = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        let meta = match FileSource::open(&path).and_then(|s| {
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
        let (meta, mut reader) = meta;
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

        out.push(LoadedFrame {
            filename,
            sha256,
            frame: Frame {
                capture_time: meta.capture_time,
                serial: meta.serial,
                t0,
            },
        });
    }
    out.sort_by_key(|a| a.frame.capture_time);
    Ok(out)
}

const DEFAULT_TIGHT: LevelParams = LevelParams {
    max_gap_secs: 10.0,
    min_similarity: 0.90,
    max_lookahead: 2,
};
const DEFAULT_SET: LevelParams = LevelParams {
    max_gap_secs: 30.0,
    min_similarity: 0.75,
    max_lookahead: 2,
};

fn build_fingerprints(
    frames: &[LoadedFrame],
    candidate: Candidate,
    dino_onnx: &Option<PathBuf>,
    ort_dylib: &Option<PathBuf>,
) -> anyhow::Result<Vec<Fingerprint>> {
    let embedder = if candidate.needs_embedding() {
        let (Some(onnx), Some(dylib)) = (dino_onnx, ort_dylib) else {
            anyhow::bail!("--candidate dino requires --dino-onnx and --ort-dylib");
        };
        Some(Dinov2Embedder::new(onnx.clone(), dylib.clone()))
    } else {
        None
    };
    frames
        .iter()
        .map(|f| fingerprint(&f.frame, embedder.as_ref()))
        .collect()
}

fn to_frames(frames: &[LoadedFrame]) -> Vec<Frame> {
    frames.iter().map(|f| f.frame.clone()).collect()
}

fn run_grouping(
    raw: &[Frame],
    fps: &[Fingerprint],
    candidate: Candidate,
    tight_params: LevelParams,
    set_params: LevelParams,
) -> (Vec<usize>, Vec<usize>) {
    let gap = |i: usize, j: usize| gap_secs(raw, i, j);
    let sim = |i: usize, j: usize| {
        if different_known_camera(raw, i, j) {
            0.0
        } else {
            candidate.similarity(raw, fps, i, j)
        }
    };
    let tight = group_tight(raw.len(), gap, sim, tight_params);
    let sets = group_sets(&tight, gap, sim, set_params);
    (tight, sets)
}

fn format_capture_time(t: &CaptureTime) -> String {
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        t.year, t.month, t.day, t.hour, t.minute, t.second, t.millis
    )
}

fn draft(
    nef_dir: &Path,
    work: &Path,
    candidate: Candidate,
    dino_onnx: Option<PathBuf>,
    ort_dylib: Option<PathBuf>,
) -> anyhow::Result<()> {
    let frames = load_frames(nef_dir)?;
    if frames.is_empty() {
        anyhow::bail!("no usable NEF files found in {}", nef_dir.display());
    }
    let fps = build_fingerprints(&frames, candidate, &dino_onnx, &ort_dylib)?;
    let raw = to_frames(&frames);
    let (tight, sets) = run_grouping(&raw, &fps, candidate, DEFAULT_TIGHT, DEFAULT_SET);

    let thumbs_dir = work.join("thumbs");
    fs::create_dir_all(&thumbs_dir)?;

    let mut draft_frames = Vec::with_capacity(frames.len());
    for (i, f) in frames.iter().enumerate() {
        let thumb_name = format!("{i:04}.jpg");
        f.frame
            .t0
            .save(thumbs_dir.join(&thumb_name))
            .map_err(|e| anyhow::anyhow!("writing thumbnail: {e}"))?;
        let gap_before = if i == 0 {
            0.0
        } else {
            raw[i - 1].capture_time.gap_seconds(&raw[i].capture_time)
        };
        draft_frames.push(DraftFrame {
            index: i,
            filename: f.filename.clone(),
            sha256: f.sha256.clone(),
            capture_time: format_capture_time(&f.frame.capture_time),
            serial: f.frame.serial.clone(),
            gap_before_secs: gap_before,
            tight_group: tight[i],
            set_group: sets[i],
            thumb: format!("thumbs/{thumb_name}"),
        });
    }
    write_draft(work, &draft_frames)?;

    println!(
        "{} frames -> {} tight groups, {} sets ({} candidate). Wrote {}",
        frames.len(),
        tight.last().map(|&g| g + 1).unwrap_or(0),
        sets.last().map(|&g| g + 1).unwrap_or(0),
        candidate.name(),
        work.join("label.html").display(),
    );
    Ok(())
}

fn eval(
    nef_dir: &Path,
    labels_path: &Path,
    candidate: Option<Candidate>,
    sweep: bool,
    dino_onnx: Option<PathBuf>,
    ort_dylib: Option<PathBuf>,
) -> anyhow::Result<()> {
    let frames = load_frames(nef_dir)?;
    let labels = read_labels(labels_path)?;
    let by_filename: HashMap<&str, &litter::label::LabelRow> =
        labels.iter().map(|l| (l.filename.as_str(), l)).collect();

    let mut truth_tight = Vec::with_capacity(frames.len());
    let mut truth_set = Vec::with_capacity(frames.len());
    for f in &frames {
        let row = by_filename.get(f.filename.as_str()).ok_or_else(|| {
            anyhow::anyhow!(
                "no label row for {} -- labels.json is out of date with nef_dir",
                f.filename
            )
        })?;
        truth_tight.push(row.tight_id);
        truth_set.push(row.set_id);
    }

    let candidates: Vec<Candidate> = match candidate {
        Some(c) => vec![c],
        None => Candidate::ALL.to_vec(),
    };

    let grid_gap = [5.0, 10.0, 20.0, 30.0];
    let grid_sim = [0.70, 0.80, 0.90, 0.95];

    println!(
        "{:<12} {:>8} {:>8} {:>8} {:>10} {:>10} {:>10} {:>8} {:>8} {:>10}",
        "candidate",
        "level",
        "gap_s",
        "min_sim",
        "precision",
        "recall",
        "f1",
        "ari",
        "over",
        "under"
    );

    for c in candidates {
        if c.needs_embedding() && (dino_onnx.is_none() || ort_dylib.is_none()) {
            eprintln!("skipping {}: needs --dino-onnx and --ort-dylib", c.name());
            continue;
        }
        let fps = match build_fingerprints(&frames, c, &dino_onnx, &ort_dylib) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("skipping {}: {e}", c.name());
                continue;
            }
        };

        let tight_grid: Vec<LevelParams> = if sweep {
            grid_gap
                .iter()
                .flat_map(|&g| {
                    grid_sim.iter().map(move |&s| LevelParams {
                        max_gap_secs: g,
                        min_similarity: s,
                        max_lookahead: 2,
                    })
                })
                .collect()
        } else {
            vec![DEFAULT_TIGHT]
        };

        let raw = to_frames(&frames);
        let gap = |i: usize, j: usize| gap_secs(&raw, i, j);
        let sim = |i: usize, j: usize| {
            if different_known_camera(&raw, i, j) {
                0.0
            } else {
                c.similarity(&raw, &fps, i, j)
            }
        };

        let mut best_tight: Option<(LevelParams, Vec<usize>, metrics::Scores)> = None;
        for params in tight_grid {
            let tight = group_tight(frames.len(), gap, sim, params);
            let scores = metrics::score(&tight, &truth_tight);
            if best_tight
                .as_ref()
                .map(|(_, _, s)| scores.bcubed_f1 > s.bcubed_f1)
                .unwrap_or(true)
            {
                best_tight = Some((params, tight, scores));
            }
        }
        let (tight_params, tight_groups, tight_scores) = best_tight.unwrap();
        print_row(c.name(), "tight", &tight_params, &tight_scores);

        let set_grid: Vec<LevelParams> = if sweep {
            grid_gap
                .iter()
                .flat_map(|&g| {
                    grid_sim.iter().map(move |&s| LevelParams {
                        max_gap_secs: g,
                        min_similarity: s,
                        max_lookahead: 2,
                    })
                })
                .collect()
        } else {
            vec![DEFAULT_SET]
        };

        let mut best_set: Option<(LevelParams, metrics::Scores)> = None;
        for params in set_grid {
            let sets = group_sets(&tight_groups, gap, sim, params);
            let scores = metrics::score(&sets, &truth_set);
            if best_set
                .as_ref()
                .map(|(_, s)| scores.bcubed_f1 > s.bcubed_f1)
                .unwrap_or(true)
            {
                best_set = Some((params, scores));
            }
        }
        let (set_params, set_scores) = best_set.unwrap();
        print_row(c.name(), "set", &set_params, &set_scores);
    }

    Ok(())
}

fn print_row(name: &str, level: &str, params: &LevelParams, scores: &metrics::Scores) {
    println!(
        "{:<12} {:>8} {:>8.1} {:>8.2} {:>10.4} {:>10.4} {:>10.4} {:>8.4} {:>8} {:>10}",
        name,
        level,
        params.max_gap_secs,
        params.min_similarity,
        scores.bcubed_precision,
        scores.bcubed_recall,
        scores.bcubed_f1,
        scores.adjusted_rand_index,
        scores.over_merge_pairs,
        scores.under_merge_pairs,
    );
}
