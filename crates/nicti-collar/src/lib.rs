//! Collar and Hiss: the security model for Claw v2 third-party plugins (ADR-0214, #230).
//!
//! - [`manifest`]: the strict `collar.toml` parser. Unknown fields, missing justifications, a
//!   `RenderStage` extension point, wildcard or IP hosts, and budgets looser than the host's are
//!   all refused at parse time. **No entry, no access.**
//! - [`consent`]: which changes in an updated manifest widen what a module may do and so disable
//!   it until the user re-consents (field by field, not just at the capability-kind level).
//! - [`authorize`]: the host-side filters ADR-0214 section 3 makes Claw's own responsibility on
//!   top of WASI's link-time switch: a request's authority against the declared hosts and the
//!   active action scope, and a path against the one directory the user picked.
//! - [`hiss`]: the kill switch's policy: bug versus violation, auto-disable, quarantine, the
//!   persisted per-module state and the file-backed safe-mode flag read before any module runs.
//! - [`sandbox`]: a wasmtime `Engine` per module that traps a module mid-loop on `kill()`.
//!
//! This crate is policy and mechanism only: nothing hosts a plugin yet (no WASI linking, no
//! consent screen, no menu item). Those are follow-ups once v2 hosting is being built.

pub mod authorize;
pub mod consent;
pub mod hiss;
pub mod manifest;
pub mod sandbox;
