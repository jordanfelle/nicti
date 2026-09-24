//! Shared spike parameters every `pelt-*` binary drives its UI from, so the four candidates are
//! measured against the identical workload rather than four subtly different ones.

/// Cell count for the virtualized grid interaction. 2,000,000 matches ADR-0006/#68's catalog-scale
/// gate (600k assets today, 2M design headroom per #4/E0's requirements) rather than the current
/// real catalog size, since the grid must not regress as the catalog grows.
pub const GRID_CELL_COUNT: usize = 2_000_000;

/// Thumbnail tile side length in pixels, matching a typical grid-view cell size.
pub const THUMB_TILE_SIZE: u32 = 256;

/// Distinct synthetic tiles backing the grid — realistic texture-atlas/cache churn without
/// generating 2M unique images. `cell_to_tile_index` maps a cell to one of these by hash.
pub const DISTINCT_TILE_COUNT: usize = 4096;

/// Loupe/hero-scenario proxy: 50 synthetic frames, matching the hero-scenario working-set size
/// (`docs/benchmarks/hero-scenario.md`) so the loupe next/prev interaction cycles the same count
/// of images LRC's own baseline does.
pub const LOUPE_FRAME_COUNT: usize = 50;

/// Loupe/viewport resolution for the live-chain compute proxy: 1080p, a representative on-screen
/// develop-module viewport size (not the 45MP hero-scenario resolution — this spike measures UI
/// input latency, not GPU compute throughput, which `spikes/glint` already covers).
pub const VIEWPORT_WIDTH: u32 = 1920;
pub const VIEWPORT_HEIGHT: u32 = 1080;

/// Maps a grid cell index to one of [`DISTINCT_TILE_COUNT`] synthetic tile identities. A cheap
/// multiplicative hash (Knuth's) is enough here: the point is spreading cell->tile assignment
/// non-trivially, not cryptographic quality.
pub fn cell_to_tile_index(cell: usize) -> usize {
    let hashed = (cell as u64).wrapping_mul(0x9E3779B97F4A7C15);
    (hashed % DISTINCT_TILE_COUNT as u64) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_index_in_range() {
        for cell in [0, 1, 2, GRID_CELL_COUNT / 2, GRID_CELL_COUNT - 1] {
            assert!(cell_to_tile_index(cell) < DISTINCT_TILE_COUNT);
        }
    }

    #[test]
    fn tile_index_deterministic() {
        assert_eq!(cell_to_tile_index(12345), cell_to_tile_index(12345));
    }

    #[test]
    fn tile_index_spreads_adjacent_cells() {
        // Not a strict requirement, just documents intent: neighboring cells shouldn't all
        // collapse onto the same tile, which would understate real texture-atlas churn.
        let a = cell_to_tile_index(1000);
        let b = cell_to_tile_index(1001);
        assert_ne!(
            a, b,
            "adjacent cells hashed to the same tile -- weak spread"
        );
    }
}
