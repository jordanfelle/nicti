//! Local adjustments and masks (#49, ADR-0048 / ADR-0049).
//!
//! - [`params`] -- the `nicti.masks` stage's typed, sanitized params (what is stored).
//! - [`raster`] -- CPU reference rasterizers for geometry and range masks (what the GPU kernels
//!   are checked against, and what tests/outcome checks use).
//! - [`compose`] -- the fold that combines components, the AI bake key, and the document stamp.
//!
//! A mask is a *weight field* in 0..=1 at some pixel extent. Geometry is rasterized live (it has
//! no bake); an AI alpha is baked once per (neutral render, recipe) and only composed here.

pub mod compose;
pub mod params;
pub mod raster;

/// A single-channel `f32` image: one mask weight per pixel, row-major.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub width: usize,
    pub height: usize,
    pub data: Vec<f32>,
}

impl Field {
    pub fn new(width: usize, height: usize, fill: f32) -> Self {
        Self {
            width,
            height,
            data: vec![fill; width * height],
        }
    }

    pub fn get(&self, x: usize, y: usize) -> f32 {
        self.data[y * self.width + x]
    }
}
