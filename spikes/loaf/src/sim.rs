//! Discrete-event simulation of #43's hero-scenario bulk-sync bake queue: 50 images, synced once
//! (decode+demosaic+denoise+mask bake per image), then the cursor walks through them in order.
//! This is the "secondary metric" #43's spec asks for (wall-clock sync-to-done) plus a
//! stale-count metric this ADR adds: how many images the cursor reaches *before* their bake
//! finishes, each of which falls back to #145's stale-while-baking live render.
//!
//! **This is a simulation, not a measurement** -- it uses `cost_model`'s per-image bake-stage
//! durations (real for decode/denoise, a labelled hypothesis for mask bake) and a single serial
//! bake worker (one shared `wgpu::Device`/one `ort::Session`, per ADR-0004/0005/0007's existing
//! "one shared GPU device" decisions -- Tapetum doesn't get to assume parallel GPU bake workers
//! just because it wants a shorter sim number). It reports what the *scheduling policy* achieves
//! given those costs, not what real end-to-end wall-clock time will be once #45 actually builds
//! the pipeline.

use std::collections::BTreeSet;
use std::time::Duration;

use crate::cost_model;
use crate::prefetch;

#[derive(Debug, Clone, Copy)]
pub struct BakeCost {
    pub decode: Duration,
    pub denoise: Duration,
    /// One mask bake per image, not two -- the hero scenario's "Select Subject" +
    /// "Select Subject, Invert" pair share one `ai_bake_key` (ADR-0024's `compose.rs` finding:
    /// `bake_keys_dedupe_the_shared_recipe_between_a_mask_and_its_inverse`), so the model runs
    /// once per image, not once per mask.
    pub mask_bake: Duration,
}

impl BakeCost {
    pub fn total(&self) -> Duration {
        self.decode + self.denoise + self.mask_bake
    }
}

impl Default for BakeCost {
    fn default() -> Self {
        Self {
            decode: cost_model::DECODE_MID,
            denoise: cost_model::DENOISE_FULL_RES,
            mask_bake: cost_model::MASK_BAKE_PREVIEW_RES_HYPOTHESIS,
        }
    }
}

#[derive(Debug, Clone)]
pub struct HeroSimResult {
    /// `bake_finish[i]` is the wall-clock time (from sync start) at which image `i`'s bake
    /// completed, indexed by image index (not bake order).
    pub bake_finish: Vec<Duration>,
    /// Total wall-clock time until every image is baked -- #43's secondary metric.
    pub total_wall_time: Duration,
    /// Time until the cursor's starting image (index 0, the hero scenario's own starting point)
    /// is baked -- the number a user waiting to see their first synced result actually feels.
    pub first_image_ready: Duration,
    /// How many images the cursor reaches (walking 0..n at `walk_pace` per step) before that
    /// image's own bake has finished -- each one falls back to a live/stale render.
    pub stale_at_arrival: usize,
}

/// Runs the sim: `n_images` bake jobs, prioritized nearest-to-`cursor_start` first
/// (`prefetch::priority_order`), processed by one serial worker, while a cursor walks
/// sequentially from `cursor_start` to `n_images - 1` at `walk_pace` per step (the hero
/// scenario's own "right-arrow through all 50" walk, `docs/benchmarks/hero-scenario.md`).
pub fn simulate_hero_bake(
    n_images: usize,
    cursor_start: usize,
    walk_pace: Duration,
    cost: BakeCost,
) -> HeroSimResult {
    let pending: BTreeSet<usize> = (0..n_images).collect();
    let order = prefetch::priority_order(&pending, cursor_start);

    let mut bake_finish = vec![Duration::ZERO; n_images];
    let mut clock = Duration::ZERO;
    for image in order {
        clock += cost.total();
        bake_finish[image] = clock;
    }
    let total_wall_time = clock;
    let first_image_ready = bake_finish.get(cursor_start).copied().unwrap_or_default();

    let stale_at_arrival = (cursor_start..n_images)
        .map(|i| {
            let arrival = walk_pace * (i - cursor_start) as u32;
            (i, arrival)
        })
        .filter(|&(i, arrival)| arrival < bake_finish[i])
        .count();

    HeroSimResult {
        bake_finish,
        total_wall_time,
        first_image_ready,
        stale_at_arrival,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_cost() -> BakeCost {
        BakeCost {
            decode: Duration::from_millis(10),
            denoise: Duration::from_millis(40),
            mask_bake: Duration::from_millis(10),
        }
    }

    #[test]
    fn nearest_to_cursor_finishes_before_farther_images() {
        let result = simulate_hero_bake(5, 2, Duration::from_millis(1), tiny_cost());
        // Cursor starts at 2; images 2, then {1,3}, then {0,4} bake in that priority order, so
        // image 2 must finish no later than any other image.
        assert!(result.bake_finish[2] <= result.bake_finish[1]);
        assert!(result.bake_finish[2] <= result.bake_finish[3]);
        assert!(result.bake_finish[1] <= result.bake_finish[0]);
    }

    #[test]
    fn total_wall_time_is_sum_of_all_job_costs_regardless_of_order() {
        let cost = tiny_cost();
        let result = simulate_hero_bake(5, 0, Duration::from_millis(1), cost);
        assert_eq!(result.total_wall_time, cost.total() * 5);
    }

    #[test]
    fn a_fast_walk_pace_arrives_before_bake_finishes_for_every_image_but_the_first() {
        // walk_pace much shorter than a single bake job's cost -- the cursor races ahead of the
        // (serial, one-worker) bake queue for every image after the first.
        let cost = tiny_cost();
        let result = simulate_hero_bake(5, 0, Duration::from_millis(1), cost);
        assert!(result.stale_at_arrival >= 4);
    }

    #[test]
    fn a_slow_walk_pace_lets_the_bake_queue_keep_up_after_the_first_image() {
        // walk_pace much longer than the whole queue's total time -- every image except the very
        // first is baked well before the cursor arrives. The first image (arrival at t=0) is
        // unavoidably stale for its own bake duration no matter how slow the walk pace is -- you
        // can't view a baked result before baking has had time to run at all, which is exactly
        // why #145's stale-while-baking fallback exists in the first place.
        let cost = tiny_cost();
        let result = simulate_hero_bake(5, 0, Duration::from_secs(10), cost);
        assert_eq!(result.stale_at_arrival, 1);
    }
}
