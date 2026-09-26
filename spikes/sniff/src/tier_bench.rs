//! Wires together ranged IFD extraction, codec re-encode, and a cache backend to produce #29's
//! real per-tier format comparison. Two tiers, two very different payload shapes:
//! - **T0 grid**: `nikon_preview_ifd` (640x424, ~137 KB), read verbatim, no re-encode -- already
//!   a camera-produced JPEG.
//! - **T2 screen**: `JpgFromRaw` decoded + resized to `decode::SCREEN_TIER_LONG_EDGE`, then
//!   re-encoded with the chosen `codec::Codec` at `quality` (~1.2 MB JPEG or ~290 KB AVIF at the
//!   settings this ADR measured).
//!
//! Both tiers measure the same round-trip: populate `cache::CacheFormat`, then random-order
//! read+decode. Added at the user's request to put real numbers behind the AVIF-vs-JPEG
//! tier-payload-format question, and the SQLite-vs-pack-vs-file cache-backend question, at both
//! payload sizes -- rather than deciding either from priors alone.

use crate::bench::pick_candidate;
use crate::cache::{
    CacheFormat, CacheKey, FileCache, Format as CacheKind, PackCache, SqliteBlobCache,
};
use crate::codec::{self, Codec};
use crate::decode;
use crate::ifd::Walker;
use crate::source::FileSource;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Debug, Clone, Copy, clap::ValueEnum, PartialEq, Eq)]
pub enum Tier {
    /// `nikon_preview_ifd`, read verbatim -- no codec/quality choice applies (see `run`'s docs).
    T0Grid,
    /// `JpgFromRaw`, decoded/resized/re-encoded with the chosen codec.
    T2Screen,
}

#[derive(Debug, Serialize)]
pub struct TierBenchResult {
    pub tier: String,
    pub codec: String,
    pub quality: u8,
    /// Only meaningful for `codec: "avif"`; recorded regardless so a JPEG/WebP run's result JSON
    /// still shows what was passed (usually the CLI default), rather than omitting the field.
    pub avif_speed: u8,
    pub cache_format: String,
    pub n_assets: usize,
    pub n_encode_failures: usize,
    pub populate_wall_ms: u128,
    pub total_encoded_bytes: u64,
    pub avg_encoded_bytes: f64,
    pub disk_bytes: u64,
    pub encode_p50_ms: f64,
    pub encode_p95_ms: f64,
    pub read_decode_p50_ms: f64,
    pub read_decode_p95_ms: f64,
    pub read_decode_max_ms: f64,
    /// `None` unless `--ssim` was passed. Encoded-vs-pre-encode-source SSIM, in `[-1.0, 1.0]`
    /// (1.0 = identical) -- see `nicti_prowl::golden::ssim`. Absent for T0 (verbatim, no
    /// re-encode, so there's nothing to score against).
    pub ssim_mean: Option<f64>,
    pub ssim_p5: Option<f64>,
    pub ssim_min: Option<f64>,
}

/// `(encoded bytes, encode-only elapsed, the resized pre-encode source, if extraction ran a
/// re-encode step)`. Shared by `extract_grid_verbatim` and `extract_and_encode` so `run`'s loop
/// can treat both tiers uniformly.
type ExtractResult =
    Result<(Vec<u8>, std::time::Duration, Option<decode::DecodedRgb>), Box<dyn std::error::Error>>;

fn percentile(sorted_ms: &[f64], p: f64) -> f64 {
    if sorted_ms.is_empty() {
        return 0.0;
    }
    let idx = ((sorted_ms.len() as f64 - 1.0) * p).round() as usize;
    sorted_ms[idx.min(sorted_ms.len() - 1)]
}

/// Walks each root non-recursively (not recursively merged into one deep walk -- #143's
/// stratified subset is intentionally several sibling folders, not a nested tree) and merges the
/// matches, still sorted overall so results stay deterministic regardless of `--root` order.
fn collect_raw_files(roots: &[PathBuf]) -> std::io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for root in roots {
        files.extend(
            std::fs::read_dir(root)?
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.extension()
                        .map(|e| {
                            let e = e.to_string_lossy().to_lowercase();
                            e == "nef" || e == "dng"
                        })
                        .unwrap_or(false)
                }),
        );
    }
    files.sort();
    Ok(files)
}

fn to_rgb_image(img: &decode::DecodedRgb) -> Result<image::RgbImage, Box<dyn std::error::Error>> {
    image::RgbImage::from_raw(img.width, img.height, img.rgb.clone())
        .ok_or_else(|| "buffer size doesn't match width*height*3".into())
}

fn percentile_opt(sorted: &[f64], p: f64) -> Option<f64> {
    if sorted.is_empty() {
        None
    } else {
        Some(percentile(sorted, p))
    }
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    roots: &[PathBuf],
    tier_kind: Tier,
    codec_kind: Codec,
    quality: u8,
    avif_speed: u8,
    cache_kind: CacheKind,
    sample_limit: Option<usize>,
    compute_ssim: bool,
    out_dir: &Path,
) -> Result<TierBenchResult, Box<dyn std::error::Error>> {
    let mut files = collect_raw_files(roots)?;
    if let Some(limit) = sample_limit {
        files.truncate(limit);
    }

    let mut payloads: Vec<(u32, Vec<u8>)> = Vec::with_capacity(files.len());
    let mut encode_ms: Vec<f64> = Vec::with_capacity(files.len());
    let mut ssim_scores: Vec<f64> = Vec::new();
    let mut n_encode_failures = 0usize;

    for (idx, path) in files.iter().enumerate() {
        let result = match tier_kind {
            Tier::T0Grid => {
                extract_grid_verbatim(path).map(|(bytes, elapsed)| (bytes, elapsed, None))
            }
            Tier::T2Screen => extract_and_encode(path, codec_kind, quality, avif_speed),
        };
        match result {
            Ok((bytes, elapsed, resized_source)) => {
                if compute_ssim {
                    if let Some(source) = &resized_source {
                        match score_ssim(codec_kind, &bytes, source) {
                            Ok(score) => ssim_scores.push(score),
                            Err(e) => eprintln!("ssim skipped for {}: {e}", path.display()),
                        }
                    }
                }
                payloads.push((idx as u32, bytes));
                encode_ms.push(elapsed.as_secs_f64() * 1000.0);
            }
            Err(e) => {
                n_encode_failures += 1;
                eprintln!("skipping {}: {e}", path.display());
            }
        }
    }

    std::fs::create_dir_all(out_dir)?;
    let tier = match tier_kind {
        Tier::T0Grid => "t0_grid",
        Tier::T2Screen => "t2_screen",
    };
    let mut cache: Box<dyn CacheFormat> = match cache_kind {
        CacheKind::Sqlite => Box::new(SqliteBlobCache::open(&out_dir.join("previews.db"))?),
        CacheKind::Pack => Box::new(PackCache::open(
            &out_dir.join("pack.bin"),
            &out_dir.join("pack_index.db"),
        )?),
        CacheKind::File => Box::new(FileCache::open(&out_dir.join("files"))?),
    };

    let populate_start = Instant::now();
    cache.begin_batch()?;
    for (id, bytes) in &payloads {
        cache.put(
            CacheKey {
                asset_id: *id,
                tier,
            },
            bytes,
        )?;
    }
    cache.commit_batch()?;
    let populate_wall_ms = populate_start.elapsed().as_millis();
    let disk_bytes = cache.disk_bytes()?;

    let mut order: Vec<u32> = payloads.iter().map(|(id, _)| *id).collect();
    {
        use rand::seq::SliceRandom;
        let mut rng = rand::rng();
        order.shuffle(&mut rng);
    }

    // T0 is always a verbatim camera JPEG -- no codec choice applies, so decode it as JPEG
    // regardless of `--codec` (which only means anything for T2).
    let decode_codec = match tier_kind {
        Tier::T0Grid => Codec::Jpeg,
        Tier::T2Screen => codec_kind,
    };

    // A cache miss or decode error must never land in `read_decode_ms` as a fast "success" --
    // that would silently deflate the very percentiles the JPEG-vs-AVIF comparison in the ADR
    // relies on. Fail loudly instead (a hostile review caught this: the original `if let Some`
    // + `let _ =` swallowed both cases while still timing them).
    let mut read_decode_ms: Vec<f64> = Vec::with_capacity(order.len());
    for id in order {
        let start = Instant::now();
        let bytes = cache
            .get(CacheKey { asset_id: id, tier })?
            .ok_or_else(|| format!("cache miss for asset {id}"))?;
        codec::decode(decode_codec, &bytes)?;
        read_decode_ms.push(start.elapsed().as_secs_f64() * 1000.0);
    }

    encode_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    read_decode_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    ssim_scores.sort_by(|a, b| a.partial_cmp(b).unwrap());

    let total_encoded_bytes: u64 = payloads.iter().map(|(_, b)| b.len() as u64).sum();
    let avg_encoded_bytes = if payloads.is_empty() {
        0.0
    } else {
        total_encoded_bytes as f64 / payloads.len() as f64
    };
    let ssim_mean = if ssim_scores.is_empty() {
        None
    } else {
        Some(ssim_scores.iter().sum::<f64>() / ssim_scores.len() as f64)
    };

    Ok(TierBenchResult {
        tier: tier.to_string(),
        codec: format!("{decode_codec:?}").to_lowercase(),
        quality: if tier_kind == Tier::T0Grid {
            0
        } else {
            quality
        },
        avif_speed,
        cache_format: format!("{cache_kind:?}").to_lowercase(),
        n_assets: payloads.len(),
        n_encode_failures,
        populate_wall_ms,
        total_encoded_bytes,
        avg_encoded_bytes,
        disk_bytes,
        encode_p50_ms: percentile(&encode_ms, 0.5),
        encode_p95_ms: percentile(&encode_ms, 0.95),
        read_decode_p50_ms: percentile(&read_decode_ms, 0.5),
        read_decode_p95_ms: percentile(&read_decode_ms, 0.95),
        read_decode_max_ms: read_decode_ms.last().copied().unwrap_or(0.0),
        ssim_mean,
        ssim_p5: percentile_opt(&ssim_scores, 0.05),
        ssim_min: ssim_scores.first().copied(),
    })
}

/// Decodes `encoded` back and scores it against `source` (the pre-encode resized image) with
/// `nicti_prowl::golden::ssim`. Runs after the timed encode call, never inside it -- SSIM is a
/// research-quality signal, not part of the latency numbers ADR-0017's comparison depends on.
fn score_ssim(
    codec_kind: Codec,
    encoded: &[u8],
    source: &decode::DecodedRgb,
) -> Result<f64, Box<dyn std::error::Error>> {
    let decoded = codec::decode(codec_kind, encoded)?;
    let source_img = to_rgb_image(source)?;
    let decoded_img = to_rgb_image(&decoded)?;
    if source_img.dimensions() != decoded_img.dimensions() {
        return Err(format!(
            "dimension mismatch: source {:?} vs decoded {:?}",
            source_img.dimensions(),
            decoded_img.dimensions()
        )
        .into());
    }
    Ok(nicti_prowl::golden::ssim(&source_img, &decoded_img))
}

/// T0: `nikon_preview_ifd`, read verbatim -- no decode/resize/re-encode. "Elapsed" here is just
/// the extraction cost (a single ranged read), not an encode; kept in the same
/// `(bytes, elapsed)` shape as `extract_and_encode` so `run`'s loop stays uniform, but its
/// `encode_p50/p95` fields should be read as "extraction latency" for this tier, not "encode
/// latency" -- there is no encode step.
fn extract_grid_verbatim(
    path: &Path,
) -> Result<(Vec<u8>, std::time::Duration), Box<dyn std::error::Error>> {
    let start = Instant::now();
    let source = FileSource::open(path, false)?;
    let mut walker = Walker::new(source)?;
    let jpegs = walker.find_embedded_jpegs()?;
    let target = decode::GRID_TIER_LONG_EDGE;
    let (off, len) = pick_candidate(&mut walker, &jpegs, Some(target)).ok_or("no embedded JPEG")?;
    let bytes = walker.read_range(off, len as usize)?;
    Ok((bytes, start.elapsed()))
}

/// Returns `(encoded bytes, encode-only elapsed, the resized pre-encode source)` -- the third
/// element is `Some` so `run`'s optional `--ssim` pass can score the encode against it without
/// re-decoding+re-resizing the original embedded JPEG a second time.
fn extract_and_encode(
    path: &Path,
    codec_kind: Codec,
    quality: u8,
    avif_speed: u8,
) -> ExtractResult {
    let source = FileSource::open(path, false)?;
    let mut walker = Walker::new(source)?;
    let jpegs = walker.find_embedded_jpegs()?;
    let target = decode::SCREEN_TIER_LONG_EDGE;
    let (off, len) = pick_candidate(&mut walker, &jpegs, Some(target)).ok_or("no embedded JPEG")?;
    let bytes = walker.read_range(off, len as usize)?;
    let decoded = decode::decode_jpeg(&bytes)?;
    let resized = decode::resize_to_long_edge(&decoded, target)?;

    let start = Instant::now();
    let bytes = codec::encode(codec_kind, &resized, quality, avif_speed)?;
    Ok((bytes, start.elapsed(), Some(resized)))
}
