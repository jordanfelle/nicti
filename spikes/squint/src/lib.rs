//! Throwaway spike for #34 (ADR-0034): blur/motion-blur/misfocus/eye detection. See each module's
//! own doc comment; `docs/research/squint-blur-eye-detection.md` has the full write-up.

pub mod af;
pub mod decode;
pub mod eval;
pub mod eyes;
pub mod label;
pub mod metrics;
pub mod sharp;
pub mod source;
pub mod synth;
