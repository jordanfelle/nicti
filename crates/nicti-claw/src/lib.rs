//! Claw: module/plugin architecture (ADR-0019). The `Module` trait (identity/versioning shared
//! by every extension point), a lazy registry (§2, §5), and a checked C-ABI dylib boundary for
//! LGPL isolation (§4). Every `nicti-*` domain crate (`nicti-cornea`, `nicti-calico`,
//! `nicti-iris`, `nicti-tapetum`, `nicti-stalk`, `nicti-preen`, `nicti-lair`) builds on this
//! crate's traits — see ADR-0019 §8.

// Never-crash policy (#538): production code returns errors instead of panicking. Tests
// are exempt. Needs a documented reason to `#[allow]` on any single site.
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

pub mod dylib;
pub mod module;
pub mod registry;

pub use dylib::{DylibError, DylibModule, VTable};
pub use module::Module;
pub use registry::{Descriptor, RegisterError, Registry};
