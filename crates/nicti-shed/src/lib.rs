//! The in-app updater (#249/ADR-0249): checks GitHub Releases for a newer `nicti` version,
//! minisign-verifies the downloaded installer against [`PUBLIC_KEY_BASE64`] before ever running
//! it, and re-invokes it silently (`/S /UPDATE`, `packaging/windows/nicti.nsi`). Named after a cat
//! shedding its old coat, matching this repo's feline naming convention.
//!
//! [`check`] (release-JSON parsing + semver compare) and [`verify`] (minisign verification) are
//! pure logic, cross-platform, and unit-tested on every CI platform. The actual network check +
//! download + re-exec path ([`net`]) is `cfg(windows)`-only, matching Nicti's Windows-only v1
//! target (ADR-0015) -- there is no update channel to check on a platform Nicti doesn't ship on
//! yet.

pub mod check;
pub mod state;
pub mod verify;

#[cfg(windows)]
pub mod net;

pub use check::{LatestRelease, ReleaseAsset};

/// The minisign public key every downloaded installer is verified against, embedded at compile
/// time so a compromised update server (or a MITM'd download) can't also spoof the verification
/// key -- see `docs/decisions/release.md`.
///
/// **Placeholder.** This is a throwaway keypair generated only to exercise this crate's own
/// tests; it is NOT the production signing key. Before the first real release, generate the real
/// keypair per `docs/releasing.md`'s one-time setup section and replace this constant with that
/// keypair's public half. Every `nicti.exe` built before that replacement cannot verify a real
/// release's signature (a fail-safe default: it refuses rather than trusting nothing).
pub const PUBLIC_KEY_BASE64: &str = "RWQQhGAWQ6j6RtbA5MtQbNkvNW+yqSuYR0GAJ+YBR1DbMY87J0uIxEmK";

/// The GitHub repository this crate checks for releases against.
pub const RELEASES_LATEST_URL: &str =
    "https://api.github.com/repos/jordanfelle/nicti/releases/latest";

#[derive(Debug)]
pub enum ShedError {
    /// The release JSON didn't parse, or didn't contain the fields this crate expects.
    Parse(String),
    /// The release's tag wasn't a valid semver version.
    InvalidVersion(String),
    /// The release had no installer asset, or no `.minisig` sidecar asset.
    MissingAsset(&'static str),
    /// minisign verification failed -- a bad signature, a corrupt download, or a key mismatch.
    /// Never distinguished further than this: giving a caller (or a user-facing message) more
    /// detail about *why* a signature failed to verify would only help an attacker iterate.
    Verify,
    /// A network request failed.
    Network(String),
    /// A filesystem operation failed.
    Io(std::io::Error),
}

impl std::fmt::Display for ShedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ShedError::Parse(msg) => write!(f, "failed to parse release metadata: {msg}"),
            ShedError::InvalidVersion(msg) => write!(f, "invalid release version: {msg}"),
            ShedError::MissingAsset(kind) => write!(f, "release has no {kind} asset"),
            ShedError::Verify => write!(f, "installer signature verification failed"),
            ShedError::Network(msg) => write!(f, "network request failed: {msg}"),
            ShedError::Io(e) => write!(f, "filesystem error: {e}"),
        }
    }
}

impl std::error::Error for ShedError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ShedError::Io(e) => Some(e),
            _ => None,
        }
    }
}
