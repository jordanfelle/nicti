//! Reads and writes the XMP packet embedded inside a JPEG's APP1 segment --
//! the format ~72% of the reference library's real files use
//! (`docs/research/shed-lrcat-schema.md`'s JPEG count), where LRC writes
//! metadata *into the original file* rather than a `.xmp` sidecar.
//!
//! **DNG/TIFF tag-700 embedded XMP is explicitly out of scope for this
//! pass** -- see the research doc's "What wasn't reachable" section. A DNG
//! rewrite is a full TIFF-IFD-rewrite problem (unlike JPEG's flat segment
//! list, TIFF's IFD offsets are absolute file positions that a naive splice
//! would corrupt), and ADR-0020 already treats any DNG content change as an
//! identity change regardless of how it's produced. Reading tag 700 could
//! reuse `spikes/sniff`'s TIFF/IFD walker; writing needs a dedicated TIFF
//! writer this spike didn't build.
//!
//! JPEG segment format (ITU-T T.81 B.1.1.3): `0xFF <marker> [<u16 len,
//! big-endian, includes itself> <len-2 bytes payload>]`. `SOI`/`EOI`/`RST0-7`
//! carry no length; the scan (`SOS`, 0xFFDA) is followed by entropy-coded
//! data with no further segment structure, so this walker stops there --
//! XMP always lives in the header, before the first scan.

use thiserror::Error;

const XMP_SIGNATURE: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
const APP0: u8 = 0xE0;
const APP1: u8 = 0xE1;
const SOS: u8 = 0xDA;
const SOI: u8 = 0xD8;
const JFIF_SIGNATURE: &[u8] = b"JFIF\0";
/// Segment length field is a u16 including itself -- max payload is
/// 65535 - 2 = 65533 bytes.
const MAX_SEGMENT_PAYLOAD: usize = 65533;

#[derive(Debug, Error)]
pub enum JpegXmpError {
    #[error("not a JPEG file (missing SOI marker)")]
    NotAJpeg,
    #[error("truncated JPEG: segment header runs past end of file")]
    Truncated,
    #[error(
        "xmp packet ({actual} bytes) plus signature exceeds the {MAX_SEGMENT_PAYLOAD}-byte \
         APP1 segment limit -- would need JPEG's multi-segment extension, not implemented here"
    )]
    TooLarge { actual: usize },
}

struct Segment {
    marker: u8,
    /// Byte offset of the 0xFF marker byte itself.
    start: usize,
    /// Byte offset one past this segment's payload (or past the marker for
    /// a length-less marker) -- i.e. where the next segment begins.
    end: usize,
    /// `(payload_start, payload_end)` within the file, if this marker has one.
    payload: Option<(usize, usize)>,
}

fn walk_segments(data: &[u8]) -> Result<Vec<Segment>, JpegXmpError> {
    if data.len() < 2 || data[0] != 0xFF || data[1] != SOI {
        return Err(JpegXmpError::NotAJpeg);
    }
    let mut segments = vec![Segment {
        marker: SOI,
        start: 0,
        end: 2,
        payload: None,
    }];
    let mut pos = 2;
    loop {
        if pos >= data.len() {
            break;
        }
        if data[pos] != 0xFF {
            // Entropy-coded data or trailing garbage after SOS -- stop.
            break;
        }
        let marker_start = pos;
        pos += 1;
        // Skip 0xFF fill bytes before the real marker code.
        while pos < data.len() && data[pos] == 0xFF {
            pos += 1;
        }
        if pos >= data.len() {
            return Err(JpegXmpError::Truncated);
        }
        let marker = data[pos];
        pos += 1;

        let is_standalone = marker == 0x01 || (0xD0..=0xD9).contains(&marker);
        if is_standalone {
            segments.push(Segment {
                marker,
                start: marker_start,
                end: pos,
                payload: None,
            });
            if marker == SOS {
                break;
            }
            continue;
        }

        if pos + 2 > data.len() {
            return Err(JpegXmpError::Truncated);
        }
        let len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
        if len < 2 || pos + len > data.len() {
            return Err(JpegXmpError::Truncated);
        }
        let payload_start = pos + 2;
        let payload_end = pos + len;
        segments.push(Segment {
            marker,
            start: marker_start,
            end: payload_end,
            payload: Some((payload_start, payload_end)),
        });
        pos = payload_end;
        if marker == SOS {
            break;
        }
    }
    Ok(segments)
}

/// Returns the raw XMP packet text embedded in this JPEG's APP1 segment, if
/// present.
pub fn read_xmp(data: &[u8]) -> Result<Option<String>, JpegXmpError> {
    for seg in walk_segments(data)? {
        if seg.marker != APP1 {
            continue;
        }
        let Some((start, end)) = seg.payload else {
            continue;
        };
        let payload = &data[start..end];
        if payload.starts_with(XMP_SIGNATURE) {
            let xmp_bytes = &payload[XMP_SIGNATURE.len()..];
            return Ok(Some(String::from_utf8_lossy(xmp_bytes).into_owned()));
        }
    }
    Ok(None)
}

/// Byte ranges of the existing APP1-XMP segment, if any -- exposed so the
/// byte-diff guard can prove everything outside this span is untouched by a
/// write, without re-deriving the same walk twice.
fn find_xmp_segment(data: &[u8]) -> Result<Option<(usize, usize)>, JpegXmpError> {
    for seg in walk_segments(data)? {
        if seg.marker != APP1 {
            continue;
        }
        if let Some((p_start, p_end)) = seg.payload {
            if data[p_start..p_end].starts_with(XMP_SIGNATURE) {
                return Ok(Some((seg.start, seg.end)));
            }
        }
    }
    Ok(None)
}

/// Replaces (or inserts, if none exists yet) the APP1-XMP segment with
/// `new_xmp`, returning the whole rewritten file. Every byte outside the old
/// and new segment spans is copied verbatim -- proven in this module's
/// `write_preserves_bytes_outside_the_segment` test, which independently
/// re-derives the untouched prefix/suffix and asserts byte equality against
/// the input.
pub fn write_xmp(data: &[u8], new_xmp: &str) -> Result<Vec<u8>, JpegXmpError> {
    let payload_len = XMP_SIGNATURE.len() + new_xmp.len();
    if payload_len > MAX_SEGMENT_PAYLOAD {
        return Err(JpegXmpError::TooLarge {
            actual: payload_len,
        });
    }
    let seg_len = payload_len + 2; // + the length field itself
    let mut new_segment = Vec::with_capacity(2 + 2 + payload_len);
    new_segment.push(0xFF);
    new_segment.push(APP1);
    new_segment.extend_from_slice(&(seg_len as u16).to_be_bytes());
    new_segment.extend_from_slice(XMP_SIGNATURE);
    new_segment.extend_from_slice(new_xmp.as_bytes());

    let existing = find_xmp_segment(data)?;
    let mut out = Vec::with_capacity(data.len() + new_segment.len());
    match existing {
        Some((start, end)) => {
            out.extend_from_slice(&data[..start]);
            out.extend_from_slice(&new_segment);
            out.extend_from_slice(&data[end..]);
        }
        None => {
            // No existing XMP -- insert right after SOI, before any other
            // segment, *unless* a JFIF APP0 segment is already there. JFIF
            // requires its own APP0 to be the very first marker after SOI;
            // inserting APP1 ahead of it would leave a reader that checks
            // for JFIF's exact prescribed marker order failing to recognize
            // the file as JFIF. Any other segment (EXIF APP1, etc.) has no
            // such positional requirement, so SOI-adjacent insertion is
            // still correct for every other case.
            if data.len() < 2 {
                return Err(JpegXmpError::NotAJpeg);
            }
            let segments = walk_segments(data)?;
            let insert_at = match segments.get(1) {
                Some(seg)
                    if seg.marker == APP0
                        && seg
                            .payload
                            .is_some_and(|(s, e)| data[s..e].starts_with(JFIF_SIGNATURE)) =>
                {
                    seg.end
                }
                _ => 2,
            };
            out.extend_from_slice(&data[..insert_at]);
            out.extend_from_slice(&new_segment);
            out.extend_from_slice(&data[insert_at..]);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_jpeg_with_app1(payload: &[u8]) -> Vec<u8> {
        let mut data = vec![0xFF, SOI];
        data.push(0xFF);
        data.push(APP1);
        let len = (payload.len() + 2) as u16;
        data.extend_from_slice(&len.to_be_bytes());
        data.extend_from_slice(payload);
        // A trailing marker + SOS + one byte of "scan data" + EOI so the
        // walker has something realistic after the APP1 segment.
        data.extend_from_slice(&[0xFF, SOS, 0x00, 0x02, 0x00, 0x00]);
        data.push(0xAB); // fake entropy-coded byte
        data.extend_from_slice(&[0xFF, 0xD9]); // EOI
        data
    }

    #[test]
    fn reads_xmp_from_app1() {
        let mut payload = XMP_SIGNATURE.to_vec();
        payload.extend_from_slice(b"<x:xmpmeta/>");
        let jpeg = minimal_jpeg_with_app1(&payload);
        let xmp = read_xmp(&jpeg).unwrap();
        assert_eq!(xmp.as_deref(), Some("<x:xmpmeta/>"));
    }

    #[test]
    fn non_xmp_app1_segments_are_ignored() {
        // e.g. a Photoshop IRB APP1 segment, different signature.
        let jpeg = minimal_jpeg_with_app1(b"Photoshop 3.0\0garbage");
        assert_eq!(read_xmp(&jpeg).unwrap(), None);
    }

    #[test]
    fn write_preserves_bytes_outside_the_segment() {
        let mut payload = XMP_SIGNATURE.to_vec();
        payload.extend_from_slice(b"<old/>");
        let original = minimal_jpeg_with_app1(&payload);

        let (seg_start, seg_end) = find_xmp_segment(&original).unwrap().unwrap();
        let prefix = original[..seg_start].to_vec();
        let suffix = original[seg_end..].to_vec();

        let rewritten =
            write_xmp(&original, "<x:xmpmeta>much longer than before</x:xmpmeta>").unwrap();

        assert!(rewritten.starts_with(&prefix), "prefix bytes changed");
        assert!(rewritten.ends_with(&suffix), "suffix bytes changed");
        assert_eq!(
            read_xmp(&rewritten).unwrap().as_deref(),
            Some("<x:xmpmeta>much longer than before</x:xmpmeta>")
        );
    }

    #[test]
    fn insert_when_no_existing_segment() {
        let mut data = vec![0xFF, SOI];
        data.extend_from_slice(&[0xFF, SOS, 0x00, 0x02, 0x00, 0x00]);
        data.push(0xAB);
        data.extend_from_slice(&[0xFF, 0xD9]);

        let rewritten = write_xmp(&data, "<x:xmpmeta/>").unwrap();
        assert_eq!(
            read_xmp(&rewritten).unwrap().as_deref(),
            Some("<x:xmpmeta/>")
        );
        // Original bytes still present, just shifted.
        assert!(rewritten.ends_with(&data[2..]));
    }

    #[test]
    fn insert_when_no_existing_xmp_goes_after_a_jfif_app0_segment() {
        // JFIF requires its own APP0 to be the very first marker after SOI
        // -- inserting a new APP1 ahead of it would break that requirement.
        let mut data = vec![0xFF, SOI];
        // APP0 "JFIF\0" + version/density placeholder bytes (9-byte payload
        // is JFIF's real minimum; content beyond the signature doesn't
        // matter for this test).
        let jfif_payload = [b'J', b'F', b'I', b'F', 0, 1, 2, 0, 0, 0, 0, 0, 0];
        data.push(0xFF);
        data.push(APP0);
        data.extend_from_slice(&((jfif_payload.len() + 2) as u16).to_be_bytes());
        data.extend_from_slice(&jfif_payload);
        data.extend_from_slice(&[0xFF, SOS, 0x00, 0x02, 0x00, 0x00]);
        data.push(0xAB);
        data.extend_from_slice(&[0xFF, 0xD9]);

        let rewritten = write_xmp(&data, "<x:xmpmeta/>").unwrap();
        assert_eq!(
            read_xmp(&rewritten).unwrap().as_deref(),
            Some("<x:xmpmeta/>")
        );
        // The APP0/JFIF segment must still be the very first marker after
        // SOI -- not pushed behind the newly inserted APP1.
        let segments = walk_segments(&rewritten).unwrap();
        assert_eq!(
            segments[1].marker, APP0,
            "JFIF APP0 must stay immediately after SOI, found marker {:#x} instead",
            segments[1].marker
        );
    }

    #[test]
    fn oversized_packet_is_rejected_not_silently_truncated() {
        let huge = "x".repeat(MAX_SEGMENT_PAYLOAD + 1);
        let jpeg = minimal_jpeg_with_app1(XMP_SIGNATURE);
        let err = write_xmp(&jpeg, &huge).unwrap_err();
        assert!(matches!(err, JpegXmpError::TooLarge { .. }));
    }

    #[test]
    fn rejects_non_jpeg_input() {
        assert!(matches!(
            read_xmp(b"not a jpeg"),
            Err(JpegXmpError::NotAJpeg)
        ));
    }
}
