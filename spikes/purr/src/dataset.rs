//! On-disk formats for the two-stage `purr dataset` -> `purr extract` pipeline, plus the parallel
//! extraction runner. Both files are scratch state (a manifest of the user's own real catalog
//! paths, and a feature cache derived from their own real photos) -- **never committed to the
//! repo**, per ADR-0061's privacy note and CONTRIBUTING.md's `ref-10k` handling.

use std::path::Path;

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::catalog::KeeperRow;
use crate::features::{ImageFeatures, HIST_FEATURE_COUNT};
use crate::sliders::{Sliders, SLIDER_COUNT};

#[derive(Serialize, Deserialize, Clone)]
pub struct ManifestRow {
    pub image_id: i64,
    pub folder_id: i64,
    pub capture_time: Option<String>,
    pub lrc_path: String,
    pub sliders: Sliders,
    pub iso: Option<f64>,
    pub shutter_speed: Option<f64>,
    pub aperture: Option<f64>,
}

impl From<KeeperRow> for ManifestRow {
    fn from(row: KeeperRow) -> Self {
        Self {
            image_id: row.image_id,
            folder_id: row.folder_id,
            capture_time: row.capture_time,
            lrc_path: row.lrc_path,
            sliders: row.sliders,
            iso: row.iso,
            shutter_speed: row.shutter_speed,
            aperture: row.aperture,
        }
    }
}

pub fn write_manifest(path: &Path, rows: &[ManifestRow]) -> anyhow::Result<()> {
    let json = serde_json::to_vec_pretty(rows)?;
    std::fs::write(path, json)?;
    Ok(())
}

pub fn read_manifest(path: &Path) -> anyhow::Result<Vec<ManifestRow>> {
    let bytes = std::fs::read(path)?;
    Ok(serde_json::from_slice(&bytes)?)
}

#[derive(Serialize, Deserialize, Clone)]
pub struct FeatureRow {
    pub folder_id: i64,
    pub capture_time: Option<String>,
    pub hist: [f32; HIST_FEATURE_COUNT],
    pub thumb: Vec<f32>,
    pub iso: Option<f64>,
    pub shutter_speed: Option<f64>,
    pub aperture: Option<f64>,
    pub targets: [f64; SLIDER_COUNT],
}

#[derive(Default)]
pub struct ExtractReport {
    pub attempted: usize,
    pub unreachable: usize,
    pub failed: usize,
    pub succeeded: usize,
    /// Count of each concrete `FeatureError` variant among the `failed` rows, keyed by its variant
    /// name (`Io`/`NoEmbeddedJpeg`/`Ifd`/`JpegDecode`/`Resize`) -- without this, `failed` alone
    /// can't distinguish "this camera's preview format has no embedded JPEG" (a sample-composition
    /// concern) from "a JPEG decode bug" (a real bug) from a transient I/O error (found in
    /// adversarial review: the original version discarded the error entirely via `.ok()`).
    pub failure_kinds: std::collections::HashMap<&'static str, usize>,
}

/// Extracts features for every manifest row in parallel (a 12-worker pool, matching the WSL 9p I/O
/// ceiling measured for `ref-10k` copies elsewhere in this repo). A row whose file can't be
/// resolved to a real path (`catalog::resolve_path` returning `None`, e.g. a UNC root) or whose
/// extraction fails is skipped and counted, never aborting the whole run -- the same "one bad file
/// doesn't stop the batch" stance `nicti-lair::scruff`'s import pipeline already takes.
pub fn extract_all(rows: &[ManifestRow]) -> (Vec<FeatureRow>, ExtractReport) {
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(12)
        .build()
        .expect("building a fixed-size thread pool");

    // `Ok(Some(_))` = extracted, `Ok(None)` = path unresolvable (never attempted extraction),
    // `Err(_)` = extraction was attempted and failed -- kept as three distinct outcomes so the
    // report below can route "unreachable" vs. "failed" without re-deriving resolvability a
    // second time (found in adversarial review: an earlier version called `resolve_path` twice
    // and, before that, discarded the failure reason entirely via `.ok()`).
    let results: Vec<Result<Option<FeatureRow>, crate::features::FeatureError>> =
        pool.install(|| {
            rows.par_iter()
                .map(|row| {
                    let Some(path) = crate::catalog::resolve_path(&row.lrc_path) else {
                        return Ok(None);
                    };
                    let ImageFeatures { hist, thumb } = crate::features::extract(&path)?;
                    Ok(Some(FeatureRow {
                        folder_id: row.folder_id,
                        capture_time: row.capture_time.clone(),
                        hist,
                        thumb,
                        iso: row.iso,
                        shutter_speed: row.shutter_speed,
                        aperture: row.aperture,
                        targets: row.sliders.as_array(),
                    }))
                })
                .collect()
        });

    let mut report = ExtractReport {
        attempted: rows.len(),
        ..Default::default()
    };
    let mut features = Vec::with_capacity(results.len());
    for result in results {
        match result {
            Ok(Some(f)) => {
                report.succeeded += 1;
                features.push(f);
            }
            Ok(None) => report.unreachable += 1,
            Err(e) => {
                report.failed += 1;
                let kind = match e {
                    crate::features::FeatureError::Io(..) => "io",
                    crate::features::FeatureError::NoEmbeddedJpeg(..) => "no_embedded_jpeg",
                    crate::features::FeatureError::Ifd(..) => "ifd",
                    crate::features::FeatureError::JpegDecode(..) => "jpeg_decode",
                    crate::features::FeatureError::Resize(..) => "resize",
                };
                *report.failure_kinds.entry(kind).or_insert(0) += 1;
            }
        }
    }
    (features, report)
}

pub fn write_feature_cache(path: &Path, rows: &[FeatureRow]) -> anyhow::Result<()> {
    let bytes = bincode::serialize(rows)?;
    std::fs::write(path, bytes)?;
    Ok(())
}

pub fn read_feature_cache(path: &Path) -> anyhow::Result<Vec<FeatureRow>> {
    let bytes = std::fs::read(path)?;
    Ok(bincode::deserialize(&bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::THUMB_FEATURE_COUNT;

    #[test]
    fn manifest_round_trips_through_json() {
        let rows = vec![ManifestRow {
            image_id: 1,
            folder_id: 2,
            capture_time: Some("2026-01-01".to_string()),
            lrc_path: "C:/x/a.NEF".to_string(),
            sliders: Sliders {
                exposure2012: 0.5,
                ..Default::default()
            },
            iso: Some(400.0),
            shutter_speed: None,
            aperture: None,
        }];
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.json");
        write_manifest(&path, &rows).unwrap();
        let read_back = read_manifest(&path).unwrap();
        assert_eq!(read_back.len(), 1);
        assert_eq!(read_back[0].image_id, 1);
    }

    #[test]
    fn feature_cache_round_trips_through_bincode() {
        let rows = vec![FeatureRow {
            folder_id: 1,
            capture_time: None,
            hist: [0.5; HIST_FEATURE_COUNT],
            thumb: vec![0.1; THUMB_FEATURE_COUNT],
            iso: None,
            shutter_speed: None,
            aperture: None,
            targets: [0.0; SLIDER_COUNT],
        }];
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("features.bin");
        write_feature_cache(&path, &rows).unwrap();
        let read_back = read_feature_cache(&path).unwrap();
        assert_eq!(read_back.len(), 1);
        assert_eq!(read_back[0].thumb.len(), THUMB_FEATURE_COUNT);
    }

    #[test]
    fn extract_all_counts_an_unresolvable_path_without_aborting() {
        let rows = vec![ManifestRow {
            image_id: 1,
            folder_id: 1,
            capture_time: None,
            lrc_path: "//unc-share/x.NEF".to_string(),
            sliders: Sliders::default(),
            iso: None,
            shutter_speed: None,
            aperture: None,
        }];
        let (features, report) = extract_all(&rows);
        assert!(features.is_empty());
        assert_eq!(report.unreachable, 1);
        assert_eq!(report.failed, 0);
    }
}
