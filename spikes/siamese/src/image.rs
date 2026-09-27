//! Plain-`f32` image types, row-major -- same shape as `spikes/groom/src/cpu_reference.rs::Image`
//! (this spike doesn't depend on groom; small enough to duplicate rather than couple two
//! throwaway spikes together).

/// A single RGBA `f32` image. `f32` here simulates linear RGBA16F, matching groom's convention.
#[derive(Debug, Clone)]
pub struct Image {
    pub width: usize,
    pub height: usize,
    pub data: Vec<[f32; 4]>,
}

impl Image {
    pub fn new(width: usize, height: usize, fill: [f32; 4]) -> Self {
        Self {
            width,
            height,
            data: vec![fill; width * height],
        }
    }

    #[inline]
    pub fn index(&self, x: i32, y: i32) -> Option<usize> {
        if x < 0 || y < 0 || x as usize >= self.width || y as usize >= self.height {
            None
        } else {
            Some(y as usize * self.width + x as usize)
        }
    }

    pub fn get(&self, x: i32, y: i32) -> [f32; 4] {
        self.index(x, y).map(|i| self.data[i]).unwrap_or([0.0; 4])
    }

    pub fn set(&mut self, x: i32, y: i32, v: [f32; 4]) {
        if let Some(i) = self.index(x, y) {
            self.data[i] = v;
        }
    }
}

/// A single-channel `f32` field, row-major -- masks/alpha, `1.0` = fully selected/applied.
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

    #[inline]
    pub fn index(&self, x: i32, y: i32) -> Option<usize> {
        if x < 0 || y < 0 || x as usize >= self.width || y as usize >= self.height {
            None
        } else {
            Some(y as usize * self.width + x as usize)
        }
    }

    pub fn get(&self, x: i32, y: i32) -> f32 {
        self.index(x, y).map(|i| self.data[i]).unwrap_or(0.0)
    }

    pub fn set(&mut self, x: i32, y: i32, v: f32) {
        if let Some(i) = self.index(x, y) {
            self.data[i] = v;
        }
    }

    /// Nearest-neighbor resample to a different resolution -- used only by tests/CLI to build
    /// synthetic inputs at an arbitrary size; `refine::guided_upsample` is the real preview-to-full
    /// path and does its own edge-aware interpolation, not this.
    pub fn resample_nearest(&self, new_width: usize, new_height: usize) -> Field {
        let mut out = Field::new(new_width, new_height, 0.0);
        if self.width == 0 || self.height == 0 || new_width == 0 || new_height == 0 {
            return out;
        }
        for oy in 0..new_height {
            for ox in 0..new_width {
                let sx = (ox * self.width / new_width).min(self.width - 1);
                let sy = (oy * self.height / new_height).min(self.height - 1);
                out.data[oy * new_width + ox] = self.data[sy * self.width + sx];
            }
        }
        out
    }
}
