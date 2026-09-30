//! AI object removal and heal-source picking (#51, ADR-0051), promoted from `spikes/groom`.
//!
//! The flow for one removal ([`remove::RemovalEngine::remove`]): a click/box prompt on the photo
//! goes to MobileSAM ([`sam`]) for an object mask; the mask picks a square crop with context around
//! it; LaMa ([`lama`]) inpaints the crop at 512x512; the result is mapped back into the heal stage's
//! own color space ([`space`]) and returned as a `nicti_tapetum::heal::RemovalPatch` the GPU
//! composites over the frame. [`job::RemoveJob`] runs that on Pounce's GPU lane.
//!
//! Everything model-independent (geometry, color mapping, the pipeline itself) is unit-tested with
//! fake segmenter/inpainter implementations; the ONNX wrappers are exercised against the real
//! weights by `#[ignore]`d tests (see each module). Weights are never bundled or fetched from here:
//! paths come from `nicti_stalk::models::RemovalModels`, installed only by an explicit user action
//! (ADR-0218).

use std::path::PathBuf;
use std::sync::Arc;

use nicti_cornea::LinearFrame;

pub mod geom;
pub mod install;
pub mod job;
pub mod lama;
pub mod real;
pub mod remove;
pub mod sam;
pub mod source;
pub mod space;

#[derive(Debug, thiserror::Error)]
pub enum RemovalError {
    #[error("model file not found: {0}")]
    ModelNotFound(PathBuf),
    #[error("ONNX Runtime error: {0}")]
    Ort(String),
    #[error("no object found at the clicked spot")]
    NoObject,
    #[error("the selected region is too large to remove ({0} px across; the limit is {1})")]
    RegionTooLarge(i32, i32),
    #[error("invalid removal request: {0}")]
    BadInput(String),
    #[error("a downloaded model failed its integrity check: {0}")]
    Integrity(String),
}

pub(crate) fn ort_err(e: impl std::fmt::Display) -> RemovalError {
    RemovalError::Ort(e.to_string())
}

/// Random access to the heal stage's input image: linear camera RGB, black-subtracted and scaled
/// to roughly [0, 1] (exactly what `nicti_tapetum::stages::normalize_pixels` produces, but read
/// lazily -- a full-resolution frame as `[f32; 4]` would be ~730 MB).
pub trait PixelSource: Send + Sync {
    fn width(&self) -> u32;
    fn height(&self) -> u32;
    /// `x < width()`, `y < height()`.
    fn pixel(&self, x: u32, y: u32) -> [f32; 3];
}

/// [`PixelSource`] over a decoded RAW frame, using `normalize.wgsl`'s formula.
pub struct FramePixels(pub Arc<LinearFrame>);

impl PixelSource for FramePixels {
    fn width(&self) -> u32 {
        self.0.width
    }
    fn height(&self) -> u32 {
        self.0.height
    }
    fn pixel(&self, x: u32, y: u32) -> [f32; 3] {
        let f = &*self.0;
        let range = f.maximum as f32 - f.black as f32;
        let base = (y as usize * f.width as usize + x as usize) * 3;
        let sample =
            |c: usize| (f.pixels[base + c] as f32 - f.black as f32 - f.cblack[c] as f32) / range;
        [sample(0), sample(1), sample(2)]
    }
}

/// An in-memory [`PixelSource`], used by tests and anywhere a small image is already in hand.
pub struct RgbBuffer {
    pub width: u32,
    pub height: u32,
    pub data: Vec<[f32; 3]>,
}

impl PixelSource for RgbBuffer {
    fn width(&self) -> u32 {
        self.width
    }
    fn height(&self) -> u32 {
        self.height
    }
    fn pixel(&self, x: u32, y: u32) -> [f32; 3] {
        self.data[y as usize * self.width as usize + x as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nicti_tapetum::stages::normalize_pixels;

    #[test]
    fn frame_pixels_match_the_tapetum_normalize_reference() {
        let frame = LinearFrame {
            make: "T".into(),
            model: "S".into(),
            width: 3,
            height: 2,
            black: 100,
            maximum: 1100,
            cam_mul: [2.0, 1.0, 1.5, 1.0],
            pre_mul: [2.0, 1.0, 1.5, 1.0],
            cam_xyz: [0.0; 12],
            cblack: [10, 20, 5, 0],
            pixels: (0..3 * 2 * 3).map(|i| 100 + i * 50).collect(),
        };
        let expected = normalize_pixels(&frame);
        let src = FramePixels(Arc::new(frame));
        for y in 0..2u32 {
            for x in 0..3u32 {
                let e = expected[(y * 3 + x) as usize];
                let a = src.pixel(x, y);
                for c in 0..3 {
                    assert!((a[c] - e[c]).abs() < 1e-6, "({x},{y}) c{c}");
                }
            }
        }
    }
}
