//! Deterministic synthetic-catalog generator. Distributions are sampled from
//! `docs/ref-10k-manifest.csv` (real Z8/D7500 EXIF values) so the query set is exercised against
//! realistic camera/ISO/compression/dimension combinations without needing 2M real files.

use rand::rngs::StdRng;
use rand::seq::IndexedRandom;
use rand::{RngExt, SeedableRng};
use rand_distr::{Distribution, Zipf};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Flag {
    None,
    Pick,
    Reject,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Asset {
    pub id: u64,
    pub folder_path: String,
    pub filename: String,
    pub capture_date: String, // ISO 8601 date, sortable as text
    pub model: String,
    pub iso: u32,
    pub compression: String,
    pub width: u32,
    pub height: u32,
    pub size_bytes: u64,
    pub rating: u8, // 0-5
    pub flag: Flag,
    /// Hierarchical keyword paths, e.g. "Locations.United_States.Washington_DC".
    pub keywords: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ManifestRow {
    #[serde(rename = "id")]
    _id: String,
    #[serde(rename = "sha256")]
    _sha256: String,
    #[serde(rename = "bucket")]
    _bucket: String,
    model: String,
    iso: u32,
    compression: String,
    width: u32,
    height: u32,
    size_bytes: u64,
}

/// Reads the reference manifest's EXIF rows to sample from. Falls back to a small built-in table
/// if the manifest isn't available (e.g. a spike-only checkout), so `den gen` never hard-fails.
fn load_exif_pool(manifest_path: &Path) -> Vec<ManifestRow> {
    if let Ok(mut rdr) = csv::Reader::from_path(manifest_path) {
        let rows: Vec<ManifestRow> = rdr.deserialize().filter_map(Result::ok).collect();
        if !rows.is_empty() {
            return rows;
        }
    }
    vec![
        ManifestRow {
            _id: String::new(),
            _sha256: String::new(),
            _bucket: "z8".into(),
            model: "NIKON Z 8".into(),
            iso: 400,
            compression: "High Efficiency".into(),
            width: 8280,
            height: 5520,
            size_bytes: 20_000_000,
        },
        ManifestRow {
            _id: String::new(),
            _sha256: String::new(),
            _bucket: "d7500".into(),
            model: "NIKON D7500".into(),
            iso: 200,
            compression: "Lossless".into(),
            width: 6000,
            height: 4000,
            size_bytes: 25_000_000,
        },
    ]
}

const LOCATIONS: &[&str] = &[
    "Locations.United_States.Washington_DC",
    "Locations.United_States.New_York",
    "Locations.United_States.California",
    "Locations.United_States.Texas",
    "Locations.Canada.Ontario",
    "Locations.United_Kingdom.London",
];
const SUBJECTS: &[&str] = &[
    "Furry.Fursuit",
    "Furry.Con",
    "People.Portrait",
    "Events.Meetup",
    "Events.Photoshoot",
];

/// A stable, genuinely rare leaf keyword to query in benchmarks (one specific event out of
/// thousands), distinct from the broad `LOCATIONS`/`SUBJECTS` categories every asset also gets a
/// few of. Real catalogs tag by specific event/subject far more often than by broad category
/// alone — a benchmark that only ever filters on the ~11 broad values can't be selective at any
/// scale, no matter how good the query is (confirmed while investigating #67's own facet/keyword
/// query-plan results: those two numbers stayed slow after fixing the query itself, because
/// *every* leaf in the old vocabulary matched a large fraction of the 2M-row corpus).
pub const BENCH_LEAF_KEYWORD: &str = "Events.Named.event-00042";

pub struct GenOptions {
    pub seed: u64,
    pub asset_count: u64,
    pub folder_count: u64,
    pub manifest_path: std::path::PathBuf,
}

/// Generates a deterministic synthetic catalog. Same `seed` + `asset_count` always produces
/// byte-identical output (verified by the `den gen` CLI's `--verify-determinism` check).
pub fn generate_catalog(opts: &GenOptions) -> Vec<Asset> {
    let mut rng = StdRng::seed_from_u64(opts.seed);
    let exif_pool = load_exif_pool(&opts.manifest_path);

    // Folder tree: volume/year/event, event count derived from folder_count.
    let events_per_year = (opts.folder_count / 6).max(1);
    let mut folders = Vec::with_capacity(opts.folder_count as usize);
    for year in 2020..2026u32 {
        for event in 0..events_per_year {
            folders.push(format!("NVMe/{year}/event-{event:05}"));
        }
    }
    folders.truncate(opts.folder_count.max(1) as usize);
    if folders.is_empty() {
        folders.push("NVMe/2026/event-00000".to_string());
    }

    // Zipf-distributed keyword vocabulary: a handful of hot keywords, a long tail.
    let keyword_zipf = Zipf::new((LOCATIONS.len() + SUBJECTS.len()) as u64, 1.2).unwrap();
    let all_keywords: Vec<&str> = LOCATIONS.iter().chain(SUBJECTS.iter()).copied().collect();

    let mut assets = Vec::with_capacity(opts.asset_count as usize);
    for id in 0..opts.asset_count {
        let exif = exif_pool.choose(&mut rng).unwrap();
        let folder = folders.choose(&mut rng).unwrap();
        let year: u32 = folder.split('/').nth(1).unwrap().parse().unwrap();
        let month = rng.random_range(1..=12u32);
        let day = rng.random_range(1..=28u32);

        // 2-10% keep rate: most assets are unrated/rejected, a minority are picks.
        let roll: f64 = rng.random();
        let (rating, flag) = if roll < 0.06 {
            (rng.random_range(3..=5u8), Flag::Pick)
        } else if roll < 0.30 {
            (0, Flag::Reject)
        } else {
            (0, Flag::None)
        };

        let keyword_count = rng.random_range(0..=3usize);
        let mut keywords = Vec::with_capacity(keyword_count + 1);
        for _ in 0..keyword_count {
            let idx = (keyword_zipf.sample(&mut rng) as usize - 1).min(all_keywords.len() - 1);
            let kw = all_keywords[idx];
            if !keywords.contains(&kw.to_string()) {
                keywords.push(kw.to_string());
            }
        }
        // Every asset also carries its specific named event as a keyword leaf — this is the
        // cardinality-scales-with-catalog-size tag a real hierarchical keyword filter narrows to
        // (see BENCH_LEAF_KEYWORD's doc comment), distinct from the small, low-selectivity
        // LOCATIONS/SUBJECTS taxonomy above.
        let event_name = folder.rsplit('/').next().unwrap();
        keywords.push(format!("Events.Named.{event_name}"));

        assets.push(Asset {
            id,
            folder_path: folder.clone(),
            filename: format!("DSC_{id:08}.NEF"),
            capture_date: format!("{year:04}-{month:02}-{day:02}"),
            model: exif.model.clone(),
            iso: exif.iso,
            compression: exif.compression.clone(),
            width: exif.width,
            height: exif.height,
            size_bytes: exif.size_bytes,
            rating,
            flag,
            keywords,
        });
    }
    assets
}

/// Hashes the generated catalog for the determinism check (`den gen --verify-determinism`).
pub fn catalog_hash(assets: &[Asset]) -> String {
    let mut hasher = blake3::Hasher::new();
    for a in assets {
        hasher.update(serde_json::to_vec(a).unwrap().as_slice());
    }
    hasher.finalize().to_hex().to_string()
}
