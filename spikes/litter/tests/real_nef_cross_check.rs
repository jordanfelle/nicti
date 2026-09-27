//! Cross-checks `litter::nef`'s parsing against real Nikon Z8 NEFs, using each file's own XMP
//! sidecar (written by Lightroom Classic) as independent ground truth for `DateTimeOriginal`+
//! `SubSecTimeOriginal`, `ShutterCount`, and `SerialNumber`.
//!
//! Requires a real, non-public NEF+XMP folder (the user's own photo library) -- gated on
//! `NICTI_TEST_REAL_NEF_DIR`, same convention as `spikes/groom`'s real-model tests. Skips (not
//! fails) when unset, so CI (which has neither the env var nor the files) passes cleanly.

use std::fs;
use std::path::PathBuf;

use litter::nef::NefReader;
use litter::source::FileSource;

/// Extracts `attr="value"` from a raw XMP sidecar's text via plain substring search -- not a real
/// XML/RDF parser (this test only ever needs a handful of flat attributes LRC always writes as
/// inline `key="value"` pairs on `rdf:Description`, never nested elements), so adding an XML
/// dependency to this spike just for a test helper isn't worth it.
fn xmp_attr(xmp: &str, attr: &str) -> Option<String> {
    let needle = format!("{attr}=\"");
    let start = xmp.find(&needle)? + needle.len();
    let end = xmp[start..].find('"')? + start;
    Some(xmp[start..end].to_string())
}

#[test]
fn real_nefs_match_their_own_xmp_sidecars() {
    let Ok(dir) = std::env::var("NICTI_TEST_REAL_NEF_DIR") else {
        eprintln!("skipping: set NICTI_TEST_REAL_NEF_DIR to a folder of real NEF+XMP pairs");
        return;
    };
    let dir = PathBuf::from(dir);

    let mut checked = 0;
    let mut entries: Vec<_> = fs::read_dir(&dir)
        .expect("read NICTI_TEST_REAL_NEF_DIR")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("nef"))
                .unwrap_or(false)
        })
        .collect();
    entries.sort();
    assert!(
        !entries.is_empty(),
        "no .NEF files found in {}",
        dir.display()
    );

    for nef_path in entries {
        let xmp_path = nef_path.with_extension("xmp");
        let Ok(xmp) = fs::read_to_string(&xmp_path) else {
            continue;
        };

        let source = FileSource::open(&nef_path).expect("open NEF");
        let mut reader = NefReader::new(source).expect("valid TIFF header");
        let meta = reader.read_meta().expect("read_meta");

        // exif:DateTimeOriginal="2025-12-27T19:48:32.520-04:00" -- date/time/millis, ignoring the
        // UTC offset suffix (litter's own CaptureTime is offset-naive by design, see nef.rs).
        let xmp_dt = xmp_attr(&xmp, "exif:DateTimeOriginal")
            .unwrap_or_else(|| panic!("no exif:DateTimeOriginal in {}", xmp_path.display()));
        let (date, rest) = xmp_dt.split_once('T').expect("date/time separator");
        let (time, millis_and_offset) = rest.split_once('.').expect("subsecond separator");
        let millis: u16 = millis_and_offset[..3].parse().expect("millis digits");
        let (y, mo, d) = {
            let mut parts = date.split('-');
            (
                parts.next().unwrap().parse::<u16>().unwrap(),
                parts.next().unwrap().parse::<u8>().unwrap(),
                parts.next().unwrap().parse::<u8>().unwrap(),
            )
        };
        let (h, mi, s) = {
            let mut parts = time.split(':');
            (
                parts.next().unwrap().parse::<u8>().unwrap(),
                parts.next().unwrap().parse::<u8>().unwrap(),
                parts.next().unwrap().parse::<u8>().unwrap(),
            )
        };

        assert_eq!(meta.capture_time.year, y, "{}: year", nef_path.display());
        assert_eq!(meta.capture_time.month, mo, "{}: month", nef_path.display());
        assert_eq!(meta.capture_time.day, d, "{}: day", nef_path.display());
        assert_eq!(meta.capture_time.hour, h, "{}: hour", nef_path.display());
        assert_eq!(
            meta.capture_time.minute,
            mi,
            "{}: minute",
            nef_path.display()
        );
        assert_eq!(
            meta.capture_time.second,
            s,
            "{}: second",
            nef_path.display()
        );
        // Within 1ms, not exact: Lightroom's own XMP-writing rounds the EXIF SubSecTimeOriginal
        // decimal-fraction value slightly differently than a literal "0.<digits>" expansion does
        // (confirmed on a real file: EXIF SubSecTimeOriginal "56" -> this parser's 560ms, exiftool
        // agrees (Composite SubSecDateTimeOriginal reports the same ".56"), but the LRC-written
        // XMP sidecar says ".559" -- LRC's own internal rounding, not a parsing bug here, and
        // irrelevant to grouping decisions at this resolution.
        let millis_diff = (meta.capture_time.millis as i32 - millis as i32).abs();
        assert!(
            millis_diff <= 1,
            "{}: millis differs by more than 1ms: ours={}, xmp={}",
            nef_path.display(),
            meta.capture_time.millis,
            millis
        );

        if let Some(image_number) = xmp_attr(&xmp, "aux:ImageNumber") {
            let expected: u32 = image_number.parse().expect("aux:ImageNumber is numeric");
            assert_eq!(
                meta.shutter_count,
                Some(expected),
                "{}: shutter_count vs aux:ImageNumber",
                nef_path.display()
            );
        }
        if let Some(serial) = xmp_attr(&xmp, "aux:SerialNumber") {
            assert_eq!(
                meta.serial.as_deref(),
                Some(serial.as_str()),
                "{}: serial vs aux:SerialNumber",
                nef_path.display()
            );
        }
        assert!(
            meta.preview.is_some(),
            "{}: no T0 preview found",
            nef_path.display()
        );

        checked += 1;
    }

    assert!(checked > 0, "no NEF+XMP pairs were actually checked");
    eprintln!("cross-checked {checked} real NEFs against their XMP sidecars");
}
