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
use crate::ifd::Walker;
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
}

#[derive(Debug, Serialize)]
struct RunResult {
    mode: String,
    order: String,
    threads: usize,
    cold: bool,
    run_index: u32,
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
    use std::io::Read;
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_NO_BUFFERING;

    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_NO_BUFFERING)
        .open(path)?;
    let len = file.metadata()?.len() as usize;
    // NO_BUFFERING requires both the read length and the buffer's address to be aligned to the
    // volume's sector size; 4096 covers every sector size in real use (512e and 4Kn drives
    // alike), so round the read length up to the next 4096-byte boundary.
    const ALIGN: usize = 4096;
    let aligned_len = len.div_ceil(ALIGN) * ALIGN;
    let mut buf = AlignedBuf::new(aligned_len, ALIGN);
    let mut total = 0usize;
    loop {
        let n = file.read(&mut buf.as_mut_slice()[total..])?;
        if n == 0 {
            break;
        }
        total += n;
    }
    Ok(buf.into_trimmed_vec(len))
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

fn locate_offset(data: &[u8], target_long_edge: Option<u32>) -> Option<(u64, u64)> {
    let mut walker = Walker::new(data).ok()?;
    let jpegs = walker.find_embedded_jpegs().ok()?;
    if jpegs.is_empty() {
        return None;
    }
    // Prefer the smallest embedded JPEG whose *actual* long edge (from its own SOF header, not
    // the IFD's declared_width/height -- real Nikon NEF PreviewIFD/SubIFD entries carry no
    // ImageWidth/ImageLength tags at all, only DNG SubIFDs do, so trusting declared_width/height
    // here silently always fell through to the largest-byte_len candidate, i.e. always decoding
    // the full 45MP embedded JPEG regardless of the requested tier) meets the target; else the
    // largest overall.
    let mut candidates: Vec<_> = jpegs
        .iter()
        .filter_map(|j| {
            let start = j.file_offset as usize;
            let end = (j.file_offset + j.byte_len) as usize;
            let slice = data.get(start..end.min(data.len()))?;
            let header = crate::jpeg_meta::inspect(slice);
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

fn run_one(path: &Path, mode: Mode, cold: bool) -> Result<(), String> {
    let read_fn = if cold { read_cold } else { read_warm };

    match mode {
        Mode::FullRead => {
            read_fn(path).map_err(|e| e.to_string())?;
            Ok(())
        }
        Mode::Locate => {
            let data = read_fn(path).map_err(|e| e.to_string())?;
            locate_offset(&data, None).ok_or_else(|| "no embedded JPEG".to_string())?;
            Ok(())
        }
        Mode::Read => {
            let data = read_fn(path).map_err(|e| e.to_string())?;
            let (off, len) = locate_offset(&data, None).ok_or("no embedded JPEG")?;
            let start = off as usize;
            let end = (off + len) as usize;
            data.get(start..end.min(data.len()))
                .ok_or("offset out of bounds")?;
            Ok(())
        }
        Mode::DecodeGrid | Mode::DecodeScreen => {
            let data = read_fn(path).map_err(|e| e.to_string())?;
            let target = if mode == Mode::DecodeGrid {
                decode::GRID_TIER_LONG_EDGE
            } else {
                decode::SCREEN_TIER_LONG_EDGE
            };
            let (off, len) = locate_offset(&data, Some(target)).ok_or("no embedded JPEG")?;
            let start = off as usize;
            let end = (off + len) as usize;
            let slice = data
                .get(start..end.min(data.len()))
                .ok_or("offset out of bounds")?;
            let decoded = decode::decode_jpeg(slice)?;
            decode::resize_to_long_edge(&decoded, target)?;
            Ok(())
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    root: &Path,
    mode: Mode,
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
        let mut rng = rand::thread_rng();
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
                    let ok = run_one(f, mode, cold).is_ok();
                    SampleResult {
                        file: f.file_name().unwrap().to_string_lossy().to_string(),
                        micros: start.elapsed().as_micros(),
                        ok,
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
        let order_name = format!("{:?}", order).to_lowercase();
        let payload = RunResult {
            mode: mode_name.clone(),
            order: order_name.clone(),
            threads,
            cold,
            run_index,
            samples: results,
        };
        let fname = format!(
            "{}_{}_{}t_{}_run{}.json",
            mode_name,
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

        // Screen tier (2560px): neither candidate clears it, so the largest (640x424) wins.
        let (_offset, len) = locate_offset(&data, Some(decode::SCREEN_TIER_LONG_EDGE))
            .expect("falls back to the largest candidate");
        assert_eq!(len, large.len() as u64);
    }
}
