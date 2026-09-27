//! Reads `retina dump-classic`/`dump-linear`'s output pair (a 16-bit camera-RGB TIFF + JSON
//! metadata sidecar) without depending on the `retina` crate itself, same reasoning as calico's
//! own copy of this reader (see `spikes/calico/src/linear_input.rs`) -- retina's LibRaw FFI/
//! submodule isn't something every candidate-comparison environment needs to build. The JSON
//! shape here must stay in sync with `spikes/retina/src/linear.rs`'s `LinearMeta`.

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

/// Same reasoning as calico's `linearize_sample`: `dcraw_process()`'s `scale_colors()` already
/// did black subtraction + scaling to the 16-bit output range before this ever sees a sample, so
/// this is a plain rescale to 0..1, not a second black/white-level correction.
pub fn linearize_sample(raw: u16) -> f64 {
    raw as f64 / u16::MAX as f64
}
