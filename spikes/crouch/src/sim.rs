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
    /// The denoise stage's tile list: `denoise_total` split into `denoise_chunk`-sized tiles,
    /// the last one truncated to whatever remains, never rounded up past the real total.
    fn denoise_chunks(&self) -> Vec<Duration> {
        let mut out = Vec::new();
        let mut remaining = self.denoise_total;
        // A zero-sized `denoise_chunk` with a nonzero `denoise_total` can't make progress through
        // the loop below (`remaining.min(ZERO)` is always `ZERO`, so `remaining` never shrinks) --
        // treat it as unchunked (one atomic chunk covering the whole total) rather than spinning
        // forever. This is directly reachable from `bin/crouch.rs`'s `sim --denoise-chunk-ms 0`.
        if self.denoise_chunk.is_zero() {
            if !remaining.is_zero() {
                out.push(remaining);
            }
        } else {
            while !remaining.is_zero() {
                let chunk = remaining.min(self.denoise_chunk);
                out.push(chunk);
                remaining = remaining.saturating_sub(chunk);
            }
        }
        out
    }

    /// The GPU lane's chunk list under the two-lane model (`simulate_hero_bake_two_lane`): the
    /// denoise tiles and the mask bake, without decode -- decode is CPU-only LibRaw work that runs
    /// on `Lane::Cpu` (`nicti-pelt`'s `decode_job.rs`), never serialized behind this lane.
    fn gpu_chunks(&self) -> Vec<Duration> {
        let mut out = self.denoise_chunks();
        if !self.mask_bake.is_zero() {
            out.push(self.mask_bake);
        }
        out
    }

    /// The chunk-duration list this job's bake actually executes, in order: one decode chunk, N
    /// denoise chunks (the last one truncated to whatever remains, never rounded up past the real
    /// total), one mask-bake chunk.
    fn chunks(&self) -> Vec<Duration> {
        let mut out = Vec::new();
        if !self.decode.is_zero() {
            out.push(self.decode);
        }
        out.extend(self.denoise_chunks());
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
    /// `denoise_chunk` sets the bound). A zero `denoise_chunk` means denoise itself runs as one
    /// unchunked unit (see `chunks()`'s own handling of this), so its atomic size is the whole
    /// `denoise_total` in that case, not zero.
    pub fn worst_case_atomic_unit(&self) -> Duration {
        self.decode.max(self.worst_case_gpu_atomic_unit())
    }

    /// The two-lane model's bound (#206): decode runs on the CPU lane, so only the GPU lane's
    /// atomic units (an unchunked denoise if `denoise_chunk` is zero, the denoise tile, the mask
    /// bake) can delay a foreground request. A zero `denoise_chunk` means denoise itself runs as
    /// one unchunked unit, so its atomic size is the whole `denoise_total` in that case.
    pub fn worst_case_gpu_atomic_unit(&self) -> Duration {
        let denoise_atomic = if self.denoise_chunk.is_zero() {
            self.denoise_total
        } else {
            self.denoise_chunk
        };
        denoise_atomic.max(self.mask_bake)
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
///
/// # Panics
///
/// Panics if `foreground_cost >= foreground_interval` (and `foreground_interval` is nonzero):
/// servicing one request would then take at least as long as the gap until the next one becomes
/// due, so the due-time backlog (`next_foreground_due <= clock`) never clears and the loop below
/// would otherwise never terminate -- the same class of bug as `ChunkedBakeCost::chunks()`'s own
/// zero-`denoise_chunk` case, caught by CodeRabbit review rather than by running it.
pub fn simulate_hero_bake_chunked(
    n_images: usize,
    cursor_start: usize,
    walk_pace: Duration,
    cost: ChunkedBakeCost,
    foreground_interval: Duration,
    foreground_cost: Duration,
) -> ChunkedSimResult {
    assert!(
        foreground_interval.is_zero() || foreground_cost < foreground_interval,
        "foreground_cost ({foreground_cost:?}) must be strictly less than foreground_interval \
         ({foreground_interval:?}), or the due-time backlog never clears"
    );

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

    finish(
        bake_finish,
        cursor_start,
        walk_pace,
        clock,
        foreground_latencies,
    )
}

fn finish(
    bake_finish: Vec<Duration>,
    cursor_start: usize,
    walk_pace: Duration,
    total_wall_time: Duration,
    foreground_latencies: Vec<Duration>,
) -> ChunkedSimResult {
    let n_images = bake_finish.len();
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

/// The two-lane variant of `simulate_hero_bake_chunked` (#206): models what #55's real
/// `nicti-pounce` runtime actually does -- decode (`Lane::Cpu`, `nicti-pelt`'s `decode_job.rs`)
/// runs concurrently with the GPU lane's denoise tiles and mask bake (`Lane::Gpu`), instead of
/// serializing all three stage types on one worker timeline.
///
/// - **CPU lane**: `cpu_decode_threads` parallel decode slots. Whenever a slot frees up it decodes
///   the pending image nearest the cursor *at that moment* (`prefetch::priority_order`). Decoded
///   frames are never evicted and the lane may run arbitrarily far ahead of the GPU lane -- the
///   RAM a real run-ahead would cost is not modeled, so this is the best case for the GPU lane.
/// - **GPU lane**: one worker, picks the nearest-to-cursor image, starts it once that image's
///   decode is ready (idle in between), then runs its denoise tiles and mask bake as chunks.
/// - **Foreground**: only the GPU lane serves it (live render is GPU work), so it waits for at
///   most the chunk in flight -- bounded by `ChunkedBakeCost::worst_case_gpu_atomic_unit`, not by
///   decode. A request due while the GPU lane is idle (waiting on a decode) is served on time.
///
/// # Panics
///
/// Panics under the same foreground interval/cost condition as `simulate_hero_bake_chunked`, and
/// if `cpu_decode_threads` is zero (no decode would ever finish).
pub fn simulate_hero_bake_two_lane(
    n_images: usize,
    cursor_start: usize,
    walk_pace: Duration,
    cost: ChunkedBakeCost,
    cpu_decode_threads: usize,
    foreground_interval: Duration,
    foreground_cost: Duration,
) -> ChunkedSimResult {
    assert!(
        foreground_interval.is_zero() || foreground_cost < foreground_interval,
        "foreground_cost ({foreground_cost:?}) must be strictly less than foreground_interval \
         ({foreground_interval:?}), or the due-time backlog never clears"
    );
    assert!(
        cpu_decode_threads > 0,
        "cpu_decode_threads must be at least 1, or no decode ever finishes"
    );

    let gpu_chunks = cost.gpu_chunks();
    let mut pending: BTreeSet<usize> = (0..n_images).collect();
    let mut undecoded: BTreeSet<usize> = (0..n_images).collect();
    let mut decode_ready: Vec<Option<Duration>> = vec![None; n_images];
    let mut cpu_slot_free = vec![Duration::ZERO; cpu_decode_threads];
    let mut bake_finish = vec![Duration::ZERO; n_images];
    let mut clock = Duration::ZERO;
    let mut next_foreground_due = foreground_interval;
    let mut foreground_latencies = Vec::new();

    while !pending.is_empty() {
        let cursor = cursor_at(cursor_start, n_images, walk_pace, clock);
        let next = prefetch::priority_order(&pending, cursor)[0];

        // Advance the CPU lane until `next` has a decode-ready time. Each iteration decodes one
        // image on whichever slot frees up first, in priority order for the cursor at that time.
        while decode_ready[next].is_none() {
            let (slot, &start) = cpu_slot_free
                .iter()
                .enumerate()
                .min_by_key(|&(_, t)| *t)
                .expect("cpu_decode_threads > 0");
            let at_cursor = cursor_at(cursor_start, n_images, walk_pace, start);
            let pick = prefetch::priority_order(&undecoded, at_cursor)[0];
            let ready = start + cost.decode;
            cpu_slot_free[slot] = ready;
            decode_ready[pick] = Some(ready);
            undecoded.remove(&pick);
        }
        let ready = decode_ready[next].expect("decoded above");

        // GPU lane idles until the decode lands. A foreground request due in that window is
        // served on time (zero latency); serving it can push the start past `ready`.
        if ready > clock {
            if !foreground_interval.is_zero() {
                while next_foreground_due <= ready {
                    foreground_latencies.push(Duration::ZERO);
                    clock = next_foreground_due + foreground_cost;
                    next_foreground_due += foreground_interval;
                }
            }
            clock = clock.max(ready);
        }

        for &chunk in &gpu_chunks {
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

    finish(
        bake_finish,
        cursor_start,
        walk_pace,
        clock,
        foreground_latencies,
    )
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
    fn chunks_treats_a_zero_denoise_chunk_as_one_unchunked_unit_instead_of_looping_forever() {
        // Regression test for a real adversarial-review finding: `remaining.min(ZERO)` is always
        // ZERO, so the naive loop never shrinks `remaining` and spins forever -- directly
        // reachable from `bin/crouch.rs`'s `sim --denoise-chunk-ms 0`.
        let cost = ChunkedBakeCost {
            decode: Duration::ZERO,
            denoise_total: Duration::from_millis(100),
            denoise_chunk: Duration::ZERO,
            mask_bake: Duration::ZERO,
        };
        assert_eq!(cost.chunks(), vec![Duration::from_millis(100)]);
        assert_eq!(cost.worst_case_atomic_unit(), Duration::from_millis(100));
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
    #[should_panic(expected = "must be strictly less than")]
    fn rejects_foreground_cost_equal_to_interval_instead_of_hanging_forever() {
        // Regression test for a real CodeRabbit finding: if foreground_cost >= foreground_interval,
        // the due-time backlog (`next_foreground_due <= clock`) never clears, so the inner while
        // loop in `simulate_hero_bake_chunked` never terminates and `foreground_latencies` grows
        // without bound -- directly reachable from `crouch sim --foreground-interval-ms 5
        // --foreground-cost-ms 5`. Caught by inspection, not by running it (it doesn't
        // self-terminate) -- same class of bug as `chunks_treats_a_zero_denoise_chunk_...` above.
        simulate_hero_bake_chunked(
            5,
            0,
            Duration::from_millis(1),
            tiny_cost(),
            Duration::from_millis(5),
            Duration::from_millis(5),
        );
    }

    #[test]
    #[should_panic(expected = "must be strictly less than")]
    fn rejects_foreground_cost_greater_than_interval() {
        simulate_hero_bake_chunked(
            5,
            0,
            Duration::from_millis(1),
            tiny_cost(),
            Duration::from_millis(5),
            Duration::from_millis(10),
        );
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

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn gpu_chunks_are_the_denoise_tiles_plus_mask_bake_without_decode() {
        assert_eq!(
            tiny_cost().gpu_chunks(),
            vec![ms(30), ms(30), ms(30), ms(10)]
        );
    }

    #[test]
    fn worst_case_gpu_atomic_unit_excludes_decode() {
        let cost = ChunkedBakeCost {
            decode: ms(1700),
            denoise_total: ms(50_900),
            denoise_chunk: ms(45),
            mask_bake: ms(1000),
        };
        // #206: with decode on its own CPU lane, the unchunked mask bake (1000ms) -- not decode
        // (1700ms) -- is what bounds foreground latency.
        assert_eq!(cost.worst_case_gpu_atomic_unit(), ms(1000));
        assert_eq!(cost.worst_case_atomic_unit(), ms(1700));
    }

    #[test]
    fn two_lane_hides_decode_behind_gpu_work() {
        // decode (10) < GPU work per image (100), one CPU slot: only the first decode is exposed.
        let cost = tiny_cost();
        let result = simulate_hero_bake_two_lane(
            5,
            0,
            Duration::from_secs(10),
            cost,
            1,
            Duration::ZERO,
            Duration::ZERO,
        );
        assert_eq!(result.first_image_ready, ms(10 + 100));
        assert_eq!(result.total_wall_time, ms(10 + 5 * 100));
        assert!(result.foreground_latencies.is_empty());
    }

    #[test]
    fn two_lane_starves_the_gpu_when_decode_is_the_bottleneck() {
        let cost = ChunkedBakeCost {
            decode: ms(200),
            ..tiny_cost()
        };
        let one_slot = simulate_hero_bake_two_lane(
            3,
            0,
            Duration::from_secs(10),
            cost,
            1,
            Duration::ZERO,
            Duration::ZERO,
        );
        // Decodes land at 200/400/600; each image then takes 100ms of GPU: 3*200 + 100.
        assert_eq!(one_slot.total_wall_time, ms(700));
        let two_slots = simulate_hero_bake_two_lane(
            3,
            0,
            Duration::from_secs(10),
            cost,
            2,
            Duration::ZERO,
            Duration::ZERO,
        );
        // Decodes land at 200/200/400; GPU never waits after the first image.
        assert_eq!(two_slots.total_wall_time, ms(500));
    }

    #[test]
    fn two_lane_is_never_slower_than_one_lane() {
        let cost = tiny_cost();
        let one = simulate_hero_bake_chunked(
            8,
            0,
            Duration::from_secs(10),
            cost,
            Duration::ZERO,
            Duration::ZERO,
        );
        let two = simulate_hero_bake_two_lane(
            8,
            0,
            Duration::from_secs(10),
            cost,
            1,
            Duration::ZERO,
            Duration::ZERO,
        );
        assert!(two.total_wall_time <= one.total_wall_time);
    }

    #[test]
    fn two_lane_foreground_latency_is_bounded_by_the_gpu_atomic_unit_not_decode() {
        // Decode (500ms) dwarfs every GPU chunk; under the one-lane model it would set the bound.
        let cost = ChunkedBakeCost {
            decode: ms(500),
            ..tiny_cost()
        };
        let bound = cost.worst_case_gpu_atomic_unit();
        assert_eq!(bound, ms(30));
        let result = simulate_hero_bake_two_lane(
            6,
            0,
            Duration::from_secs(10),
            cost,
            1,
            ms(7), // deliberately not a multiple of any chunk size
            ms(2),
        );
        assert!(!result.foreground_latencies.is_empty());
        for latency in &result.foreground_latencies {
            assert!(
                *latency <= bound,
                "latency {latency:?} exceeded the GPU atomic unit {bound:?}"
            );
        }
        let one_lane =
            simulate_hero_bake_chunked(6, 0, Duration::from_secs(10), cost, ms(7), ms(2));
        assert!(one_lane.foreground_worst_latency() > bound);
    }

    #[test]
    fn two_lane_serves_foreground_on_time_while_the_gpu_waits_for_decode() {
        // The only GPU wait is the first decode (0..500ms); requests due then see an idle GPU.
        let cost = ChunkedBakeCost {
            decode: ms(500),
            ..tiny_cost()
        };
        let result =
            simulate_hero_bake_two_lane(1, 0, Duration::from_secs(10), cost, 1, ms(100), ms(5));
        // Dues at 100/200/300/400/500 fall in the idle window: all zero latency. Only the one due
        // exactly when the decode lands (500) overlaps the GPU start, delaying it by its cost.
        assert!(result.foreground_latencies.len() >= 5);
        assert!(result.foreground_latencies[..5].iter().all(|l| l.is_zero()));
        assert!(result.first_image_ready >= ms(500 + 5 + 100));
    }

    #[test]
    #[should_panic(expected = "cpu_decode_threads must be at least 1")]
    fn two_lane_rejects_zero_decode_threads() {
        simulate_hero_bake_two_lane(3, 0, ms(1), tiny_cost(), 0, Duration::ZERO, Duration::ZERO);
    }

    #[test]
    #[should_panic(expected = "must be strictly less than")]
    fn two_lane_rejects_foreground_cost_equal_to_interval() {
        simulate_hero_bake_two_lane(3, 0, ms(1), tiny_cost(), 1, ms(5), ms(5));
    }
}
