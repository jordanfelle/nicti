//! `sniff inventory <root> <manifest.csv>`: walks every file in the frozen `ref-10k` set, finds
//! its embedded JPEG(s) via `ifd::Walker`, inspects each one's header, and writes one CSV row
//! per embedded JPEG. Also verifies file integrity against the committed manifest's SHA-256
//! column (full verification on NVMe, a sample on HDD -- hashing 232GB on a spinning disk is not
//! worth the wall-clock for a research spike).

use crate::ifd::{PreviewSource, Walker};
use crate::jpeg_meta;
use rayon::prelude::*;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize)]
pub struct InventoryRow {
    pub id: String,
    pub bucket: String,
    pub model: String,
    pub source: String,
    pub file_offset: u64,
    pub byte_len: u64,
    pub declared_width: Option<u32>,
    pub declared_height: Option<u32>,
    pub new_subfile_type: Option<u32>,
    pub sof_width: Option<u16>,
    pub sof_height: Option<u16>,
    pub subsampling: Option<String>,
    pub quality_estimate: Option<u8>,
    pub sha256_ok: Option<bool>,
    pub parse_error: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct ManifestRow {
    id: String,
    sha256: String,
    bucket: String,
    model: String,
    #[allow(dead_code)]
    iso: String,
    #[allow(dead_code)]
    compression: String,
    #[allow(dead_code)]
    width: u32,
    #[allow(dead_code)]
    height: u32,
    #[allow(dead_code)]
    size_bytes: u64,
}

fn source_name(s: PreviewSource) -> String {
    match s {
        PreviewSource::Ifd0 => "ifd0".into(),
        PreviewSource::ThumbnailIfd => "thumbnail_ifd".into(),
        PreviewSource::SubIfd(i) => format!("sub_ifd_{i}"),
        PreviewSource::NikonPreviewIfd => "nikon_preview_ifd".into(),
    }
}

fn sha256_hex(path: &Path) -> std::io::Result<String> {
    let mut f = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    Ok(digest.iter().map(|b| format!("{b:02x}")).collect())
}

fn process_file(
    path: &Path,
    manifest_row: Option<&ManifestRow>,
    verify_hash: bool,
) -> Vec<InventoryRow> {
    let id = path
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_default();
    let bucket = manifest_row.map(|m| m.bucket.clone()).unwrap_or_default();
    let model = manifest_row.map(|m| m.model.clone()).unwrap_or_default();

    let sha256_ok = if verify_hash {
        manifest_row.and_then(|m| sha256_hex(path).ok().map(|actual| actual == m.sha256))
    } else {
        None
    };

    let data = match fs::read(path) {
        Ok(d) => d,
        Err(e) => {
            return vec![InventoryRow {
                id,
                bucket,
                model,
                source: String::new(),
                file_offset: 0,
                byte_len: 0,
                declared_width: None,
                declared_height: None,
                new_subfile_type: None,
                sof_width: None,
                sof_height: None,
                subsampling: None,
                quality_estimate: None,
                sha256_ok,
                parse_error: Some(format!("read error: {e}")),
            }]
        }
    };

    let mut walker = match Walker::new(&data) {
        Ok(w) => w,
        Err(e) => {
            return vec![InventoryRow {
                id,
                bucket,
                model,
                source: String::new(),
                file_offset: 0,
                byte_len: 0,
                declared_width: None,
                declared_height: None,
                new_subfile_type: None,
                sof_width: None,
                sof_height: None,
                subsampling: None,
                quality_estimate: None,
                sha256_ok,
                parse_error: Some(format!("TIFF header error: {e}")),
            }]
        }
    };

    let jpegs = match walker.find_embedded_jpegs() {
        Ok(v) => v,
        Err(e) => {
            return vec![InventoryRow {
                id,
                bucket,
                model,
                source: String::new(),
                file_offset: 0,
                byte_len: 0,
                declared_width: None,
                declared_height: None,
                new_subfile_type: None,
                sof_width: None,
                sof_height: None,
                subsampling: None,
                quality_estimate: None,
                sha256_ok,
                parse_error: Some(format!("IFD walk error: {e}")),
            }]
        }
    };

    if jpegs.is_empty() {
        return vec![InventoryRow {
            id,
            bucket,
            model,
            source: String::new(),
            file_offset: 0,
            byte_len: 0,
            declared_width: None,
            declared_height: None,
            new_subfile_type: None,
            sof_width: None,
            sof_height: None,
            subsampling: None,
            quality_estimate: None,
            sha256_ok,
            parse_error: Some("no embedded JPEG found".to_string()),
        }];
    }

    jpegs
        .into_iter()
        .map(|j| {
            let start = j.file_offset as usize;
            let end = (j.file_offset + j.byte_len) as usize;
            let header = data
                .get(start..end.min(data.len()))
                .map(jpeg_meta::inspect)
                .unwrap_or_default();
            InventoryRow {
                id: id.clone(),
                bucket: bucket.clone(),
                model: model.clone(),
                source: source_name(j.source),
                file_offset: j.file_offset,
                byte_len: j.byte_len,
                declared_width: j.declared_width,
                declared_height: j.declared_height,
                new_subfile_type: j.new_subfile_type,
                sof_width: header.width,
                sof_height: header.height,
                subsampling: header.subsampling.map(|s| s.to_string()),
                quality_estimate: header.quality_estimate,
                sha256_ok,
                parse_error: if header.has_soi {
                    None
                } else {
                    Some("extracted range failed JPEG SOI check".to_string())
                },
            }
        })
        .collect()
}

pub fn run(
    root: &Path,
    manifest_path: &Path,
    out_csv: &Path,
    hdd_sample_every: u32,
) -> std::io::Result<()> {
    let mut reader = csv::Reader::from_path(manifest_path)?;
    let manifest: HashMap<String, ManifestRow> = reader
        .deserialize::<ManifestRow>()
        .filter_map(|r| r.ok())
        .map(|r| (r.id.clone(), r))
        .collect();

    let is_hdd_like = hdd_sample_every > 1;

    let paths: Vec<PathBuf> = manifest
        .keys()
        .map(|id| root.join(id))
        .filter(|p| p.exists())
        .collect();

    let rows: Vec<InventoryRow> = paths
        .par_iter()
        .enumerate()
        .flat_map(|(i, path)| {
            let id = path.file_name().unwrap().to_string_lossy().to_string();
            let m = manifest.get(&id);
            let verify = !is_hdd_like || (i as u32).is_multiple_of(hdd_sample_every);
            process_file(path, m, verify)
        })
        .collect();

    let mut writer = csv::Writer::from_path(out_csv)?;
    for row in &rows {
        writer.serialize(row).map_err(std::io::Error::other)?;
    }
    writer.flush()?;

    let errors = rows.iter().filter(|r| r.parse_error.is_some()).count();
    let hash_fails = rows.iter().filter(|r| r.sha256_ok == Some(false)).count();
    eprintln!(
        "inventory: {} files, {} embedded-JPEG rows, {} parse errors, {} sha256 mismatches",
        paths.len(),
        rows.len(),
        errors,
        hash_fails
    );
    Ok(())
}
