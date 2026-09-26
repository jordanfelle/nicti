//! #71's relative-path normalization rules, shared by `schema.rs` and `relink.rs`: UTF-8, `/`
//! separator (never `\`), NFC-normalized, plus a case-folded lookup column for filesystems that
//! are case-insensitive-but-preserving (NTFS, exFAT) without assuming a 260-character `MAX_PATH`.

use std::path::Path;
use unicode_normalization::UnicodeNormalization;

/// Normalizes an absolute path's tail (relative to some root) into the canonical form stored in
/// `asset.rel_path`: forward slashes, NFC-composed, no leading/trailing slash.
pub fn normalize_rel_path(path: &Path) -> String {
    let raw = path.to_string_lossy().replace('\\', "/");
    let composed: String = raw.nfc().collect();
    composed.trim_matches('/').to_string()
}

/// Case-folded form for the `rel_path_fold` lookup column -- NTFS/exFAT are case-insensitive but
/// case-preserving, so a lookup by path must not depend on which case the user (or LRC) happened
/// to write.
pub fn fold(rel_path: &str) -> String {
    rel_path.to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn backslashes_become_forward_slashes() {
        assert_eq!(
            normalize_rel_path(&PathBuf::from(r"2026\09\26\IMG_0001.NEF")),
            "2026/09/26/IMG_0001.NEF"
        );
    }

    #[test]
    fn nfc_composes_decomposed_forms() {
        // "é" as e + combining acute (NFD) must normalize to the same string as precomposed é (NFC).
        let decomposed = PathBuf::from("caf\u{0065}\u{0301}.NEF");
        let precomposed = PathBuf::from("caf\u{00e9}.NEF");
        assert_eq!(
            normalize_rel_path(&decomposed),
            normalize_rel_path(&precomposed)
        );
    }

    #[test]
    fn fold_is_case_insensitive() {
        assert_eq!(fold("2026/IMG_0001.NEF"), fold("2026/img_0001.nef"));
    }

    #[test]
    fn no_leading_or_trailing_slash() {
        assert_eq!(normalize_rel_path(&PathBuf::from("/foo/bar/")), "foo/bar");
    }
}
