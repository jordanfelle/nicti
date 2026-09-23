use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

/// Cheap, always-available identity for a module — enumerable at startup without
/// running the (possibly expensive) factory that builds the real instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Descriptor {
    pub id: &'static str,
    pub version: u32,
}

/// A module that stays "sheathed" (its factory unrun) until first use, then is built at
/// most once regardless of how many threads race to be the first caller.
pub struct LazyModule<T> {
    descriptor: Descriptor,
    factory: fn() -> T,
    instance: OnceLock<Arc<T>>,
    build_count: AtomicUsize,
}

impl<T> LazyModule<T> {
    pub const fn new(descriptor: Descriptor, factory: fn() -> T) -> Self {
        Self {
            descriptor,
            factory,
            instance: OnceLock::new(),
            build_count: AtomicUsize::new(0),
        }
    }

    pub fn descriptor(&self) -> Descriptor {
        self.descriptor
    }

    /// Returns the built instance, running the factory on the first call only.
    pub fn get(&self) -> Arc<T> {
        Arc::clone(self.instance.get_or_init(|| {
            self.build_count.fetch_add(1, Ordering::SeqCst);
            Arc::new((self.factory)())
        }))
    }

    /// How many times the factory has actually run. Used by tests to prove laziness
    /// (0 before first `get()`) and single-init-under-concurrency (still 1 after a race).
    pub fn build_count(&self) -> usize {
        self.build_count.load(Ordering::SeqCst)
    }
}

/// Looks up a module by id among a fixed set of descriptors, without building any of them.
/// Models the registry-level "no module installed for this stage id" case from ADR-0002 —
/// a plugin stage a build doesn't recognize stays read-only rather than being dropped or guessed at.
pub fn find_descriptor<'a>(descriptors: &'a [Descriptor], id: &str) -> Option<&'a Descriptor> {
    descriptors.iter().find(|d| d.id == id)
}
