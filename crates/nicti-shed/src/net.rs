//! The network half of the updater: checking GitHub Releases, downloading, and re-invoking the
//! installer. `cfg(windows)`-only -- see this crate's own module doc comment for why.

use std::process::Command;

use semver::Version;

use crate::check::{self, LatestRelease};
use crate::verify;
use crate::{ShedError, PUBLIC_KEY_BASE64, RELEASES_LATEST_URL};

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

/// Downloads the installer and its minisig sidecar, verifies the installer against
/// [`PUBLIC_KEY_BASE64`], and -- only once verification succeeds -- spawns it with `/S /UPDATE`
/// and exits this process. An installer that fails verification is deleted, never executed, and
/// this function returns an error instead; the caller should surface that to the user rather
/// than silently retrying (a persistently failing verification is a signal something's wrong,
/// not a transient condition to paper over).
pub fn download_and_apply(release: &LatestRelease) -> Result<(), ShedError> {
    let installer_bytes = get_bytes(&release.installer.download_url)?;
    let signature_text = get_string(&release.minisig.download_url)?;

    verify::verify_installer(PUBLIC_KEY_BASE64, &installer_bytes, &signature_text)?;

    let dir = std::env::temp_dir().join("nicti-update");
    std::fs::create_dir_all(&dir).map_err(ShedError::Io)?;
    let installer_path = dir.join(&release.installer.name);
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
    ureq::get(url)
        .header("User-Agent", "nicti-shed")
        .call()
        .map_err(|e| ShedError::Network(e.to_string()))?
        .body_mut()
        .read_to_string()
        .map_err(|e| ShedError::Network(e.to_string()))
}

fn get_bytes(url: &str) -> Result<Vec<u8>, ShedError> {
    ureq::get(url)
        .header("User-Agent", "nicti-shed")
        .call()
        .map_err(|e| ShedError::Network(e.to_string()))?
        .body_mut()
        .read_to_vec()
        .map_err(|e| ShedError::Network(e.to_string()))
}
