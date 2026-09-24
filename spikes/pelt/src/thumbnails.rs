//! Synthetic thumbnail tile pool for the virtualized-grid interaction. Toolkit-agnostic: produces
//! plain `Vec<u8>` RGBA8 pixel data, which every candidate's own texture/image type is built from
//! at the glue layer in `spikes/pelt-*`.

use crate::config::{DISTINCT_TILE_COUNT, THUMB_TILE_SIZE};

/// One synthetic `THUMB_TILE_SIZE`^2 RGBA8 tile, deterministically generated from `index` so a
/// re-run reproduces the exact same pool without storing real image bytes.
pub fn generate_tile(index: usize) -> Vec<u8> {
    let size = THUMB_TILE_SIZE as usize;
    let mut pixels = Vec::with_capacity(size * size * 4);
    // A simple per-tile hue-ish gradient plus a border, distinct enough per index to be visually
    // checkable during manual smoke testing, cheap enough to regenerate 4096 times at startup.
    let base = (index as u32).wrapping_mul(2654435761) as u8;
    for y in 0..size {
        for x in 0..size {
            let is_border = x < 4 || y < 4 || x >= size - 4 || y >= size - 4;
            if is_border {
                pixels.extend_from_slice(&[base, base, base, 255]);
            } else {
                let r = ((x * 255) / size) as u8;
                let g = ((y * 255) / size) as u8;
                let b = base;
                pixels.extend_from_slice(&[r, g, b, 255]);
            }
        }
    }
    pixels
}

/// The full pool of [`DISTINCT_TILE_COUNT`] distinct tiles, generated once at startup.
pub fn generate_tile_pool() -> Vec<Vec<u8>> {
    (0..DISTINCT_TILE_COUNT).map(generate_tile).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_has_expected_byte_length() {
        let tile = generate_tile(0);
        assert_eq!(
            tile.len(),
            THUMB_TILE_SIZE as usize * THUMB_TILE_SIZE as usize * 4
        );
    }

    #[test]
    fn distinct_indices_produce_distinct_tiles() {
        assert_ne!(generate_tile(0), generate_tile(1));
    }

    #[test]
    fn tile_generation_is_deterministic() {
        assert_eq!(generate_tile(42), generate_tile(42));
    }

    #[test]
    fn pool_has_expected_size() {
        let pool = generate_tile_pool();
        assert_eq!(pool.len(), DISTINCT_TILE_COUNT);
    }
}
