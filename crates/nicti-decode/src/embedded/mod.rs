//! Embedded-preview extraction (#22): finds and reads out the embedded JPEG previews a NEF/DNG
//! already carries (PreviewIFD/thumbnail/SubIFD), so import doesn't need a RAW decode at all to
//! produce the T0 grid preview ADR-0029 specifies.
//!
//! Adapted from `spikes/sniff`'s IFD walker (#28/#29), which was cross-checked byte-for-byte
//! against `exiftool` on real Z8/D7500 files — see `docs/decisions/preview-tiers.md`. This is a
//! production-scoped copy: ranged reads via `ByteSource`/`FileSource` are kept (the whole point of
//! going ranged is avoiding a full-file read just to locate a preview), but `sniff`'s cold/warm
//! `FILE_FLAG_NO_BUFFERING` benchmarking distinction is dropped — that's a measurement concern for
//! `sniff`'s own benchmark harness, not something import-time extraction needs.

pub mod ifd;
pub mod source;

pub use ifd::{ByteOrder, EmbeddedJpeg, IfdError, PreviewSource, Walker};
pub use source::{ByteSource, FileSource, SliceSource};
