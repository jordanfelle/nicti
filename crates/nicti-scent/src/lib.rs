//! XMP interop with Lightroom Classic (#60, promoted from the #59 `scent` spike). See
//! `docs/research/scent-xmp-interop.md` for the full write-up and
//! `docs/adr/0059-xmp-interop.md` (still Proposed, pending #187) for the decision it backs.
//!
//! Scope, matching ADR-0021's handoff to #59:
//! - `lrc_fields`: layer (a), the LRC-convention rating/label/keyword mapping.
//! - `packet`: reads/patches a full XMP packet, preserving every property
//!   this spike doesn't own (`crs:`, `exif:`, `aux:`, `photoshop:`, ...)
//!   byte-for-byte, plus the lossless `nicti:` recovery layer (b).
//! - `sidecar`: RAW-file `.xmp` sidecar naming + atomic write.
//! - `embedded`: XMP inside a JPEG's APP1 segment (DNG/TIFF tag-700 write is
//!   an explicit follow-up -- see the research doc's "What wasn't reachable"
//!   section).
//! - `conflict`: the ADR-0021 newer-wins conflict rule wired to real file
//!   mtimes/hashes, plus the layer-(c) `crs:` write gate #59 was left to pick.

pub mod conflict;
pub mod embedded;
pub mod lrc_fields;
pub mod packet;
pub mod sidecar;

pub use lrc_fields::LrcMeta;
