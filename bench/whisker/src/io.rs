//! Reads raw gray8 video (as produced by `ffmpeg -pix_fmt gray -f rawvideo`) into a
//! [`crate::FrameStream`]. See `bench/whisker/README.md` for the exact ffmpeg invocation.

use crate::FrameStream;
use std::fs;
use std::io;
use std::path::Path;

/// Reads a raw gray8 file into a [`FrameStream`]. `width`/`height` must match the ffmpeg crop
/// that produced the file. A trailing partial frame (a capture cut off mid-write) is dropped
/// rather than treated as an error, so an in-progress or slightly truncated capture still
/// analyzes the frames it does have.
pub fn read_frames_gray8(path: &Path, width: usize, height: usize) -> io::Result<FrameStream> {
    let frame_size = width * height;
    if frame_size == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "width and height must both be non-zero",
        ));
    }
    let bytes = fs::read(path)?;
    let n = bytes.len() / frame_size;
    let frames = (0..n)
        .map(|i| bytes[i * frame_size..(i + 1) * frame_size].to_vec())
        .collect();
    Ok(FrameStream {
        width,
        height,
        frames,
    })
}

/// One row of a mixed-sequence capture's `events.csv` sidecar, written by `hero.ahk`'s `mixed`
/// interaction (interaction D, #100) as it flashes the indicator. Row order matches
/// indicator-edge order 1:1 — `hero.ahk` appends one row per flash, in the same order it fires
/// them — so there's no separate lookup needed to know which flash a row describes; `edge` is a
/// self-check only (must equal the row's own 0-based position).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MixedEvent {
    pub edge: usize,
    /// What this specific flash measures (`switch`, `crop-enter`, `drag-start`, `drag-end`,
    /// `auto-tone`, `straighten`, or the trailing `end` bounding flash) — see
    /// `docs/benchmarks/hero-scenario.md`'s interaction D step table.
    pub kind: String,
    /// Which step token in the driven sequence this flash belongs to (e.g. `switch`, `crop`,
    /// `auto-tone`, `straighten`) — a step can emit several flashes of different `kind`s (a crop
    /// step emits `crop-enter`, `drag-start`, `drag-end`).
    pub step: String,
    /// 0-based index of this flash's step occurrence within the driven sequence. Several
    /// consecutive rows share one `step_index` when their step emits multiple flashes.
    pub step_index: usize,
}

/// Reads a `events.csv` sidecar (header `edge,kind,step,step_index`, one row per indicator
/// flash; the header line is optional). Row `i` (0-based, header excluded) must have `edge == i`
/// — `hero.ahk` assigns `edge` as a running counter as it flashes, so a mismatch means a
/// corrupted or hand-edited file, not a legitimate alternate ordering worth silently accepting.
pub fn read_events_csv(path: &Path) -> io::Result<Vec<MixedEvent>> {
    let content = fs::read_to_string(path)?;
    let mut out = Vec::new();
    let mut row = 0usize;
    for (line_no, raw_line) in content.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        if line_no == 0 && line.eq_ignore_ascii_case("edge,kind,step,step_index") {
            continue;
        }
        let parts: Vec<&str> = line.split(',').collect();
        if parts.len() != 4 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "events.csv line {}: expected 'edge,kind,step,step_index', got '{line}'",
                    line_no + 1
                ),
            ));
        }
        let edge: usize = parts[0].parse().map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("events.csv line {}: bad edge column: {e}", line_no + 1),
            )
        })?;
        if edge != row {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "events.csv line {}: edge {edge} does not match row position {row}",
                    line_no + 1
                ),
            ));
        }
        let step_index: usize = parts[3].parse().map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "events.csv line {}: bad step_index column: {e}",
                    line_no + 1
                ),
            )
        })?;
        out.push(MixedEvent {
            edge,
            kind: parts[1].to_string(),
            step: parts[2].to_string(),
            step_index,
        });
        row += 1;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn reads_whole_frames_and_drops_trailing_partial() {
        let path = unique_temp_path();
        // 2 full 2x2 (4-byte) frames plus 2 stray trailing bytes.
        fs::File::create(&path)
            .unwrap()
            .write_all(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 9])
            .unwrap();
        let stream = read_frames_gray8(&path, 2, 2).unwrap();
        fs::remove_file(&path).unwrap();
        assert_eq!(stream.frames.len(), 2);
        assert_eq!(stream.frames[0], vec![1, 2, 3, 4]);
        assert_eq!(stream.frames[1], vec![5, 6, 7, 8]);
    }

    #[test]
    fn zero_dims_is_error() {
        let path = unique_temp_path();
        fs::File::create(&path).unwrap();
        let result = read_frames_gray8(&path, 0, 10);
        fs::remove_file(&path).unwrap();
        assert!(result.is_err());
    }

    // Minimal same-process tempfile helper so this crate doesn't need a `tempfile` dependency
    // just for two tests. Uses an atomic counter (not just PID) so parallel test threads never
    // collide on the same path.
    fn unique_temp_path() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("whisker-io-test-{}-{}", std::process::id(), n))
    }

    #[test]
    fn read_events_csv_parses_rows_and_skips_optional_header() {
        let path = unique_temp_path();
        fs::write(
            &path,
            "edge,kind,step,step_index\n0,switch,switch,0\n1,crop-enter,crop,1\n2,drag-start,crop,1\n",
        )
        .unwrap();
        let events = read_events_csv(&path).unwrap();
        fs::remove_file(&path).unwrap();
        assert_eq!(
            events,
            vec![
                MixedEvent {
                    edge: 0,
                    kind: "switch".to_string(),
                    step: "switch".to_string(),
                    step_index: 0,
                },
                MixedEvent {
                    edge: 1,
                    kind: "crop-enter".to_string(),
                    step: "crop".to_string(),
                    step_index: 1,
                },
                MixedEvent {
                    edge: 2,
                    kind: "drag-start".to_string(),
                    step: "crop".to_string(),
                    step_index: 1,
                },
            ]
        );
    }

    #[test]
    fn read_events_csv_without_header_still_parses() {
        let path = unique_temp_path();
        fs::write(&path, "0,switch,switch,0\n1,auto-tone,auto-tone,1\n").unwrap();
        let events = read_events_csv(&path).unwrap();
        fs::remove_file(&path).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].kind, "auto-tone");
    }

    #[test]
    fn read_events_csv_edge_mismatch_is_an_error() {
        let path = unique_temp_path();
        // Row position 1 claims edge 5 -- a corrupted/hand-edited file, not a valid reorder.
        fs::write(&path, "0,switch,switch,0\n5,switch,switch,1\n").unwrap();
        let result = read_events_csv(&path);
        fs::remove_file(&path).unwrap();
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("does not match row position"));
    }

    #[test]
    fn read_events_csv_wrong_column_count_is_an_error() {
        let path = unique_temp_path();
        fs::write(&path, "0,switch,switch\n").unwrap();
        let result = read_events_csv(&path);
        fs::remove_file(&path).unwrap();
        assert!(result.is_err());
    }
}
