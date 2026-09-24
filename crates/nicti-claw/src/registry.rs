//! A lazy, explicit-registration module registry (ADR-0004 §2, §5, §7) — generalized from
//! `spikes/sheath/src/registry.rs`. Every module registers a cheap, always-available
//! `Descriptor` at startup; building the actual instance is deferred to a `OnceLock`-backed
//! factory that runs on first use, at most once even under a concurrent race to be the first
//! caller. This is the literal mechanism behind the "Claw" name (claws stay sheathed until
//! needed).

use std::fmt;
use std::sync::{Arc, OnceLock};

use crate::module::Module;

/// Cheap, always-available identity for a module — enumerable at startup without running the
/// (possibly expensive) factory that builds the real instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Descriptor {
    pub id: &'static str,
    pub schema_version: u32,
}

/// A module that stays "sheathed" (its factory unrun) until first use, then is built at most
/// once regardless of how many threads race to be the first caller. `T` is typically a `dyn
/// Trait` extension-point supertrait (e.g. `dyn RawDecoder`), so the factory returns an `Arc<T>`
/// rather than a bare `T` — a trait object can't be returned by value.
struct LazyModule<T: ?Sized> {
    descriptor: Descriptor,
    factory: fn() -> Arc<T>,
    instance: OnceLock<Arc<T>>,
}

impl<T: ?Sized + Module> LazyModule<T> {
    fn get(&self) -> Arc<T> {
        Arc::clone(self.instance.get_or_init(|| {
            let built = (self.factory)();
            // Always-on, not `debug_assert_eq!`: this guards the module system's core identity
            // contract (a registered descriptor must describe the instance its own factory
            // builds), and a `debug_assert_eq!` compiles to nothing in a release build — silently
            // letting `get()`/`descriptors()` disagree about a module's id/version in exactly the
            // build that ships.
            assert_eq!(
                built.id(),
                self.descriptor.id,
                "module factory for {:?} built an instance whose id() disagrees with its own descriptor",
                self.descriptor,
            );
            assert_eq!(
                built.schema_version(),
                self.descriptor.schema_version,
                "module factory for {:?} built an instance whose schema_version() disagrees with its own descriptor",
                self.descriptor,
            );
            built
        }))
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum RegisterError {
    /// The id doesn't follow the `namespace.name` convention (ADR-0002's `vendor.stage_name`
    /// shape, generalized in ADR-0004 §7): at least two dot-separated segments, each segment
    /// non-empty and restricted to `[a-z0-9_]`.
    InvalidId(&'static str),
    /// A descriptor with this id is already registered.
    DuplicateId(&'static str),
}

impl fmt::Display for RegisterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RegisterError::InvalidId(id) => {
                write!(f, "invalid module id {id:?}: expected `namespace.name` with each segment matching [a-z0-9_]+")
            }
            RegisterError::DuplicateId(id) => write!(f, "module id {id:?} is already registered"),
        }
    }
}

impl std::error::Error for RegisterError {}

fn is_valid_id(id: &str) -> bool {
    let segments: Vec<&str> = id.split('.').collect();
    segments.len() >= 2
        && segments.iter().all(|segment| {
            !segment.is_empty()
                && segment
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        })
}

/// A registry of lazily-built modules for one extension point (e.g. RAW decoders). Looking up
/// an id with no registered module returns `None` (ADR-0004 §5) — a build that doesn't
/// recognize a module id treats it as read-only rather than dropping or guessing at it.
pub struct Registry<T: ?Sized + Module> {
    modules: Vec<LazyModule<T>>,
}

impl<T: ?Sized + Module> Default for Registry<T> {
    fn default() -> Self {
        Self {
            modules: Vec::new(),
        }
    }
}

impl<T: ?Sized + Module> Registry<T> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a module's descriptor and factory. The factory does not run here — only on
    /// first `get()`.
    pub fn register(
        &mut self,
        descriptor: Descriptor,
        factory: fn() -> Arc<T>,
    ) -> Result<(), RegisterError> {
        if !is_valid_id(descriptor.id) {
            return Err(RegisterError::InvalidId(descriptor.id));
        }
        if self
            .modules
            .iter()
            .any(|m| m.descriptor.id == descriptor.id)
        {
            return Err(RegisterError::DuplicateId(descriptor.id));
        }
        self.modules.push(LazyModule {
            descriptor,
            factory,
            instance: OnceLock::new(),
        });
        Ok(())
    }

    /// All registered descriptors, without building any instance.
    pub fn descriptors(&self) -> Vec<Descriptor> {
        self.modules.iter().map(|m| m.descriptor).collect()
    }

    pub fn find(&self, id: &str) -> Option<Descriptor> {
        self.modules
            .iter()
            .find(|m| m.descriptor.id == id)
            .map(|m| m.descriptor)
    }

    /// Builds (on first call) and returns the module for `id`. `None` if no module with that id
    /// is registered.
    pub fn get(&self, id: &str) -> Option<Arc<T>> {
        self.modules
            .iter()
            .find(|m| m.descriptor.id == id)
            .map(|m| m.get())
    }
}
