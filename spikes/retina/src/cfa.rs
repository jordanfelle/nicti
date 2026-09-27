//! `retina dump-cfa`: for #40/ADR-0040's Path A (Bayer-domain model) input. Writes the
//! still-mosaiced, black-subtracted, white-normalized Bayer plane as a 16-bit grayscale TIFF (the
//! shim's normalized-0..1 float rescaled to 0..65535 -- `image` 0.25's `tiff` feature has no
//! plain single-channel float `ColorType`, so this avoids a runtime encode failure rather than
//! fighting the format; a `rods` reader rescales back to float on load), plus a metadata JSON
//! sidecar carrying what a caller needs to interpret the RGGB phase and undo the normalization
//! (`black`/`maximum`) if a candidate model expects sensor-native values instead.
//!
//! No white balance, no demosaic -- this is deliberately earlier in the pipeline than
//! `dump-linear`/`dump-classic`.

use std::fs;
use std::path::{Path, PathBuf};

use image::{ImageBuffer, Luma};
use serde::Serialize;

use nicti_decode::LibRawHandle;

#[derive(Debug, Serialize)]
pub struct CfaMeta {
    pub make: String,
    pub model: String,
    pub raw_width: u32,
    pub raw_height: u32,
    /// LibRaw's CFA pattern descriptor (`imgdata.idata.filters`) -- callers derive each pixel's
    /// color via the same FC(row, col) bit-pattern LibRaw itself uses, not a fixed RGGB
    /// assumption (a handful of Nikon bodies use a different phase offset).
    pub filters: u32,
    pub black: u32,
    pub maximum: u32,
    /// As-shot white-balance multipliers (LibRaw's `cam_mul`), R/G/B/G2 -- not applied to the
    /// written plane, provided so a candidate model that wants WB-aware input can apply it itself.
    pub cam_mul: [f32; 4],
}

/// Decodes `path` with LibRaw and writes `<stem>.cfa.tiff` (16-bit grayscale, rescaled from the
/// shim's normalized 0..1 float) + `<stem>.cfa.meta.json` into `out_dir` (created if missing).
pub fn dump_cfa(path: &Path, out_dir: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(out_dir)?;
    let data = fs::read(path)?;

    let mut handle = LibRawHandle::new();
    handle
        .decode(&data)
        .map_err(|e| anyhow::anyhow!("decode {}: {e}", path.display()))?;

    let meta = handle.metadata();
    let cfa = handle.cfa_normalized()?;

    let width = meta.raw_width as u32;
    let height = meta.raw_height as u32;
    anyhow::ensure!(
        cfa.len() == width as usize * height as usize,
        "cfa_normalized length {} != raw_width*raw_height ({}) for {}",
        cfa.len(),
        width as usize * height as usize,
        path.display()
    );

    // Round-to-nearest rather than truncate, so 1.0 lands exactly on u16::MAX rather than
    // wrapping/saturating asymmetrically at the two ends of the range.
    let scaled: Vec<u16> = cfa
        .iter()
        .map(|&v| (v.clamp(0.0, 1.0) * u16::MAX as f32).round() as u16)
        .collect();
    let buf: ImageBuffer<Luma<u16>, Vec<u16>> = ImageBuffer::from_raw(width, height, scaled)
        .ok_or_else(|| anyhow::anyhow!("buffer size mismatch building CFA image"))?;

    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("output");
    let tiff_path: PathBuf = out_dir.join(format!("{stem}.cfa.tiff"));
    let json_path: PathBuf = out_dir.join(format!("{stem}.cfa.meta.json"));

    buf.save(&tiff_path)
        .map_err(|e| anyhow::anyhow!("write {}: {e}", tiff_path.display()))?;

    let sidecar = CfaMeta {
        make: meta.make.clone(),
        model: meta.model.clone(),
        raw_width: width,
        raw_height: height,
        filters: meta.filters,
        black: meta.black,
        maximum: meta.maximum,
        cam_mul: meta.cam_mul,
    };
    fs::write(&json_path, serde_json::to_string_pretty(&sidecar)?)?;

    eprintln!(
        "wrote {} ({width}x{height}) and {}",
        tiff_path.display(),
        json_path.display()
    );
    Ok(())
}
