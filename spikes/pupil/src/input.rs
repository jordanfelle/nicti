//! Reads `retina dump-linear`'s output pair (a 16-bit linear-camera-RGB TIFF + JSON metadata
//! sidecar). Deliberately a small independent copy of `spikes/calico/src/linear_input.rs`'s
//! `LinearMeta`/`load`, not a path dependency on `calico` -- each spike here stays self-contained
//! and depends only on crates.io crates, the same pattern `spikes/rods` already follows rather
//! than depending on `calico`/`retina` directly.

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

/// `retina dump-linear` already black-subtracts and scales to the 16-bit output range (see
/// calico's own `linearize_sample` doc comment for why re-subtracting `black`/`cblack` here would
/// double-apply the correction) -- this just maps the u16 sample to `0.0..=1.0`.
pub fn linearize_sample(raw: u16) -> f64 {
    raw as f64 / u16::MAX as f64
}

/// As-shot white balance: `cam_mul`, normalized so the green channel (index 1) is 1.0, matching
/// the convention LibRaw's own `cam_mul` values are published in.
pub fn white_balance(cam_rgb: [f64; 3], cam_mul: &[f32; 4]) -> [f64; 3] {
    let green = cam_mul[1] as f64;
    std::array::from_fn(|c| cam_rgb[c] * (cam_mul[c] as f64 / green))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linearize_sample_maps_full_range() {
        assert_eq!(linearize_sample(0), 0.0);
        assert!((linearize_sample(u16::MAX) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn white_balance_leaves_green_unchanged() {
        let cam_mul = [2.0_f32, 1.0, 1.5, 0.0];
        let out = white_balance([0.5, 0.5, 0.5], &cam_mul);
        assert!((out[1] - 0.5).abs() < 1e-9);
        assert!((out[0] - 1.0).abs() < 1e-9);
        assert!((out[2] - 0.75).abs() < 1e-9);
    }
}
