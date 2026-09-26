//! Reads `retina dump-linear`'s output pair (a 16-bit linear-camera-RGB TIFF + JSON metadata
//! sidecar) without depending on the `retina` crate itself (which needs the LibRaw FFI/submodule
//! this spike deliberately stays free of -- see ADR-0021). The JSON shape here must stay in sync
//! with `spikes/retina/src/linear.rs`'s `LinearMeta`; there's no shared type between the two
//! crates since retina can't build in every environment calico needs to build in.

use std::path::Path;

use image::{ImageBuffer, Rgb};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct LinearMeta {
    pub make: String,
    pub model: String,
    pub width: u32,
    pub height: u32,
    pub black: u32,
    pub maximum: u32,
    pub cam_mul: [f32; 4],
    pub pre_mul: [f32; 4],
    pub cam_xyz: [f32; 12],
    pub cblack: [u32; 4],
}

pub struct LinearInput {
    pub meta: LinearMeta,
    pub image: ImageBuffer<Rgb<u16>, Vec<u16>>,
}

pub fn load(tiff_path: &Path, json_path: &Path) -> anyhow::Result<LinearInput> {
    let meta: LinearMeta = serde_json::from_slice(&std::fs::read(json_path)?)?;
    let image = image::open(tiff_path)?.into_rgb16();
    anyhow::ensure!(
        image.width() == meta.width && image.height() == meta.height,
        "TIFF dimensions ({}x{}) don't match metadata sidecar ({}x{})",
        image.width(),
        image.height(),
        meta.width,
        meta.height
    );
    Ok(LinearInput { meta, image })
}

/// `retina dump-linear`'s `LibRaw::dcraw_process()` call already runs `scale_colors()` (black
/// subtraction + per-channel scaling to the 16-bit output range) before this ever sees a sample --
/// `shim.h`'s own doc comment says so. An earlier version of this function re-subtracted
/// `black`/`cblack` and re-divided by the raw sensor `maximum` on top of that, which
/// double-applies the black-level correction and uses the wrong (pre-scaling) denominator; for a
/// 14-bit sensor this can push values above `1.0` and corrupts every downstream color/tone stage.
/// `meta.black`/`meta.maximum`/`meta.cblack` are kept in the sidecar for reference (and because
/// LibRaw's own `maximum` field can change *during* `scale_colors()`, so a later consumer
/// shouldn't assume it means "pre-scaling sensor max"), not consumed here.
pub fn linearize_sample(raw: u16, _channel: usize, _meta: &LinearMeta) -> f64 {
    raw as f64 / u16::MAX as f64
}
