//! Collects a tiled render into one crop-sized linear RGB buffer (#57).
//!
//! `TiledRender` hands back RGBA f32 tiles (alpha is always 1 and is dropped here). The buffer is
//! allocated with `try_reserve_exact`, so a 45 MP export on a machine that can't spare ~540 MB
//! fails that one photo with a message instead of aborting the whole process.

use nicti_preen::resize::StreamResizer;
use nicti_tapetum::tile::{Rect, TileSink};

pub struct AccumSink {
    pub width: u32,
    pub height: u32,
    /// Interleaved linear RGB, `width * height * 3`.
    pub pixels: Vec<f32>,
}

impl AccumSink {
    pub fn try_new(width: u32, height: u32) -> Result<Self, String> {
        let too_big = || format!("{width}x{height} is too large to export");
        let len = (width as usize)
            .checked_mul(height as usize)
            .and_then(|n| n.checked_mul(3))
            .ok_or_else(too_big)?;
        let mut pixels = Vec::new();
        pixels.try_reserve_exact(len).map_err(|_| {
            format!(
                "not enough memory to render {width}x{height} ({} MB needed)",
                len * 4 / 1_000_000
            )
        })?;
        pixels.resize(len, 0.0);
        Ok(Self {
            width,
            height,
            pixels,
        })
    }
}

impl TileSink for AccumSink {
    fn write_tile(&mut self, core: Rect, pixels: &[[f32; 4]]) {
        debug_assert_eq!(pixels.len(), (core.width * core.height) as usize);
        for row in 0..core.height {
            let y = core.y + row;
            if y >= self.height {
                break;
            }
            let width = core.width.min(self.width.saturating_sub(core.x)) as usize;
            let dst = ((y * self.width + core.x) * 3) as usize;
            let src = (row * core.width) as usize;
            for i in 0..width {
                let p = pixels[src + i];
                let d = dst + i * 3;
                self.pixels[d] = p[0];
                self.pixels[d + 1] = p[1];
                self.pixels[d + 2] = p[2];
            }
        }
    }
}

/// Resizes tiles to the export size as they arrive (#336): the readback still happens at render
/// resolution, but the full-size frame is never resident, so a 2048 px export of a 45 MP photo
/// holds ~35 MB instead of ~540 MB.
pub struct ShrinkSink {
    resizer: StreamResizer,
}

impl TileSink for ShrinkSink {
    fn write_tile(&mut self, core: Rect, pixels: &[[f32; 4]]) {
        self.resizer
            .add_tile_rgba(core.x, core.y, core.width, core.height, pixels);
    }
}

/// Where a render's tiles go: a full-size buffer, or straight into a shrinking resize.
pub enum RenderSink {
    Full(AccumSink),
    Shrink(ShrinkSink),
}

impl RenderSink {
    /// `shrink` is the export size (from `nicti_preen::resize::shrink_target`) when the export
    /// shrinks the render; `None` keeps the full-size buffer.
    pub fn try_new(width: u32, height: u32, shrink: Option<(u32, u32)>) -> Result<Self, String> {
        match shrink {
            None => AccumSink::try_new(width, height).map(Self::Full),
            Some((dw, dh)) => StreamResizer::try_new(width, height, dw, dh)
                .map(|resizer| Self::Shrink(ShrinkSink { resizer }))
                .map_err(|e| e.to_string()),
        }
    }

    /// The finished frame's size and interleaved linear RGB pixels.
    pub fn into_frame(self) -> (u32, u32, Vec<f32>) {
        match self {
            Self::Full(s) => (s.width, s.height, s.pixels),
            Self::Shrink(s) => {
                let (w, h) = s.resizer.dst_size();
                (w, h, s.resizer.finish())
            }
        }
    }
}

impl TileSink for RenderSink {
    fn write_tile(&mut self, core: Rect, pixels: &[[f32; 4]]) {
        match self {
            Self::Full(s) => s.write_tile(core, pixels),
            Self::Shrink(s) => s.write_tile(core, pixels),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiles_land_at_their_offsets_with_alpha_dropped() {
        let mut sink = AccumSink::try_new(4, 3).unwrap();
        let tile = Rect {
            x: 2,
            y: 1,
            width: 2,
            height: 2,
        };
        let px = [
            [1.0, 2.0, 3.0, 1.0],
            [4.0, 5.0, 6.0, 1.0],
            [7.0, 8.0, 9.0, 1.0],
            [10.0, 11.0, 12.0, 1.0],
        ];
        sink.write_tile(tile, &px);
        let at = |x: usize, y: usize| &sink.pixels[(y * 4 + x) * 3..(y * 4 + x) * 3 + 3];
        assert_eq!(at(2, 1), [1.0, 2.0, 3.0]);
        assert_eq!(at(3, 1), [4.0, 5.0, 6.0]);
        assert_eq!(at(2, 2), [7.0, 8.0, 9.0]);
        assert_eq!(at(3, 2), [10.0, 11.0, 12.0]);
        assert_eq!(at(0, 0), [0.0, 0.0, 0.0]);
    }

    #[test]
    fn an_absurd_size_is_an_error_not_an_abort() {
        assert!(AccumSink::try_new(u32::MAX, u32::MAX).is_err());
        // Fits in usize on 64-bit but no machine can back it.
        assert!(AccumSink::try_new(400_000, 400_000).is_err());
    }

    #[test]
    fn a_shrinking_sink_never_holds_the_full_frame_and_matches_a_resize_of_it() {
        let (w, h, dw, dh) = (64u32, 48u32, 16u32, 12u32);
        let full: Vec<f32> = (0..w * h * 3)
            .map(|i| ((i * 37) % 101) as f32 / 101.0)
            .collect();
        let mut sink = RenderSink::try_new(w, h, Some((dw, dh))).unwrap();
        for ty in (0..h).step_by(20) {
            for tx in (0..w).step_by(30) {
                let (tw, th) = (30.min(w - tx), 20.min(h - ty));
                let mut px = Vec::new();
                for y in 0..th {
                    for x in 0..tw {
                        let i = (((ty + y) * w + tx + x) * 3) as usize;
                        px.push([full[i], full[i + 1], full[i + 2], 1.0]);
                    }
                }
                sink.write_tile(
                    Rect {
                        x: tx,
                        y: ty,
                        width: tw,
                        height: th,
                    },
                    &px,
                );
            }
        }
        let (ow, oh, out) = sink.into_frame();
        assert_eq!((ow, oh), (dw, dh));
        let want = nicti_preen::resize::resize_linear_f32(full, w, h, dw, dh).unwrap();
        let worst = out
            .iter()
            .zip(&want)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(worst < 2e-3, "worst {worst}");
        assert!(matches!(
            RenderSink::try_new(w, h, None).unwrap(),
            RenderSink::Full(_)
        ));
    }
}
