//! CLI for `spikes/squint` (#34/ADR-0034). Three subcommands:
//! - `synthetic`: runs the synthetic-degradation + keeper-false-flag measurement pass
//!   (`eval::run`) against a directory of already-decoded keeper images (JPEG/PNG/TIFF -- anything
//!   `image` opens), reporting per-candidate detection rates and ms/frame.
//! - `draft`: extracts each NEF's mid-size preview, computes every *global* sharpness candidate's
//!   score (the AF-region-aware candidate is deliberately not included here -- `af::AfArea`'s
//!   `AFInfo2` parsing is unverified against a real file, see `af.rs`'s own doc comment, so its
//!   score doesn't belong in a labelling page presented as trustworthy), and writes a local
//!   `label.html` contact sheet for real human labelling.
//! - `labels`: reads a `draft.json`/`labels.json` pair back and reports each candidate's score
//!   distribution grouped by real human tag -- a diagnostic, not a hard precision/recall verdict,
//!   since a real labelled con card (this pass's actual gap, see ADR-0034's Context) is what would
//!   make that verdict meaningful.

use std::fs;
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use nicti_prowl::perf::Protocol;
use serde::Serialize;

use squint::af::AfReader;
use squint::decode::decode_jpeg;
use squint::eval::{self, candidates};
use squint::label::{self, DraftFrame, LabelRow};
use squint::sharp::GrayFrame;
use squint::source::FileSource;

#[derive(Parser)]
#[command(name = "squint")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Synthetic-degradation + keeper-false-flag measurement pass.
    Synthetic {
        /// Directory of real, already-kept images (any format `image` opens).
        #[arg(long)]
        keepers_dir: PathBuf,
    },
    /// Extract previews from a NEF directory and write a local labelling contact sheet.
    Draft {
        #[arg(long)]
        nef_dir: PathBuf,
        #[arg(long)]
        work: PathBuf,
    },
    /// Report each candidate's score distribution grouped by a `labels.json`'s real human tags.
    Labels {
        /// The `draft`-produced work directory (holds `draft.json`).
        #[arg(long)]
        work: PathBuf,
        /// The exported `labels.json` (from `label.html`'s Export button).
        #[arg(long)]
        labels: PathBuf,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Synthetic { keepers_dir } => run_synthetic(&keepers_dir),
        Command::Draft { nef_dir, work } => run_draft(&nef_dir, &work),
        Command::Labels { work, labels } => run_labels(&work, &labels),
    }
}

#[derive(Debug, Serialize)]
struct SyntheticOutput {
    reports: Vec<eval::CandidateReport>,
    ms_per_frame: Vec<(String, f64)>,
}

fn run_synthetic(keepers_dir: &Path) -> anyhow::Result<()> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(keepers_dir)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            paths.push(entry.path());
        }
    }
    if paths.is_empty() {
        anyhow::bail!("no files found in {}", keepers_dir.display());
    }
    let keepers = eval::load_keepers(&paths)?;
    let reports = eval::run(&keepers);

    let protocol = Protocol {
        warmup: 1,
        measured: 5,
    };
    let mut ms_per_frame = Vec::new();
    for (name, score_fn) in candidates() {
        let frame = GrayFrame::from_rgb(&keepers[0]);
        let stats = protocol.run(|| {
            let _ = score_fn(&frame);
        });
        ms_per_frame.push((name.to_string(), stats.p50_ms));
    }

    let output = SyntheticOutput {
        reports,
        ms_per_frame,
    };
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

fn run_draft(nef_dir: &Path, work: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(work)?;
    let thumbs_dir = work.join("thumbs");
    fs::create_dir_all(&thumbs_dir)?;

    let mut nef_paths: Vec<PathBuf> = fs::read_dir(nef_dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("nef"))
                .unwrap_or(false)
        })
        .collect();
    nef_paths.sort();

    let mut frames = Vec::new();
    for (i, path) in nef_paths.iter().enumerate() {
        let source = FileSource::open(path)?;
        let mut reader = match AfReader::new(source) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skipping {}: {e}", path.display());
                continue;
            }
        };
        let meta = match reader.read_meta() {
            Ok(m) => m,
            Err(e) => {
                eprintln!("skipping {}: {e}", path.display());
                continue;
            }
        };
        let Some(preview) = meta.mid_preview.clone().or(meta.full_res_preview.clone()) else {
            eprintln!("skipping {}: no embedded preview found", path.display());
            continue;
        };
        let bytes = reader.read_range(preview.file_offset, preview.byte_len as usize)?;
        let img = match decode_jpeg(&bytes) {
            Ok(img) => img,
            Err(e) => {
                eprintln!("skipping {}: decode failed: {e}", path.display());
                continue;
            }
        };

        let thumb_name = format!("{i:04}.jpg");
        img.save(thumbs_dir.join(&thumb_name))?;

        let gray = GrayFrame::from_rgb(&img);
        let candidate_scores = candidates()
            .into_iter()
            .map(|(name, score_fn)| (name.to_string(), score_fn(&gray)))
            .collect();

        let filename = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let sha256 = format!("{:x}", blake3_ish_placeholder(&bytes));

        frames.push(DraftFrame {
            index: i,
            filename,
            sha256,
            thumb: format!("thumbs/{thumb_name}"),
            candidate_scores,
        });
    }

    label::write_draft(work, &frames)?;
    println!(
        "wrote {} frames to {}",
        frames.len(),
        work.join("label.html").display()
    );
    Ok(())
}

/// A cheap, non-cryptographic content fingerprint for `draft.json`'s `sha256` field -- this spike
/// doesn't pull in a hashing crate of its own for identity purposes (unlike `litter`, which needs
/// real content identity for its labelling round-trip); this is just enough to spot an obviously
/// stale/mismatched thumbnail during manual review. **Not a real SHA-256** despite the field name
/// (kept for `label.rs`/`DraftFrame` schema parity with `litter`'s own draft format) -- a real
/// content hash is a one-line swap to `sha2`/`blake3` if this spike is ever promoted.
fn blake3_ish_placeholder(bytes: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

fn run_labels(work: &Path, labels_path: &Path) -> anyhow::Result<()> {
    let draft_json = fs::read_to_string(work.join("draft.json"))?;
    let frames: Vec<DraftFrame> = serde_json::from_str(&draft_json)?;
    let labels: Vec<LabelRow> = label::read_labels(labels_path)?;

    let mut by_filename = std::collections::HashMap::new();
    for l in &labels {
        by_filename.insert(l.filename.clone(), l);
    }

    // A `labels.json` matched by filename alone can silently misapply: if the NEF directory was
    // re-populated (different photos, filenames reused/renumbered) since the draft that produced
    // this labels.json was exported, a same-named-but-different frame would get the old file's
    // tags with no error. Cross-check the content identifier before trusting a match.
    for frame in &frames {
        if let Some(label) = by_filename.get(&frame.filename) {
            if label.sha256 != frame.sha256 {
                anyhow::bail!(
                    "label row for {} doesn't match this draft's frame (sha256 mismatch) -- \
                     labels.json looks stale, re-run `squint draft` and re-export",
                    frame.filename
                );
            }
        }
    }

    #[derive(Serialize)]
    struct TagStats {
        tag: String,
        n: usize,
        mean_scores: Vec<(String, f64)>,
    }

    type TagPredicate = (&'static str, fn(&LabelRow) -> bool);
    let tags: [TagPredicate; 7] = [
        ("sharp", |l| l.sharp),
        ("motion_blur", |l| l.motion_blur),
        ("defocus", |l| l.defocus),
        ("misfocus", |l| l.misfocus),
        ("eyes_closed", |l| l.eyes_closed),
        ("eyes_obscured", |l| l.eyes_obscured),
        ("not_applicable", |l| l.not_applicable),
    ];

    let mut out = Vec::new();
    for (tag_name, predicate) in tags {
        let matching: Vec<&DraftFrame> = frames
            .iter()
            .filter(|f| {
                by_filename
                    .get(&f.filename)
                    .map(|l| predicate(l))
                    .unwrap_or(false)
            })
            .collect();
        if matching.is_empty() {
            continue;
        }
        let candidate_names: Vec<String> = matching[0]
            .candidate_scores
            .iter()
            .map(|(n, _)| n.clone())
            .collect();
        let mut mean_scores = Vec::new();
        for name in candidate_names {
            let sum: f64 = matching
                .iter()
                .filter_map(|f| f.candidate_scores.iter().find(|(n, _)| n == &name))
                .map(|(_, s)| *s)
                .sum();
            mean_scores.push((name, sum / matching.len() as f64));
        }
        out.push(TagStats {
            tag: tag_name.to_string(),
            n: matching.len(),
            mean_scores,
        });
    }

    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use squint::label::LabelRow;

    fn write_draft_json(work: &Path, frames: &[DraftFrame]) {
        fs::write(
            work.join("draft.json"),
            serde_json::to_string(frames).unwrap(),
        )
        .unwrap();
    }

    fn frame(filename: &str, sha256: &str) -> DraftFrame {
        DraftFrame {
            index: 0,
            filename: filename.to_string(),
            sha256: sha256.to_string(),
            thumb: "thumbs/0000.jpg".to_string(),
            candidate_scores: vec![("laplacian_variance".to_string(), 1.0)],
        }
    }

    #[test]
    fn run_labels_rejects_a_sha256_mismatch_for_a_matching_filename() {
        let dir = tempfile::tempdir().unwrap();
        write_draft_json(dir.path(), &[frame("DSC_0001.NEF", "aaa")]);

        let labels = vec![LabelRow {
            filename: "DSC_0001.NEF".to_string(),
            sha256: "bbb".to_string(), // different content -- a stale labels.json
            sharp: true,
            ..Default::default()
        }];
        let labels_path = dir.path().join("labels.json");
        fs::write(&labels_path, serde_json::to_string(&labels).unwrap()).unwrap();

        let err = run_labels(dir.path(), &labels_path).unwrap_err();
        assert!(err.to_string().contains("sha256 mismatch"));
    }

    #[test]
    fn run_labels_accepts_a_matching_sha256() {
        let dir = tempfile::tempdir().unwrap();
        write_draft_json(dir.path(), &[frame("DSC_0001.NEF", "aaa")]);

        let labels = vec![LabelRow {
            filename: "DSC_0001.NEF".to_string(),
            sha256: "aaa".to_string(),
            sharp: true,
            ..Default::default()
        }];
        let labels_path = dir.path().join("labels.json");
        fs::write(&labels_path, serde_json::to_string(&labels).unwrap()).unwrap();

        assert!(run_labels(dir.path(), &labels_path).is_ok());
    }
}
