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

/// Per-channel neutral estimate: raw-domain black-subtracted, normalized to [0, 1] by
/// `black`/`maximum` (identical scaling every LibRaw decode applies, per shim.h).
pub fn linearize_sample(raw: u16, channel: usize, meta: &LinearMeta) -> f64 {
    let black = meta.black as f64 + meta.cblack[channel] as f64;
    let white = meta.maximum as f64;
    ((raw as f64 - black) / (white - black)).max(0.0)
}
