//! Turning LRC's `CameraProfile` *name* into a profile nicti can render with (#381).
//!
//! Finding "Camera Landscape" for a Nikon Z 8 means walking the user's own Adobe profile folders
//! and loading the `.dcp` -- filesystem work that belongs to `nicti-pelt` (`camera_profiles.rs`),
//! which this crate cannot depend on (pelt depends on stray). The job is therefore handed a
//! resolver by whoever builds its [`ImportConfig`](crate::ImportConfig); `None` simply means
//! profiles are left out of the import (and counted as unresolved in the report).

use std::fmt;
use std::sync::Arc;

use nicti_tapetum::coat::CameraProfileParams;

/// Resolves a profile name for one camera model to the stage params that select it.
pub trait ProfileResolver: Send + Sync {
    /// `make`/`model` are the photo's own EXIF strings (as the catalog stores them); `name` is
    /// LRC's `CameraProfile` value, never empty, never `Adobe Standard`/`Embedded`.
    ///
    /// `None` when the user has no such profile installed for this camera -- the import then
    /// leaves the profile out rather than guessing a different one.
    fn resolve(&self, make: &str, model: &str, name: &str) -> Option<CameraProfileParams>;
}

/// A shareable, `Debug`-able handle so [`ImportConfig`](crate::ImportConfig) can stay `Debug + Clone`.
#[derive(Clone)]
pub struct ResolverHandle(pub Arc<dyn ProfileResolver>);

impl ResolverHandle {
    pub fn new(resolver: impl ProfileResolver + 'static) -> Self {
        Self(Arc::new(resolver))
    }
}

impl fmt::Debug for ResolverHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ResolverHandle(..)")
    }
}
