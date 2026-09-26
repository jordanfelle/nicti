//! Maps a Nikon `NEFCompression` MakerNote tag value to the same label strings used in
//! `docs/ref-10k-manifest.csv`'s `compression` column, so `sweep`/`compare` output can be joined
//! against the manifest by label instead of raw tag value.

/// Only three of these values are actually verified against something real: **3** (Lossless,
/// confirmed against a real decode of `ref-07516.nef`, manifest-labeled "Lossless" -- an earlier
/// draft of this map guessed wrong values by pattern-matching LibRaw's *other* enums instead of
/// checking a real file) and **13/14** (HE/HE*, confirmed directly from `nikon_he_decoder.cpp`'s
/// `kNefCompressionHe`/`kNefCompressionHeStar` and `tiff.cpp`'s dispatch). Every other value below
/// is copied from ExifTool's `Nikon.pm` `NEFCompression` PrintConv table from memory, not
/// cross-checked against any real file or primary source in this research pass -- a hostile
/// review caught an earlier draft presenting all 11 entries with identical confidence, which is
/// exactly the mistake that produced the tag-3 error above. Labeled `(unverified)` below so a
/// sweep/scan hitting one of these (no ref-10k file did, in this research) is visibly flagged
/// rather than silently trusted. Confirm against a real file before removing the suffix.
pub fn label(nef_compression: u16) -> &'static str {
    match nef_compression {
        1 => "Lossy (Type 1) (unverified)",
        2 => "Uncompressed (unverified)",
        3 => "Lossless",
        4 => "Lossy (Type 2) (unverified)",
        5 => "Striped packed 12 bit (unverified)",
        6 => "Unpacked 12 bit (unverified)",
        7 => "Striped packed 14 bit (unverified)",
        8 => "Unpacked 14 bit (unverified)",
        9 => "High Efficiency (unverified)",
        10 => "High Efficiency* (unverified)",
        13 => "High Efficiency",
        14 => "High Efficiency*",
        // Deliberately not panicking: an unrecognized tag value should show up as data in a
        // sweep's JSONL output (for investigation), not crash the whole run.
        other => unrecognized_label(other),
    }
}

fn unrecognized_label(tag: u16) -> &'static str {
    // Leaked on purpose -- this is spike code, only hit for genuinely unexpected tag values
    // (which the ref-10k sweep should never actually produce), and avoids plumbing a String
    // through every call site for a case that shouldn't occur.
    Box::leak(format!("Unknown({tag})").into_boxed_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn he_and_he_star_map_correctly() {
        assert_eq!(label(13), "High Efficiency");
        assert_eq!(label(14), "High Efficiency*");
    }

    #[test]
    fn lossless_matches_manifest_label() {
        // Tag value 3, not 6 -- confirmed against a real decode of ref-07516.nef
        // (manifest-labeled "Lossless"), see this file's own doc comment.
        assert_eq!(label(3), "Lossless");
    }
}
