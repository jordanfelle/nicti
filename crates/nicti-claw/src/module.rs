//! The `Module` trait: identity and versioning shared by every extension point (RAW decoder,
//! camera color profile, lens-correction data, render stage, AI model provider, exporter,
//! catalog store). Deliberately excludes execution — see ADR-0004 §7.

/// Shared identity/versioning shape for every Claw extension point. A render stage's actual
/// per-frame execution signature is owned by #16/#44, not this trait.
pub trait Module: Send + Sync {
    /// Namespaced id (e.g. "nicti.decoder.libraw", or "vendor.stage_name" for a v2 plugin —
    /// ADR-0002's stage-id convention, generalized to every extension point).
    fn id(&self) -> &str;

    fn schema_version(&self) -> u32;

    /// Migrates an older params blob forward (ADR-0002's `legacy_params()`-style pattern).
    /// `None` means this build doesn't recognize the version at all — the caller must treat
    /// the data as read-only rather than dropping or guessing at it.
    fn migrate_params(
        &self,
        from_version: u32,
        params: serde_json::Value,
    ) -> Option<serde_json::Value>;
}
