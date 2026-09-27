//! Claw: module/plugin architecture (ADR-0019). The `Module` trait (identity/versioning shared
//! by every extension point), a lazy registry (§2, §5), and a checked C-ABI dylib boundary for
//! LGPL isolation (§4). Every `nicti-*` domain crate (`nicti-cornea`, `nicti-calico`,
//! `nicti-iris`, `nicti-render`, `nicti-stalk`, `nicti-preen`, `nicti-lair`) builds on this
//! crate's traits — see ADR-0019 §8.

pub mod dylib;
pub mod module;
pub mod registry;

pub use dylib::{DylibError, DylibModule, VTable};
pub use module::Module;
pub use registry::{Descriptor, RegisterError, Registry};
