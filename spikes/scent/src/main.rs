//! CLI for the #59 XMP-interop spike. Real file I/O, not just library tests
//! -- `survey` is what this spike's field/namespace-frequency findings in
//! `docs/research/scent-xmp-interop.md` come from.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use scent::{embedded, lrc_fields, packet, sidecar};

#[derive(Parser)]
#[command(name = "scent")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print the LRC-convention metadata found in a file's sidecar or
    /// embedded XMP packet.
    Dump { path: PathBuf },
    /// Set one field on a real sidecar/JPEG (creates a sidecar if the RAW
    /// has none yet).
    Write {
        path: PathBuf,
        #[arg(long)]
        rating: Option<i8>,
        #[arg(long)]
        label: Option<String>,
    },
    /// Read a file's current metadata, write it right back unchanged, and
    /// diff -- proves the read/patch round-trip is lossless on a real file.
    Roundtrip { path: PathBuf },
    /// Scan a directory of real sidecars/JPEGs and report field/namespace
    /// frequency.
    Survey { dir: PathBuf },
}

fn load_xmp(path: &Path) -> Result<(String, bool)> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if ext == "xmp" {
        return Ok((fs::read_to_string(path)?, false));
    }
    if ext == "jpg" || ext == "jpeg" {
        let data = fs::read(path)?;
        let xmp = embedded::read_xmp(&data)?
            .with_context(|| format!("no embedded XMP found in {}", path.display()))?;
        return Ok((xmp, true));
    }
    // RAW file: read its sidecar.
    let sidecar_path = sidecar::sidecar_path(path);
    Ok((
        fs::read_to_string(&sidecar_path)
            .with_context(|| format!("no sidecar at {}", sidecar_path.display()))?,
        false,
    ))
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Dump { path } => {
            let (xmp, _) = load_xmp(&path)?;
            let meta = lrc_fields::read(&xmp)?;
            println!("{meta:#?}");
        }
        Command::Write {
            path,
            rating,
            label,
        } => {
            let (xmp, is_embedded) = load_xmp(&path).unwrap_or_else(|_| (String::new(), false));
            let xmp = if xmp.is_empty() {
                r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description xmlns:xmp="http://ns.adobe.com/xap/1.0/"/></rdf:RDF></x:xmpmeta>"#.to_string()
            } else {
                xmp
            };
            let patched = packet::apply(
                &xmp,
                &packet::Patch {
                    rating: rating.map(Some),
                    label: label.map(Some),
                    ..Default::default()
                },
            )?;
            if is_embedded {
                let data = fs::read(&path)?;
                let rewritten = embedded::write_xmp(&data, &patched)?;
                sidecar::atomic_write(&path, &rewritten)?;
            } else {
                let target = sidecar::sidecar_path(&path);
                sidecar::atomic_write(&target, patched.as_bytes())?;
                println!("wrote {}", target.display());
            }
        }
        Command::Roundtrip { path } => {
            let (xmp, _) = load_xmp(&path)?;
            let before = lrc_fields::read(&xmp)?;
            let patched = packet::apply(&xmp, &packet::Patch::default())?;
            let after = lrc_fields::read(&patched)?;
            if before == after {
                println!("OK: metadata identical after a no-op patch");
            } else {
                println!("MISMATCH:\n  before: {before:#?}\n  after:  {after:#?}");
            }
            if xmp == patched {
                println!("byte-identical");
            } else {
                println!(
                    "not byte-identical ({} vs {} bytes) -- expected if quick-xml normalizes \
                     whitespace/attribute quoting; metadata equality above is what matters",
                    xmp.len(),
                    patched.len()
                );
            }
        }
        Command::Survey { dir } => {
            let mut field_counts: BTreeMap<&'static str, u32> = BTreeMap::new();
            let mut total = 0u32;
            for entry in fs::read_dir(&dir)? {
                let entry = entry?;
                let path = entry.path();
                let ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                let xmp = if ext == "xmp" {
                    fs::read_to_string(&path).ok()
                } else if ext == "jpg" || ext == "jpeg" {
                    fs::read(&path)
                        .ok()
                        .and_then(|d| embedded::read_xmp(&d).ok().flatten())
                } else {
                    continue;
                };
                let Some(xmp) = xmp else { continue };
                total += 1;
                let Ok(meta) = lrc_fields::read(&xmp) else {
                    continue;
                };
                if meta.rating.is_some() {
                    *field_counts.entry("rating").or_default() += 1;
                }
                if meta.label.is_some() {
                    *field_counts.entry("label").or_default() += 1;
                }
                if !meta.keywords.is_empty() {
                    *field_counts.entry("keywords").or_default() += 1;
                }
                if !meta.hierarchical_keywords.is_empty() {
                    *field_counts.entry("hierarchical_keywords").or_default() += 1;
                }
            }
            println!("{total} files with an XMP packet found");
            for (field, count) in field_counts {
                println!("  {field}: {count}/{total}");
            }
        }
    }
    Ok(())
}
