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
}
