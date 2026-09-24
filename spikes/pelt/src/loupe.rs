//! Synthetic loupe frame set: `LOUPE_FRAME_COUNT` distinct RGBA8 images at a representative
//! on-screen loupe resolution, cycled via next/prev to measure switch-settled latency the same
//! way `docs/benchmarks/hero-scenario.md` interaction A does against LRC.

use crate::config::LOUPE_FRAME_COUNT;

pub const LOUPE_WIDTH: u32 = 1600;
pub const LOUPE_HEIGHT: u32 = 1200;

/// One synthetic loupe-resolution RGBA8 frame, distinct per index.
pub fn generate_loupe_frame(index: usize) -> Vec<u8> {
    let w = LOUPE_WIDTH as usize;
    let h = LOUPE_HEIGHT as usize;
    let mut pixels = Vec::with_capacity(w * h * 4);
    let hue_shift = (index as u32).wrapping_mul(97) as u8;
    for y in 0..h {
        for x in 0..w {
            let r = ((x * 255) / w) as u8;
            let g = ((y * 255) / h) as u8;
            pixels.extend_from_slice(&[r, g, hue_shift, 255]);
        }
    }
    pixels
}

/// The full `LOUPE_FRAME_COUNT`-frame set, generated once at startup.
pub fn generate_loupe_set() -> Vec<Vec<u8>> {
    (0..LOUPE_FRAME_COUNT).map(generate_loupe_frame).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_has_expected_byte_length() {
        let frame = generate_loupe_frame(0);
        assert_eq!(
            frame.len(),
            LOUPE_WIDTH as usize * LOUPE_HEIGHT as usize * 4
        );
    }

    #[test]
    fn distinct_frames_differ() {
        assert_ne!(generate_loupe_frame(0), generate_loupe_frame(1));
    }

    #[test]
    fn set_has_expected_size() {
        assert_eq!(generate_loupe_set().len(), LOUPE_FRAME_COUNT);
    }
}
