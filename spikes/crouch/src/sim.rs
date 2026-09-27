//! Extends `spikes/loaf/src/sim.rs`'s hero-scenario bake-queue simulation (copied, not
//! depended-on) with tile-granular preemption: each image's bake job is now split into the same
//! stage chunks a real worker would actually submit (one atomic decode, N denoise tiles, one
//! atomic mask bake), and a periodic foreground demand (representing the user still touching
//! live sliders during a bulk sync, not just walking the cursor) can only be serviced at a chunk
//! boundary -- per `job.rs`'s own cooperative-cancellation contract, never mid-chunk.
//!
//! **The load-bearing finding this adds over `loaf`'s own sim**: chunking denoise into tiles
//! bounds *its own* worst-case preemption latency by one tile's duration, but decode and mask
//! bake are still modeled as atomic (a single-file LibRaw decode, a whole-image mask-model
//! inference) -- so the real worst-case foreground latency is bounded by whichever atomic stage
//! is in flight when a foreground request arrives, not by the tile chunk size alone. See
//! `docs/adr/0054`'s Measured results for what this means with ADR-0037/0040's real per-stage
//! costs plugged in.

use std::collections::BTreeSet;
use std::time::Duration;

use crate::prefetch;

#[derive(Debug, Clone, Copy)]
pub struct ChunkedBakeCost {
    /// A single-file LibRaw decode -- not chunked in this model (ADR-0037 measured it as one
    /// atomic operation; streaming/tiled decode isn't something #37/#40 built).
    pub decode: Duration,
    /// The whole image's total denoise cost (ADR-0040's real full-res SCUNet figure) -- split
    /// into `denoise_chunk`-sized tiles for this sim, matching how `spikes/rods::ai::TiledDenoiser`
    /// actually runs it.
    pub denoise_total: Duration,
    /// One tile's duration -- the real cancellation/contention granularity `job.rs::ChunkedJob`
    /// asks for.
    pub denoise_chunk: Duration,
    /// AI mask bake -- not chunked in this model (ADR-0048's own hypothesis is a single
    /// preview-resolution inference call, not a tiled one).
    pub mask_bake: Duration,
}

impl ChunkedBakeCost {
    /// The chunk-duration list this job's bake actually executes, in order: one decode chunk, N
    /// denoise chunks (the last one truncated to whatever remains, never rounded up past the real
    /// total), one mask-bake chunk.
    fn chunks(&self) -> Vec<Duration> {
        let mut out = Vec::new();
        if !self.decode.is_zero() {
            out.push(self.decode);
        }
        let mut remaining = self.denoise_total;
        while !remaining.is_zero() {
            let chunk = remaining.min(self.denoise_chunk);
            out.push(chunk);
            remaining = remaining.saturating_sub(chunk);
        }
        if !self.mask_bake.is_zero() {
            out.push(self.mask_bake);
        }
        out
    }

    pub fn total(&self) -> Duration {
        self.decode + self.denoise_total + self.mask_bake
    }

    /// The largest single atomic unit this job ever runs without a foreground-preemption
    /// opportunity -- the real worst-case bound on foreground latency, per this module's own doc
    /// comment (decode and mask bake aren't chunked here, so whichever is larger than
    /// `denoise_chunk` sets the bound).
    pub fn worst_case_atomic_unit(&self) -> Duration {
        self.decode.max(self.denoise_chunk).max(self.mask_bake)
    }
}

#[derive(Debug, Clone)]
pub struct ChunkedSimResult {
    pub bake_finish: Vec<Duration>,
    pub total_wall_time: Duration,
    pub first_image_ready: Duration,
    pub stale_at_arrival: usize,
    /// One entry per serviced foreground request: how late it was served past when it became due
    /// (i.e. past the previous multiple of `foreground_interval`) -- bounded, per this module's
    /// doc comment, by whichever atomic chunk was in flight at the moment it became due.
    pub foreground_latencies: Vec<Duration>,
}

impl ChunkedSimResult {
    pub fn foreground_worst_latency(&self) -> Duration {
        self.foreground_latencies
            .iter()
            .copied()
            .max()
            .unwrap_or_default()
    }
}

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

/// Runs the chunked sim. `foreground_interval` is how often a foreground request becomes due
/// (zero disables foreground demand entirely, reducing to `loaf`'s own unchunked-cost behavior
/// modulo the chunk split); `foreground_cost` is how long the worker spends servicing one before
/// resuming the background job it interrupted.
pub fn simulate_hero_bake_chunked(
    n_images: usize,
    cursor_start: usize,
    walk_pace: Duration,
    cost: ChunkedBakeCost,
    foreground_interval: Duration,
    foreground_cost: Duration,
) -> ChunkedSimResult {
    let mut pending: BTreeSet<usize> = (0..n_images).collect();
    let mut bake_finish = vec![Duration::ZERO; n_images];
    let mut clock = Duration::ZERO;
    let mut next_foreground_due = foreground_interval;
    let mut foreground_latencies = Vec::new();

    while !pending.is_empty() {
        let cursor = cursor_at(cursor_start, n_images, walk_pace, clock);
        let order = prefetch::priority_order(&pending, cursor);
        let next = order[0];

        for chunk in cost.chunks() {
            clock += chunk;
            if !foreground_interval.is_zero() {
                while next_foreground_due <= clock {
                    let latency = clock.saturating_sub(next_foreground_due);
                    foreground_latencies.push(latency);
                    clock += foreground_cost;
                    next_foreground_due += foreground_interval;
                }
            }
        }

        bake_finish[next] = clock;
        pending.remove(&next);
    }

    let total_wall_time = clock;
    let first_image_ready = bake_finish.get(cursor_start).copied().unwrap_or_default();
    let stale_at_arrival = (cursor_start..n_images)
        .map(|i| (i, walk_pace * (i - cursor_start) as u32))
        .filter(|&(i, arrival)| arrival < bake_finish[i])
        .count();

    ChunkedSimResult {
        bake_finish,
        total_wall_time,
        first_image_ready,
        stale_at_arrival,
        foreground_latencies,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_cost() -> ChunkedBakeCost {
        ChunkedBakeCost {
            decode: Duration::from_millis(10),
            denoise_total: Duration::from_millis(90),
            denoise_chunk: Duration::from_millis(30),
            mask_bake: Duration::from_millis(10),
        }
    }

    #[test]
    fn chunks_splits_denoise_into_equal_tiles_when_evenly_divisible() {
        let cost = tiny_cost();
        let chunks = cost.chunks();
        assert_eq!(
            chunks,
            vec![
                Duration::from_millis(10),
                Duration::from_millis(30),
                Duration::from_millis(30),
                Duration::from_millis(30),
                Duration::from_millis(10),
            ]
        );
    }

    #[test]
    fn chunks_truncates_the_last_denoise_tile_to_whatever_remains() {
        let cost = ChunkedBakeCost {
            decode: Duration::ZERO,
            denoise_total: Duration::from_millis(100),
            denoise_chunk: Duration::from_millis(30),
            mask_bake: Duration::ZERO,
        };
        let chunks = cost.chunks();
        // 30+30+30+10 = 100, never rounds up past the real total.
        assert_eq!(chunks.iter().sum::<Duration>(), Duration::from_millis(100));
        assert_eq!(*chunks.last().unwrap(), Duration::from_millis(10));
    }

    #[test]
    fn no_foreground_demand_matches_total_cost_times_image_count() {
        let cost = tiny_cost();
        let result = simulate_hero_bake_chunked(
            5,
            0,
            Duration::from_millis(1),
            cost,
            Duration::ZERO,
            Duration::ZERO,
        );
        assert_eq!(result.total_wall_time, cost.total() * 5);
        assert!(result.foreground_latencies.is_empty());
    }

    #[test]
    fn foreground_demand_adds_its_own_cost_to_total_wall_time() {
        let cost = tiny_cost();
        let without_fg = simulate_hero_bake_chunked(
            3,
            0,
            Duration::from_secs(10),
            cost,
            Duration::ZERO,
            Duration::ZERO,
        );
        let with_fg = simulate_hero_bake_chunked(
            3,
            0,
            Duration::from_secs(10),
            cost,
            Duration::from_millis(25),
            Duration::from_millis(5),
        );
        assert!(with_fg.total_wall_time > without_fg.total_wall_time);
        assert!(!with_fg.foreground_latencies.is_empty());
    }

    #[test]
    fn foreground_latency_never_exceeds_the_worst_case_atomic_unit() {
        // A foreground request due mid-chunk can only be serviced once that chunk's boundary is
        // reached -- bounded by the largest atomic unit this job ever runs uninterrupted.
        let cost = tiny_cost();
        let bound = cost.worst_case_atomic_unit();
        let result = simulate_hero_bake_chunked(
            10,
            0,
            Duration::from_secs(10),
            cost,
            Duration::from_millis(7), // deliberately not a multiple of any chunk size
            Duration::from_millis(2),
        );
        assert!(!result.foreground_latencies.is_empty());
        for latency in &result.foreground_latencies {
            assert!(
                *latency <= bound,
                "latency {latency:?} exceeded the worst-case atomic unit {bound:?}"
            );
        }
    }

    #[test]
    fn worst_case_atomic_unit_is_the_largest_unchunked_stage() {
        let cost = ChunkedBakeCost {
            decode: Duration::from_millis(1700),
            denoise_total: Duration::from_millis(50_900),
            denoise_chunk: Duration::from_millis(45),
            mask_bake: Duration::from_millis(1000),
        };
        // Decode (1700ms) dwarfs both the denoise chunk (45ms) and mask bake (1000ms) -- real
        // ADR-0037/0040/0048 figures plugged in.
        assert_eq!(cost.worst_case_atomic_unit(), Duration::from_millis(1700));
    }
}
