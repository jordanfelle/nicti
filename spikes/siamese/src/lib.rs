//! Throwaway spike for #48 (`docs/adr/0048-masking.md`): AI subject/sky segmentation, the
//! brush+gradient local-adjustment geometry model, the AI+geometry mask-group compose model, a
//! preview-to-full-res guided-filter upsample, and CPU/GPU parity for the parts #44 (Tapetum)
//! needs to run per-frame (gradient eval, brush rasterize, group compose, masked-adjust apply).
//! See each module's own doc comment; `docs/research/siamese-masking.md` is the write-up this
//! spike feeds. Not a production crate -- see `CLAUDE.md`'s package-map note on `spikes/*`.

pub mod compose;
pub mod geometry;
pub mod gpu;
pub mod image;
pub mod refine;
pub mod segment;
pub mod sky;
