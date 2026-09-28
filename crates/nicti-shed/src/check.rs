//! Parses a GitHub `releases/latest` API response and compares its version against the running
//! binary's own. Pure logic -- no network I/O here, see [`crate::net`] for that (Windows-only).

use semver::Version;
use serde::Deserialize;

use crate::ShedError;

#[derive(Debug, Clone, Deserialize)]
struct RawRelease {
    tag_name: String,
    assets: Vec<RawAsset>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawAsset {
    name: String,
    browser_download_url: String,
}

/// One downloadable file attached to a GitHub release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseAsset {
    pub name: String,
    pub download_url: String,
}

/// The parts of a GitHub release this crate actually needs: its version, the installer, and the
/// installer's minisig sidecar. `release.yml` always publishes exactly one of each per release
/// (`Nicti-Setup-<version>-x64.exe` + `.exe.minisig`), so "which asset is which" is decided by
/// filename suffix, not position or count.
#[derive(Debug, Clone)]
pub struct LatestRelease {
    pub version: Version,
    pub installer: ReleaseAsset,
    pub minisig: ReleaseAsset,
}

/// Parses a GitHub `GET /repos/.../releases/latest` response body into a [`LatestRelease`].
pub fn parse_latest_release(json: &str) -> Result<LatestRelease, ShedError> {
    let raw: RawRelease =
        serde_json::from_str(json).map_err(|e| ShedError::Parse(e.to_string()))?;

    // release.yml's tags are always "v<version>" (the workflow itself requires this, checking
    // the tag against Cargo.toml's version before publishing) -- strip the leading "v" rather
    // than requiring semver::Version::parse to accept it (it doesn't; a bare leading "v" isn't
    // part of the semver grammar).
    let version_str = raw.tag_name.strip_prefix('v').unwrap_or(&raw.tag_name);
    let version =
        Version::parse(version_str).map_err(|e| ShedError::InvalidVersion(e.to_string()))?;

    let installer = raw
        .assets
        .iter()
        .find(|a| a.name.starts_with("Nicti-Setup-") && a.name.ends_with(".exe"))
        .map(to_asset)
        .ok_or(ShedError::MissingAsset("installer"))?;
    let minisig = raw
        .assets
        .iter()
        .find(|a| a.name.ends_with(".exe.minisig"))
        .map(to_asset)
        .ok_or(ShedError::MissingAsset("minisig"))?;

    Ok(LatestRelease {
        version,
        installer,
        minisig,
    })
}

fn to_asset(raw: &RawAsset) -> ReleaseAsset {
    ReleaseAsset {
        name: raw.name.clone(),
        download_url: raw.browser_download_url.clone(),
    }
}

/// Whether `latest` is a real update over `current` -- strictly newer, never "different."
pub fn update_available(current: &Version, latest: &Version) -> bool {
    latest > current
}

/// The GitHub repo's single moving `edge` release/tag (see `release.yml`'s "Publish edge
/// release" step, #267) -- a fixed URL, not one derived from `RELEASES_LATEST_URL`, since GitHub's
/// `/releases/latest` never returns a prerelease and `edge` is always published with
/// `--prerelease`.
pub const EDGE_RELEASE_URL: &str =
    "https://api.github.com/repos/jordanfelle/nicti/releases/tags/edge";

#[derive(Debug, Clone, Deserialize)]
struct RawEdgeRelease {
    target_commitish: String,
    assets: Vec<RawAsset>,
}

/// The edge channel's own release shape: no usable semver (edge builds don't bump
/// `Cargo.toml`'s version per commit), so identity is the commit SHA `edge` currently points at
/// instead -- `target_commitish`, which `release.yml`'s own publish step sets to the exact
/// `GITHUB_SHA` it built from (verified there via its own `still_current`/tag-ref checks).
#[derive(Debug, Clone)]
pub struct EdgeRelease {
    /// Full 40-character commit SHA.
    pub commit_sha: String,
    /// First 7 characters of `commit_sha`, for a short display label (matches `release.yml`'s
    /// own release title, `"Nicti edge (<sha7>)"`).
    pub short_sha: String,
    pub installer: ReleaseAsset,
    pub minisig: ReleaseAsset,
}

/// Parses a GitHub `GET /repos/.../releases/tags/edge` response body into an [`EdgeRelease`].
pub fn parse_edge_release(json: &str) -> Result<EdgeRelease, ShedError> {
    let raw: RawEdgeRelease =
        serde_json::from_str(json).map_err(|e| ShedError::Parse(e.to_string()))?;

    // Every ASCII hex digit is exactly one byte, so a length check alone (the original form of
    // this guard) doesn't rule out a non-ASCII string that's *byte*-long-enough but splits a
    // multi-byte UTF-8 character right at the 7-byte mark -- `target_commitish[..7]` below would
    // then panic instead of returning an error (CodeRabbit review, PR #283). Requiring every byte
    // to be an ASCII hex digit rules that out entirely, and is what a real commit SHA always is
    // anyway.
    if raw.target_commitish.len() < 7
        || !raw.target_commitish.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(ShedError::Parse(format!(
            "edge release's target_commitish isn't a plausible commit SHA: {:?}",
            raw.target_commitish
        )));
    }

    let installer = raw
        .assets
        .iter()
        .find(|a| a.name.starts_with("Nicti-Setup-") && a.name.ends_with(".exe"))
        .map(to_asset)
        .ok_or(ShedError::MissingAsset("installer"))?;
    let minisig = raw
        .assets
        .iter()
        .find(|a| a.name.ends_with(".exe.minisig"))
        .map(to_asset)
        .ok_or(ShedError::MissingAsset("minisig"))?;

    Ok(EdgeRelease {
        short_sha: raw.target_commitish[..7].to_string(),
        commit_sha: raw.target_commitish,
        installer,
        minisig,
    })
}

/// Whether `latest_sha` names a different commit than `current_sha` -- the edge channel has no
/// ordering to compare (unlike stable's semver), just "is this the same build I'm already
/// running."
pub fn edge_update_available(current_sha: &str, latest_sha: &str) -> bool {
    current_sha != latest_sha
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release_json(tag: &str, assets: &[(&str, &str)]) -> String {
        let assets_json: Vec<String> = assets
            .iter()
            .map(|(name, url)| format!(r#"{{"name":"{name}","browser_download_url":"{url}"}}"#))
            .collect();
        format!(
            r#"{{"tag_name":"{tag}","assets":[{}]}}"#,
            assets_json.join(",")
        )
    }

    #[test]
    fn parses_a_real_shaped_release() {
        let json = release_json(
            "v0.2.0",
            &[
                (
                    "Nicti-Setup-0.2.0-x64.exe",
                    "https://example.com/Nicti-Setup-0.2.0-x64.exe",
                ),
                (
                    "Nicti-Setup-0.2.0-x64.exe.minisig",
                    "https://example.com/Nicti-Setup-0.2.0-x64.exe.minisig",
                ),
                (
                    "Nicti-Setup-0.2.0-x64.exe.sha256",
                    "https://example.com/Nicti-Setup-0.2.0-x64.exe.sha256",
                ),
            ],
        );
        let release = parse_latest_release(&json).unwrap();
        assert_eq!(release.version, Version::new(0, 2, 0));
        assert_eq!(release.installer.name, "Nicti-Setup-0.2.0-x64.exe");
        assert_eq!(release.minisig.name, "Nicti-Setup-0.2.0-x64.exe.minisig");
    }

    #[test]
    fn rejects_missing_installer_asset() {
        let json = release_json(
            "v0.2.0",
            &[("readme.txt", "https://example.com/readme.txt")],
        );
        assert!(matches!(
            parse_latest_release(&json),
            Err(ShedError::MissingAsset("installer"))
        ));
    }

    #[test]
    fn rejects_missing_minisig_asset() {
        let json = release_json(
            "v0.2.0",
            &[(
                "Nicti-Setup-0.2.0-x64.exe",
                "https://example.com/Nicti-Setup-0.2.0-x64.exe",
            )],
        );
        assert!(matches!(
            parse_latest_release(&json),
            Err(ShedError::MissingAsset("minisig"))
        ));
    }

    #[test]
    fn rejects_invalid_version_tag() {
        let json = release_json("not-a-version", &[]);
        assert!(matches!(
            parse_latest_release(&json),
            Err(ShedError::InvalidVersion(_))
        ));
    }

    #[test]
    fn newer_version_is_an_update() {
        assert!(update_available(
            &Version::new(0, 1, 0),
            &Version::new(0, 2, 0)
        ));
    }

    #[test]
    fn same_or_older_version_is_not_an_update() {
        assert!(!update_available(
            &Version::new(0, 2, 0),
            &Version::new(0, 2, 0)
        ));
        assert!(!update_available(
            &Version::new(0, 2, 0),
            &Version::new(0, 1, 0)
        ));
    }

    fn edge_release_json(sha: &str, assets: &[(&str, &str)]) -> String {
        let assets_json: Vec<String> = assets
            .iter()
            .map(|(name, url)| format!(r#"{{"name":"{name}","browser_download_url":"{url}"}}"#))
            .collect();
        format!(
            r#"{{"target_commitish":"{sha}","assets":[{}]}}"#,
            assets_json.join(",")
        )
    }

    #[test]
    fn parses_a_real_shaped_edge_release() {
        let json = edge_release_json(
            "548afd26cbbec1279fc007f44eb3d6095a1a9c34",
            &[
                (
                    "Nicti-Setup-0.1.0-x64.exe",
                    "https://example.com/Nicti-Setup-0.1.0-x64.exe",
                ),
                (
                    "Nicti-Setup-0.1.0-x64.exe.minisig",
                    "https://example.com/Nicti-Setup-0.1.0-x64.exe.minisig",
                ),
                (
                    "Nicti-Setup-0.1.0-x64.exe.sha256",
                    "https://example.com/Nicti-Setup-0.1.0-x64.exe.sha256",
                ),
            ],
        );
        let release = parse_edge_release(&json).unwrap();
        assert_eq!(
            release.commit_sha,
            "548afd26cbbec1279fc007f44eb3d6095a1a9c34"
        );
        assert_eq!(release.short_sha, "548afd2");
        assert_eq!(release.installer.name, "Nicti-Setup-0.1.0-x64.exe");
        assert_eq!(release.minisig.name, "Nicti-Setup-0.1.0-x64.exe.minisig");
    }

    #[test]
    fn rejects_edge_release_missing_installer_asset() {
        let json = edge_release_json(
            "548afd26cbbec1279fc007f44eb3d6095a1a9c34",
            &[("readme.txt", "https://example.com/readme.txt")],
        );
        assert!(matches!(
            parse_edge_release(&json),
            Err(ShedError::MissingAsset("installer"))
        ));
    }

    #[test]
    fn rejects_edge_release_missing_minisig_asset() {
        let json = edge_release_json(
            "548afd26cbbec1279fc007f44eb3d6095a1a9c34",
            &[(
                "Nicti-Setup-0.1.0-x64.exe",
                "https://example.com/Nicti-Setup-0.1.0-x64.exe",
            )],
        );
        assert!(matches!(
            parse_edge_release(&json),
            Err(ShedError::MissingAsset("minisig"))
        ));
    }

    #[test]
    fn rejects_edge_release_with_too_short_a_commitish() {
        let json = edge_release_json("abc", &[]);
        assert!(matches!(
            parse_edge_release(&json),
            Err(ShedError::Parse(_))
        ));
    }

    #[test]
    fn rejects_a_non_ascii_commitish_instead_of_panicking() {
        // 8 bytes, none of them a valid single-byte boundary at index 7 (each "é" is 2 UTF-8
        // bytes) -- a naive byte-length check alone would accept this and then panic slicing
        // `[..7]` mid-character.
        let json = edge_release_json("éééé", &[]);
        assert!(matches!(
            parse_edge_release(&json),
            Err(ShedError::Parse(_))
        ));
    }

    #[test]
    fn different_commit_is_an_edge_update() {
        assert!(edge_update_available("aaaaaaa", "bbbbbbb"));
    }

    #[test]
    fn same_commit_is_not_an_edge_update() {
        assert!(!edge_update_available("aaaaaaa", "aaaaaaa"));
    }
}
