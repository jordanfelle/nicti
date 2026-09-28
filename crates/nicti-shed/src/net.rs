//! The network half of the updater: checking GitHub Releases, downloading, and re-invoking the
//! installer. `cfg(windows)`-only -- see this crate's own module doc comment for why.

use std::process::Command;
use std::time::Duration;

use semver::Version;

use crate::check::{self, EdgeRelease, LatestRelease, ReleaseAsset};
use crate::verify;
use crate::{ShedError, PUBLIC_KEY_BASE64, RELEASES_LATEST_URL};

/// Covers the whole request (DNS through reading the body), not just connect -- a stalled
/// connection must not be able to block an update check or download indefinitely. `ureq`'s
/// implicit default agent (what a bare `ureq::get` uses) has no global timeout at all.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
/// `ureq`'s own default body-read limit is 10MB, too small for a real installer (which bundles
/// more than a bare `nicti.exe` as this grows -- DLLs, an uninstaller, etc.). 512MB is a generous
/// ceiling, not a real expected size -- just far above `ureq`'s 10MB default and comfortably
/// above anything this installer is realistically going to reach.
const MAX_INSTALLER_BYTES: u64 = 512 * 1024 * 1024;

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(REQUEST_TIMEOUT))
        .build()
        .into()
}

/// Checks GitHub Releases for a version newer than `current`. Returns `Ok(None)` when already
/// up to date -- that's the expected common case, not an error.
pub fn check_for_update(current: &Version) -> Result<Option<LatestRelease>, ShedError> {
    let body = get_string(RELEASES_LATEST_URL)?;
    let release = check::parse_latest_release(&body)?;
    if check::update_available(current, &release.version) {
        Ok(Some(release))
    } else {
        Ok(None)
    }
}

/// Checks the `edge` release (see `check::EDGE_RELEASE_URL`) against this binary's own build
/// commit ([`crate::BUILD_COMMIT_SHA`]). Returns `Ok(None)` when already on the current edge
/// build -- that's the expected common case, not an error.
pub fn check_for_edge_update() -> Result<Option<EdgeRelease>, ShedError> {
    let body = get_string(check::EDGE_RELEASE_URL)?;
    let release = check::parse_edge_release(&body)?;
    if check::edge_update_available(crate::BUILD_COMMIT_SHA, &release.commit_sha) {
        Ok(Some(release))
    } else {
        Ok(None)
    }
}

/// Downloads the installer and its minisig sidecar, verifies the installer against
/// [`PUBLIC_KEY_BASE64`], and -- only once verification succeeds -- spawns it with `/S /UPDATE`
/// and exits this process. An installer that fails verification is deleted, never executed, and
/// this function returns an error instead; the caller should surface that to the user rather
/// than silently retrying (a persistently failing verification is a signal something's wrong,
/// not a transient condition to paper over).
pub fn download_and_apply(release: &LatestRelease) -> Result<(), ShedError> {
    download_verify_and_launch(&release.installer, &release.minisig)
}

/// The edge-channel counterpart of [`download_and_apply`] -- same verify-then-launch path, just
/// over an [`EdgeRelease`]'s assets instead of a [`LatestRelease`]'s.
pub fn download_and_apply_edge(release: &EdgeRelease) -> Result<(), ShedError> {
    download_verify_and_launch(&release.installer, &release.minisig)
}

fn download_verify_and_launch(
    installer: &ReleaseAsset,
    minisig: &ReleaseAsset,
) -> Result<(), ShedError> {
    let installer_bytes = get_bytes(&installer.download_url)?;
    let signature_text = get_string(&minisig.download_url)?;

    verify::verify_installer(PUBLIC_KEY_BASE64, &installer_bytes, &signature_text)?;

    let dir = std::env::temp_dir().join("nicti-update");
    std::fs::create_dir_all(&dir).map_err(ShedError::Io)?;
    let installer_path = dir.join(&installer.name);
    std::fs::write(&installer_path, &installer_bytes).map_err(ShedError::Io)?;

    // nicti.nsi's own /UPDATE handling waits for a running nicti.exe to exit before overwriting
    // it -- this spawn-then-exit is the primary synchronization point that relies on (the
    // installer's own taskkill wait is a defensive second check, not the main mechanism).
    Command::new(&installer_path)
        .args(["/S", "/UPDATE"])
        .spawn()
        .map_err(ShedError::Io)?;

    std::process::exit(0);
}

fn get_string(url: &str) -> Result<String, ShedError> {
    agent()
        .get(url)
        .header("User-Agent", "nicti-shed")
        .call()
        .map_err(|e| ShedError::Network(e.to_string()))?
        .body_mut()
        .read_to_string()
        .map_err(|e| ShedError::Network(e.to_string()))
}

fn get_bytes(url: &str) -> Result<Vec<u8>, ShedError> {
    agent()
        .get(url)
        .header("User-Agent", "nicti-shed")
        .call()
        .map_err(|e| ShedError::Network(e.to_string()))?
        .body_mut()
        .with_config()
        .limit(MAX_INSTALLER_BYTES)
        .read_to_vec()
        .map_err(|e| ShedError::Network(e.to_string()))
}
