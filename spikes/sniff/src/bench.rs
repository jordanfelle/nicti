//! `sniff bench <root> --mode ... --threads N --order {manifest,random} [--cold]`: measures
//! per-file latency for locating, reading, and decoding embedded JPEGs, and writes raw JSON
//! results under `bench-results/sniff/` (gitignored) for later pooling into p50/p95/max, per
//! `docs/benchmarks.md`'s methodology (1 discarded warm-up run, then 5 measured runs).
//!
//! Cold reads bypass the OS page/file cache via `FILE_FLAG_NO_BUFFERING` on Windows (sector-
//! aligned reads, no RAMMap dependency). On non-Windows this flag doesn't exist, so `--cold` here
//! is a best-effort request only -- the numbers that matter for `docs/benchmarks.md` come from
//! the Windows-native reference-machine run, not from WSL.

use crate::decode;
use crate::ifd::{EmbeddedJpeg, Walker};
use crate::source::{ByteSource, FileSource, SliceSource};
use rand::seq::SliceRandom;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Debug, Clone, Copy, clap::ValueEnum, PartialEq, Eq)]
pub enum Mode {
    Locate,
    Read,
    DecodeGrid,
    DecodeScreen,
    FullRead,
    /// Walks the file once and collects every embedded-JPEG's (offset, len) -- simulates the
    /// per-tier index an ingest pass would record so later reads never re-walk the IFD tree.
    /// Only meaningful with `--io ranged` (see `IoMode`); under `--io whole` it still runs, just
    /// without the whole-file-read-avoidance the mode exists to measure.
    ExtractIndex,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum, PartialEq, Eq)]
pub enum IoMode {
    /// `fs::read` the whole file first, then locate/decode within the in-memory buffer --
    /// the pessimistic upper bound `docs/research/sniff-embedded-jpeg.md` originally measured.
    Whole,
    /// Positioned reads only (`source::FileSource`): the IFD walk and header inspection touch
    /// only the small windows they need, and only the target embedded JPEG's own byte range is
    /// read in full (for decode modes) -- never the rest of the file.
    Ranged,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum Order {
    Manifest,
    Random,
}

#[derive(Debug, Serialize)]
struct SampleResult {
    file: String,
    micros: u128,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct RunResult {
    mode: String,
    io: String,
    order: String,
    threads: usize,
    cold: bool,
    run_index: u32,
    /// The sample root this run actually read from -- added after a real incident: a sweep
    /// script reused one output directory for both an NVMe and an HDD config sharing the same
    /// (mode, io, order, cold, threads) filename, and the second run's write silently clobbered
    /// the first's, undetected until the pooled numbers didn't match physical expectations. This
    /// field alone doesn't prevent a bad output-dir choice, but it lets a mismatch be caught by
    /// inspecting the JSON itself, and lets multiple roots safely share one output directory.
    root: String,
    /// Host identity fields `docs/research/sniff-embedded-jpeg.md` flagged as missing from this
    /// JSON (recorded by hand instead, in that doc's Throughput section). This covers what's
    /// cheaply knowable from inside the process; CPU/GPU/driver/RAM/drive-model identity still
    /// needs to be recorded by hand per `docs/benchmarks.md`'s methodology.
    os: String,
    arch: String,
    hostname: String,
    samples: Vec<SampleResult>,
}

/// A sector-aligned owned buffer for `FILE_FLAG_NO_BUFFERING` reads. Deliberately not a plain
/// `Vec<u8>`: `Vec<T>`'s allocator contract assumes `align_of::<T>()` (1, for `u8`), so
/// allocating with a larger alignment and later letting `Vec`'s own `Drop` run would deallocate
/// with a mismatched layout -- undefined behavior. This type owns the allocation and matches the
/// `Layout` used for `alloc_zeroed` exactly in its own `Drop`.
#[cfg(windows)]
struct AlignedBuf {
    ptr: *mut u8,
    len: usize,
    layout: std::alloc::Layout,
}

#[cfg(windows)]
impl AlignedBuf {
    fn new(len: usize, align: usize) -> Self {
        let alloc_len = len.max(align);
        let layout = std::alloc::Layout::from_size_align(alloc_len, align).expect("valid layout");
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        if ptr.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        AlignedBuf { ptr, len, layout }
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.layout.size()) }
    }

    fn into_trimmed_vec(self, actual_len: usize) -> Vec<u8> {
        let slice = unsafe { std::slice::from_raw_parts(self.ptr, actual_len.min(self.len)) };
        slice.to_vec()
    }
}

#[cfg(windows)]
impl Drop for AlignedBuf {
    fn drop(&mut self) {
        unsafe { std::alloc::dealloc(self.ptr, self.layout) }
    }
}

#[cfg(windows)]
fn read_cold(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_NO_BUFFERING;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_NO_BUFFERING)
        .open(path)?;
    let len = file.metadata()?.len();
    read_cold_range(&file, 0, len as usize)
}

/// Sector-aligned, cache-bypassing ranged read: `offset`/`len` need not themselves be
/// sector-aligned, only the underlying request is (rounded up/out to the nearest 4096-byte
/// boundary, then trimmed back to the caller's exact window). Shared by `read_cold` above (the
/// offset-0/whole-file case) and `source::FileSource`'s ranged cold path -- both need the same
/// `FILE_FLAG_NO_BUFFERING` handle, just opened once per call site.
///
/// Note: `file` here must already have been opened with `FILE_FLAG_NO_BUFFERING` (see
/// `read_cold` above and `source::FileSource::open`'s Windows cold path) -- this function only
/// handles the sector-alignment math, not the flag itself.
#[cfg(windows)]
pub(crate) fn read_cold_range(
    file: &std::fs::File,
    offset: u64,
    len: usize,
) -> std::io::Result<Vec<u8>> {
    use std::os::windows::fs::FileExt;

    const ALIGN: u64 = 4096;
    // NO_BUFFERING requires the read length, the buffer's address, AND the file offset to be
    // aligned to the volume's sector size -- round the window out to the enclosing aligned range
    // rather than just the length, and remember how far the caller's real start is into it.
    let aligned_offset = (offset / ALIGN) * ALIGN;
    let front_pad = (offset - aligned_offset) as usize;
    let content_end = front_pad + len;
    let aligned_len = (content_end as u64).div_ceil(ALIGN) * ALIGN;
    let mut buf = AlignedBuf::new(aligned_len as usize, ALIGN as usize);
    // A single seek_read of the whole aligned window, not a total-tracking loop: if the OS ever
    // returns fewer bytes than requested (observed on a slower drive -- confirmed via
    // `os error 87`, ERROR_INVALID_PARAMETER), resuming from `buf.as_mut_slice()[total..]` passes
    // a buffer address of `base_ptr + total` to the next call, which is only guaranteed
    // sector-aligned if `total` itself is a multiple of ALIGN -- not guaranteed by a short read in
    // general (the explicit `aligned_offset + total` this loop passed as the *file offset* was
    // itself fine, since `seek_read` doesn't rely on an implicit cursor -- the buffer-address
    // side was the actual violation). Retrying the identical seek_read call (same aligned_offset,
    // same full aligned_len) on the same handle needs no reopen -- `seek_read` is positional, not
    // cursor-based -- so every attempt's buffer address (the allocation's own base) and file
    // offset (`aligned_offset`, fixed) stay aligned by construction.
    let mut n = file.seek_read(buf.as_mut_slice(), aligned_offset)?;
    if n < content_end {
        n = file.seek_read(buf.as_mut_slice(), aligned_offset)?;
    }
    if n < content_end {
        return Err(std::io::Error::other(format!(
            "short read after retry: got {n} of {content_end} bytes (offset {offset}, len {len})"
        )));
    }
    let trimmed = buf.into_trimmed_vec(n);
    let start = front_pad.min(trimmed.len());
    let end = (start + len).min(trimmed.len());
    Ok(trimmed[start..end].to_vec())
}

#[cfg(not(windows))]
fn read_cold(path: &Path) -> std::io::Result<Vec<u8>> {
    // No NO_BUFFERING-equivalent used here; matches docs/benchmarks.md's own rule that cold
    // numbers only come from the Windows-native reference-machine run, not WSL.
    std::fs::read(path)
}

fn read_warm(path: &Path) -> std::io::Result<Vec<u8>> {
    std::fs::read(path)
}

/// Only the first `HEADER_INSPECT_LEN` bytes of each candidate are read to inspect its SOF/DQT
/// header -- comfortably past where SOF appears in every real file this spike has seen (APPn/DQT/
/// SOF/DHT/SOS, all metadata, well before any entropy-coded scan data), and tiny next to the
/// multi-MB `JpgFromRaw` candidate a whole-file read would otherwise pay for just to measure its
/// dimensions.
const HEADER_INSPECT_LEN: usize = 65536;

/// Picks the smallest embedded JPEG whose *actual* long edge (from its own SOF header, not the
/// IFD's declared_width/height -- real Nikon NEF PreviewIFD/SubIFD entries carry no
/// ImageWidth/ImageLength tags at all, only DNG SubIFDs do, so trusting declared_width/height
/// here silently always fell through to the largest-byte_len candidate, i.e. always decoding the
/// full 45MP embedded JPEG regardless of the requested tier) meets `target_long_edge`; else the
/// largest overall. Reads only a bounded header prefix per candidate via `walker`, not each
/// candidate's full bytes.
pub(crate) fn pick_candidate<S: ByteSource>(
    walker: &mut Walker<S>,
    jpegs: &[EmbeddedJpeg],
    target_long_edge: Option<u32>,
) -> Option<(u64, u64)> {
    let mut candidates: Vec<_> = jpegs
        .iter()
        .filter_map(|j| {
            let prefix_len = HEADER_INSPECT_LEN.min(j.byte_len as usize);
            let prefix = walker.read_range(j.file_offset, prefix_len).ok()?;
            let header = crate::jpeg_meta::inspect(&prefix);
            let long_edge = header
                .width
                .zip(header.height)
                .map(|(w, h)| w.max(h) as u32)?;
            Some((j, long_edge))
        })
        .collect();
    candidates.sort_by_key(|(j, _)| j.byte_len);

    if let Some(target) = target_long_edge {
        candidates
            .iter()
            .find(|(_, long_edge)| *long_edge >= target)
            .or_else(|| candidates.last())
            .map(|(j, _)| (j.file_offset, j.byte_len))
    } else {
        candidates.last().map(|(j, _)| (j.file_offset, j.byte_len))
    }
}

fn locate_offset(data: &[u8], target_long_edge: Option<u32>) -> Option<(u64, u64)> {
    let mut walker = Walker::new(SliceSource::new(data)).ok()?;
    let jpegs = walker.find_embedded_jpegs().ok()?;
    if jpegs.is_empty() {
        return None;
    }
    pick_candidate(&mut walker, &jpegs, target_long_edge)
}

fn run_one(path: &Path, mode: Mode, io: IoMode, cold: bool) -> Result<(), String> {
    if let Mode::FullRead = mode {
        let read_fn = if cold { read_cold } else { read_warm };
        read_fn(path).map_err(|e| e.to_string())?;
        return Ok(());
    }
    if let Mode::ExtractIndex = mode {
        // Ranged by construction regardless of `io`: the whole point of this mode is the
        // ingest-index-build cost, which `FileSource` always serves via positioned reads.
        let source = FileSource::open(path, cold).map_err(|e| e.to_string())?;
        let mut walker = Walker::new(source).map_err(|e| e.to_string())?;
        walker.find_embedded_jpegs().map_err(|e| e.to_string())?;
        return Ok(());
    }

    match io {
        IoMode::Whole => {
            let read_fn = if cold { read_cold } else { read_warm };
            let data = read_fn(path).map_err(|e| e.to_string())?;
            match mode {
                Mode::Locate => {
                    locate_offset(&data, None).ok_or("no embedded JPEG")?;
                }
                Mode::Read => {
                    let (off, len) = locate_offset(&data, None).ok_or("no embedded JPEG")?;
                    let start = off as usize;
                    let end = (off + len) as usize;
                    data.get(start..end.min(data.len()))
                        .ok_or("offset out of bounds")?;
                }
                Mode::DecodeGrid | Mode::DecodeScreen => {
                    let target = if mode == Mode::DecodeGrid {
                        decode::GRID_TIER_LONG_EDGE
                    } else {
                        decode::SCREEN_TIER_LONG_EDGE
                    };
                    let (off, len) =
                        locate_offset(&data, Some(target)).ok_or("no embedded JPEG")?;
                    let start = off as usize;
                    let end = (off + len) as usize;
                    let slice = data
                        .get(start..end.min(data.len()))
                        .ok_or("offset out of bounds")?;
                    let decoded = decode::decode_jpeg(slice)?;
                    decode::resize_to_long_edge(&decoded, target)?;
                }
                Mode::FullRead | Mode::ExtractIndex => unreachable!("handled above"),
            }
            Ok(())
        }
        IoMode::Ranged => {
            let source = FileSource::open(path, cold).map_err(|e| e.to_string())?;
            let mut walker = Walker::new(source).map_err(|e| e.to_string())?;
            let jpegs = walker.find_embedded_jpegs().map_err(|e| e.to_string())?;
            match mode {
                Mode::Locate => {
                    pick_candidate(&mut walker, &jpegs, None).ok_or("no embedded JPEG")?;
                }
                Mode::Read => {
                    let (off, len) =
                        pick_candidate(&mut walker, &jpegs, None).ok_or("no embedded JPEG")?;
                    walker
                        .read_range(off, len as usize)
                        .map_err(|e| e.to_string())?;
                }
                Mode::DecodeGrid | Mode::DecodeScreen => {
                    let target = if mode == Mode::DecodeGrid {
                        decode::GRID_TIER_LONG_EDGE
                    } else {
                        decode::SCREEN_TIER_LONG_EDGE
                    };
                    let (off, len) = pick_candidate(&mut walker, &jpegs, Some(target))
                        .ok_or("no embedded JPEG")?;
                    let bytes = walker
                        .read_range(off, len as usize)
                        .map_err(|e| e.to_string())?;
                    let decoded = decode::decode_jpeg(&bytes)?;
                    decode::resize_to_long_edge(&decoded, target)?;
                }
                Mode::FullRead | Mode::ExtractIndex => unreachable!("handled above"),
            }
            Ok(())
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    root: &Path,
    mode: Mode,
    io: IoMode,
    threads: usize,
    order: Order,
    cold: bool,
    warmups: u32,
    runs: u32,
    sample_limit: Option<usize>,
    out_dir: &Path,
) -> std::io::Result<()> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(root)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .map(|e| {
                    let e = e.to_string_lossy().to_lowercase();
                    e == "nef" || e == "dng"
                })
                .unwrap_or(false)
        })
        .collect();
    files.sort();

    if let Order::Random = order {
        let mut rng = rand::rng();
        files.shuffle(&mut rng);
    }
    if let Some(limit) = sample_limit {
        files.truncate(limit);
    }

    std::fs::create_dir_all(out_dir)?;

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads.max(1))
        .build()
        .map_err(std::io::Error::other)?;

    let total_rounds = warmups + runs;
    for round in 0..total_rounds {
        let is_warmup = round < warmups;
        let results: Vec<SampleResult> = pool.install(|| {
            use rayon::prelude::*;
            files
                .par_iter()
                .map(|f| {
                    let start = Instant::now();
                    let result = run_one(f, mode, io, cold);
                    let ok = result.is_ok();
                    let error = result.err();
                    SampleResult {
                        file: f.file_name().unwrap().to_string_lossy().to_string(),
                        micros: start.elapsed().as_micros(),
                        ok,
                        error,
                    }
                })
                .collect()
        });

        if is_warmup {
            eprintln!("warm-up round {} done ({} files)", round + 1, results.len());
            continue;
        }

        let run_index = round - warmups;
        let mode_name = format!("{:?}", mode).to_lowercase();
        let io_name = format!("{:?}", io).to_lowercase();
        let order_name = format!("{:?}", order).to_lowercase();
        let payload = RunResult {
            mode: mode_name.clone(),
            io: io_name.clone(),
            order: order_name.clone(),
            threads,
            cold,
            run_index,
            root: root.display().to_string(),
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            hostname: std::env::var("COMPUTERNAME")
                .or_else(|_| std::env::var("HOSTNAME"))
                .unwrap_or_else(|_| "unknown".to_string()),
            samples: results,
        };
        let fname = format!(
            "{}_{}_{}_{}t_{}_run{}.json",
            mode_name,
            io_name,
            order_name,
            threads,
            if cold { "cold" } else { "warm" },
            run_index,
        );
        let path = out_dir.join(fname);
        let f = std::fs::File::create(&path)?;
        serde_json::to_writer_pretty(f, &payload).map_err(std::io::Error::other)?;
        eprintln!("wrote {}", path.display());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal SOI+SOF0+EOI JPEG with the given dimensions -- enough for `jpeg_meta::inspect`
    /// to read real width/height, without needing a decodable entropy-coded payload (`locate_offset`
    /// never decodes these, only inspects headers).
    fn fake_jpeg(width: u16, height: u16, padding: usize) -> Vec<u8> {
        let mut j = vec![0xFF, 0xD8];
        let sof = [
            8,
            (height >> 8) as u8,
            height as u8,
            (width >> 8) as u8,
            width as u8,
            1,
            1,
            0x11,
            0,
        ];
        j.extend_from_slice(&[0xFF, 0xC0]);
        j.extend_from_slice(&((sof.len() + 2) as u16).to_be_bytes());
        j.extend_from_slice(&sof);
        j.extend(std::iter::repeat_n(0u8, padding));
        j.extend_from_slice(&[0xFF, 0xD9]);
        j
    }

    /// A minimal little-endian TIFF file with two embedded JPEGs at IFD0 and a "next" thumbnail
    /// IFD, neither carrying ImageWidth/ImageLength TIFF tags -- exactly the shape real Nikon
    /// NEF PreviewIFD/SubIFD entries have (see ifd.rs's own doc comment on this). Regression
    /// fixture for the bug where `locate_offset` trusted `declared_width`/`declared_height`
    /// (always absent here) instead of each JPEG's own SOF header, and so always picked the
    /// largest candidate regardless of the requested tier.
    fn build_two_jpeg_file(small: &[u8], large: &[u8]) -> Vec<u8> {
        let mut data = vec![b'I', b'I', 42, 0, 0, 0, 0, 0];
        let small_off = data.len() as u32;
        data.extend_from_slice(small);
        let large_off = data.len() as u32;
        data.extend_from_slice(large);

        // IFD1 (thumbnail chain): the small JPEG, no width/height tags.
        let ifd1_off = data.len() as u32;
        data.extend_from_slice(&2u16.to_le_bytes());
        data.extend_from_slice(&TAG_JPEG_IF_OFFSET.to_le_bytes());
        data.extend_from_slice(&4u16.to_le_bytes());
        data.extend_from_slice(&1u32.to_le_bytes());
        data.extend_from_slice(&small_off.to_le_bytes());
        data.extend_from_slice(&TAG_JPEG_IF_LENGTH.to_le_bytes());
        data.extend_from_slice(&4u16.to_le_bytes());
        data.extend_from_slice(&1u32.to_le_bytes());
        data.extend_from_slice(&(small.len() as u32).to_le_bytes());
        data.extend_from_slice(&0u32.to_le_bytes()); // next

        // IFD0: the large JPEG, no width/height tags, next -> IFD1.
        let ifd0_off = data.len() as u32;
        data.extend_from_slice(&2u16.to_le_bytes());
        data.extend_from_slice(&TAG_JPEG_IF_OFFSET.to_le_bytes());
        data.extend_from_slice(&4u16.to_le_bytes());
        data.extend_from_slice(&1u32.to_le_bytes());
        data.extend_from_slice(&large_off.to_le_bytes());
        data.extend_from_slice(&TAG_JPEG_IF_LENGTH.to_le_bytes());
        data.extend_from_slice(&4u16.to_le_bytes());
        data.extend_from_slice(&1u32.to_le_bytes());
        data.extend_from_slice(&(large.len() as u32).to_le_bytes());
        data.extend_from_slice(&ifd1_off.to_le_bytes()); // next -> IFD1

        data[4..8].copy_from_slice(&ifd0_off.to_le_bytes());
        data
    }

    use crate::ifd::{TAG_JPEG_IF_LENGTH, TAG_JPEG_IF_OFFSET};

    #[test]
    fn picks_smallest_candidate_meeting_the_tier_even_without_declared_dimensions() {
        // Small JPEG is padded so it's still the smaller of the two by byte_len, same as real
        // Nikon files (the small preview is both lower-resolution and fewer bytes than the
        // full-size embedded JPEG).
        let small = fake_jpeg(640, 424, 100);
        let large = fake_jpeg(8256, 5504, 5000);
        assert!(small.len() < large.len());
        let data = build_two_jpeg_file(&small, &large);

        // decode-grid tier (512px): the 640x424 preview clears it, so it should be picked --
        // not the 8256x5504 full-size image the pre-fix code always fell back to.
        let (offset, len) =
            locate_offset(&data, Some(decode::GRID_TIER_LONG_EDGE)).expect("finds a candidate");
        assert_eq!(len, small.len() as u64);
        let picked = &data[offset as usize..(offset + len) as usize];
        assert_eq!(picked, small.as_slice());
    }

    #[test]
    fn falls_back_to_largest_when_no_candidate_meets_the_tier() {
        let small = fake_jpeg(160, 120, 10);
        let large = fake_jpeg(640, 424, 100);
        let data = build_two_jpeg_file(&small, &large);

        // Screen tier (3840px): neither candidate clears it, so the largest (640x424) wins.
        let (_offset, len) = locate_offset(&data, Some(decode::SCREEN_TIER_LONG_EDGE))
            .expect("falls back to the largest candidate");
        assert_eq!(len, large.len() as u64);
    }
}
