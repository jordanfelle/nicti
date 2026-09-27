//! Discrete-event simulation of #43's hero-scenario bulk-sync bake queue: 50 images, synced once
//! (decode+demosaic+denoise+mask bake per image), then the cursor walks through them in order.
//! This is the "secondary metric" #43's spec asks for (wall-clock sync-to-done) plus a
//! stale-count metric this ADR adds: how many images the cursor reaches *before* their bake
//! finishes, each of which falls back to #145's stale-while-baking live render.
//!
//! **This is a simulation, not a measurement** -- it uses `cost_model`'s per-image bake-stage
//! durations (real for decode/denoise, a labelled hypothesis for mask bake) and a single serial
//! bake worker (one shared `wgpu::Device`/one `ort::Session`, per ADR-0019/0016/0050's existing
//! "one shared GPU device" decisions -- Tapetum doesn't get to assume parallel GPU bake workers
//! just because it wants a shorter sim number). It reports what the *scheduling policy* achieves
//! given those costs, not what real end-to-end wall-clock time will be once #45 actually builds
//! the pipeline.
//!
//! **The pending-job priority order is recomputed every time the bake worker picks its next job**,
//! using the cursor's position *at that moment* (not the position it started at) -- a CodeRabbit
//! review of this PR found the original version fixed the whole bake order once, at
//! `cursor_start`, which silently stopped matching `prefetch`'s own "reprioritized immediately
//! whenever the cursor moves" contract the moment the cursor moved at all, and could misreport
//! `stale_at_arrival` as a result (worked example: 5 images, `cursor_start = 2`, 130ms walk pace,
//! 60ms job cost -- the fixed-order version bakes image 4 at 300ms, after its own 260ms arrival,
//! and counts it stale; the corrected version reprioritizes toward the cursor's real position once
//! it has moved past image 3, bakes image 4 at 240ms -- before its 260ms arrival -- and correctly
//! does not count it stale).

use std::collections::BTreeSet;
use std::time::Duration;

use crate::cost_model;
use crate::prefetch;

#[derive(Debug, Clone, Copy)]
pub struct BakeCost {
    pub decode: Duration,
    pub denoise: Duration,
    /// One mask bake per image, not two -- the hero scenario's "Select Subject" +
    /// "Select Subject, Invert" pair share one `ai_bake_key` (ADR-0048's `compose.rs` finding:
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
    /// Total wall-clock time until every image is baked -- #43's secondary metric. Independent of
    /// scheduling policy: a serial worker processes every job exactly once, so the sum of all job
    /// costs is invariant regardless of order.
    pub total_wall_time: Duration,
    /// Time until the cursor's starting image (index 0, the hero scenario's own starting point)
    /// is baked -- the number a user waiting to see their first synced result actually feels.
    pub first_image_ready: Duration,
    /// How many images the cursor reaches (walking `cursor_start..n_images` at `walk_pace` per
    /// step) before that image's own bake has finished -- each one falls back to a live/stale
    /// render.
    pub stale_at_arrival: usize,
}

/// The cursor's position at wall-clock `clock`, walking forward from `cursor_start` one image
/// every `walk_pace` (the hero scenario's own "right-arrow through all 50" walk,
/// `docs/benchmarks/hero-scenario.md`), clamped to the last image once it reaches the end. A zero
/// `walk_pace` is treated as an instant walk straight to the last image.
fn cursor_at(cursor_start: usize, n_images: usize, walk_pace: Duration, clock: Duration) -> usize {
    let last = n_images.saturating_sub(1);
    if walk_pace.is_zero() {
        return last;
    }
    let steps = (clock.as_secs_f64() / walk_pace.as_secs_f64()).floor();
    if steps <= 0.0 {
        return cursor_start.min(last);
    }
    (cursor_start + steps as usize).min(last)
}

/// Runs the sim: `n_images` bake jobs on one serial worker. Before picking each job, the pending
/// set is re-sorted by distance from the cursor's *current* position (`cursor_at`, evaluated at
/// the wall-clock moment the worker becomes free) -- matching `prefetch::priority_order`'s own
/// "reprioritized immediately whenever the cursor moves" contract, not a one-time snapshot of the
/// order at `cursor_start`.
pub fn simulate_hero_bake(
    n_images: usize,
    cursor_start: usize,
    walk_pace: Duration,
    cost: BakeCost,
) -> HeroSimResult {
    let mut pending: BTreeSet<usize> = (0..n_images).collect();
    let mut bake_finish = vec![Duration::ZERO; n_images];
    let mut clock = Duration::ZERO;

    while !pending.is_empty() {
        let cursor = cursor_at(cursor_start, n_images, walk_pace, clock);
        let order = prefetch::priority_order(&pending, cursor);
        let next = order[0];
        clock += cost.total();
        bake_finish[next] = clock;
        pending.remove(&next);
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
        // Cursor starts at 2 and (at this fast walk pace) reaches the last image almost
        // immediately, but the very first pick is still nearest to the *starting* cursor.
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
        // walk_pace much longer than the whole queue's total time -- the cursor never moves
        // during the simulation at all, so this also exercises the "no reprioritization needed"
        // case. Every image except the very first is baked well before the cursor arrives. The
        // first image (arrival at t=0) is unavoidably stale for its own bake duration no matter
        // how slow the walk pace is -- you can't view a baked result before baking has had time to
        // run at all, which is exactly why #145's stale-while-baking fallback exists in the first
        // place.
        let cost = tiny_cost();
        let result = simulate_hero_bake(5, 0, Duration::from_secs(10), cost);
        assert_eq!(result.stale_at_arrival, 1);
    }

    /// Regression test for the CodeRabbit-found reprioritization bug, using its own worked
    /// example: 5 images, cursor starting at 2, a 130ms walk pace, and a 60ms job cost. The cursor
    /// reaches image 3 (130ms away from the start) partway through the simulation, so the bake
    /// worker's 4th pick should reprioritize toward the cursor's new position (image 4, now
    /// nearest) rather than continuing the order implied by the *starting* cursor position (which
    /// would pick image 0 next). See this module's own doc comment for the full worked timeline.
    #[test]
    fn reprioritizes_toward_the_cursors_moved_position_mid_simulation() {
        let cost = BakeCost {
            decode: Duration::from_millis(60),
            denoise: Duration::ZERO,
            mask_bake: Duration::ZERO,
        };
        let result = simulate_hero_bake(5, 2, Duration::from_millis(130), cost);

        // Expected bake order (see module doc comment): 2, 1, 3, 4, 0 -- finishing at
        // 60/120/180/240/300ms respectively.
        assert_eq!(result.bake_finish[2], Duration::from_millis(60));
        assert_eq!(result.bake_finish[1], Duration::from_millis(120));
        assert_eq!(result.bake_finish[3], Duration::from_millis(180));
        assert_eq!(result.bake_finish[4], Duration::from_millis(240));
        assert_eq!(result.bake_finish[0], Duration::from_millis(300));

        // Image 4 arrives at 260ms (130ms * 2 steps from cursor_start=2) -- after its own 240ms
        // bake finish, so it must NOT be counted stale. A fixed-order simulation (bake order
        // 2,1,3,0,4, ignoring the cursor's movement) would instead finish image 4 at 300ms, after
        // its arrival, and wrongly count it stale.
        let arrival_image_4 = Duration::from_millis(130) * 2;
        assert!(
            arrival_image_4 > result.bake_finish[4],
            "image 4 should finish before its own arrival once the cursor's movement is accounted for"
        );
        assert_eq!(
            result.stale_at_arrival, 2,
            "only images 2 and 3 should be stale; image 4 finishes before its own arrival"
        );
    }

    #[test]
    fn cursor_at_clamps_to_the_last_image_once_the_walk_reaches_the_end() {
        assert_eq!(
            cursor_at(0, 5, Duration::from_millis(1), Duration::from_secs(1)),
            4
        );
        assert_eq!(cursor_at(2, 5, Duration::from_millis(1), Duration::ZERO), 2);
    }
}
