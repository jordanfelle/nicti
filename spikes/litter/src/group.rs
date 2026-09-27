//! Sequence-constrained two-level (tight/set) segmentation over frames in capture order.
//!
//! Con-day duplicates are pose sets a few seconds to ~30s apart, not sub-second bursts (measured:
//! Anthrocon 2026-07-03's 1,368 frames have most consecutive gaps at 2-10s, only 152 <=1s) -- a
//! pure timestamp threshold misses most real duplicates, so time acts as a *constraint* here
//! (never link across a gap bigger than the level's own budget) while a similarity signal makes
//! the actual link/no-link decision.
//!
//! **Nesting invariant**: every set group is a union of whole tight groups, never a partial one --
//! guaranteed structurally (`group_sets` operates on tight-group boundaries, never splits a tight
//! group), not just asserted.

/// One level's linking parameters: link frame `i` to a following frame within `max_gap_secs` (and
/// within `max_lookahead` positions, to tolerate a single interleaved/odd frame -- e.g. a second
/// shooter's frame, or a test shot) when `similarity(i, j) >= min_similarity`.
#[derive(Debug, Clone, Copy)]
pub struct LevelParams {
    pub max_gap_secs: f64,
    pub min_similarity: f64,
    pub max_lookahead: usize,
}

/// Assigns each frame index a group id (0-based, monotonically non-decreasing in capture order).
/// `gap_secs(i, j)` and `similarity(i, j)` are only ever called with `j > i`.
pub fn group_tight(
    n: usize,
    gap_secs: impl Fn(usize, usize) -> f64,
    similarity: impl Fn(usize, usize) -> f64,
    params: LevelParams,
) -> Vec<usize> {
    if n == 0 {
        return Vec::new();
    }
    let mut group_id = vec![0usize; n];
    let mut next_id = 0usize;
    group_id[0] = 0;

    for i in 1..n {
        let lookback = params.max_lookahead.min(i);
        let mut linked = false;
        for back in 1..=lookback {
            let j = i - back;
            if gap_secs(j, i) > params.max_gap_secs {
                // Frames only get further apart in time as `back` grows (capture order is
                // monotonic), so once one predecessor is out of budget, every earlier one is too.
                break;
            }
            if similarity(j, i) >= params.min_similarity {
                // `back > 1` means one or more intervening frames (already assigned their own
                // group id, since they didn't link backward within budget) sit between `j` and
                // `i` -- retroactively fold them into `j`'s group too, so this group stays a
                // contiguous run of frames rather than sandwiching an orphaned single-frame group
                // between two occurrences of the same id (a real bug an adversarial review
                // caught: a non-contiguous group corrupted `group_sets`'s span-based boundary
                // comparison, since it violated this function's own "monotonically
                // non-decreasing" doc claim).
                for k in (j + 1)..i {
                    group_id[k] = group_id[j];
                }
                group_id[i] = group_id[j];
                linked = true;
                break;
            }
        }
        if !linked {
            next_id += 1;
            group_id[i] = next_id;
        }
    }
    compact_ids(&group_id)
}

/// Remaps raw ids to a dense `0..k` range, in first-appearance order.
///
/// The fold above (line `for k in (j + 1)..i`) keeps every materialized id's frames contiguous,
/// but it does so by overwriting positions that already held a *different*, previously-assigned
/// raw id -- that raw id was allocated (via `next_id += 1`) but never appears in the output again,
/// leaving a gap in the numbering (e.g. raw ids `[0, 0, 0, 2, 3]`, where `1` was allocated then
/// folded away). `group_sets` assumes ids are dense and increasing (`debug_assert_eq!(gid,
/// spans.len())`), so a gap panics in debug builds and reads out of bounds in release -- a real
/// bug an adversarial review caught, reachable under the shipped default `max_lookahead: 2` any
/// time a lookahead fold is followed by a later, unrelated new group. First-appearance order is
/// capture order here (each id's first frame is always its earliest frame), so this preserves the
/// "monotonically non-decreasing" contract as well as the contiguity the fold already guarantees.
fn compact_ids(ids: &[usize]) -> Vec<usize> {
    let Some(&max_id) = ids.iter().max() else {
        return Vec::new();
    };
    let mut remap = vec![None; max_id + 1];
    let mut next = 0usize;
    ids.iter()
        .map(|&id| {
            *remap[id].get_or_insert_with(|| {
                let assigned = next;
                next += 1;
                assigned
            })
        })
        .collect()
}

/// Merges whole tight groups into set groups, using each tight group's first and last frame as
/// its boundary representatives. Never splits a tight group -- the nesting invariant this module
/// exists to guarantee.
pub fn group_sets(
    tight_group_id: &[usize],
    gap_secs: impl Fn(usize, usize) -> f64,
    similarity: impl Fn(usize, usize) -> f64,
    params: LevelParams,
) -> Vec<usize> {
    let n = tight_group_id.len();
    if n == 0 {
        return Vec::new();
    }

    // One (first_idx, last_idx) span per tight group, in the order tight groups first appear
    // (which is capture order, since `group_tight`'s ids are assigned in capture order).
    let mut spans: Vec<(usize, usize)> = Vec::new();
    for (i, &gid) in tight_group_id.iter().enumerate() {
        match spans.get_mut(gid) {
            Some((_, last)) => *last = i,
            None => {
                debug_assert_eq!(
                    gid,
                    spans.len(),
                    "tight group ids must be assigned in order"
                );
                spans.push((i, i));
            }
        }
    }

    let mut set_of_tight = vec![0usize; spans.len()];
    let mut next_set = 0usize;
    set_of_tight[0] = 0;

    for g in 1..spans.len() {
        let lookback = params.max_lookahead.min(g);
        let mut linked = false;
        for back in 1..=lookback {
            let prev = g - back;
            let (_, prev_last) = spans[prev];
            let (this_first, _) = spans[g];
            if gap_secs(prev_last, this_first) > params.max_gap_secs {
                break;
            }
            if similarity(prev_last, this_first) >= params.min_similarity {
                set_of_tight[g] = set_of_tight[prev];
                linked = true;
                break;
            }
        }
        if !linked {
            next_set += 1;
            set_of_tight[g] = next_set;
        }
    }

    tight_group_id
        .iter()
        .map(|&gid| set_of_tight[gid])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(times: &[f64], i: usize, j: usize) -> f64 {
        (times[j] - times[i]).abs()
    }

    #[test]
    fn links_consecutive_similar_frames_within_gap() {
        let times = [0.0, 1.0, 2.0, 30.0, 31.0];
        let sim = |_i: usize, _j: usize| 1.0; // always similar
        let params = LevelParams {
            max_gap_secs: 5.0,
            min_similarity: 0.5,
            max_lookahead: 1,
        };
        let groups = group_tight(times.len(), |i, j| secs(&times, i, j), sim, params);
        assert_eq!(groups, vec![0, 0, 0, 1, 1]);
    }

    #[test]
    fn dissimilar_frames_never_link_even_within_gap() {
        let times = [0.0, 1.0, 2.0];
        let sim = |_i: usize, _j: usize| 0.0; // never similar
        let params = LevelParams {
            max_gap_secs: 5.0,
            min_similarity: 0.5,
            max_lookahead: 1,
        };
        let groups = group_tight(times.len(), |i, j| secs(&times, i, j), sim, params);
        assert_eq!(groups, vec![0, 1, 2]);
    }

    #[test]
    fn tolerates_one_interleaved_frame_via_lookahead() {
        // Frame 1 is dissimilar to both neighbors (a second shooter's interleaved frame), but
        // frame 0 and frame 2 are similar to each other and within budget via max_lookahead=2 --
        // the interleaved frame folds into the same group as its neighbors (not left as its own
        // one-frame group sandwiched between two occurrences of the same id, which would make
        // group ids non-contiguous -- a real bug an adversarial review caught, since a
        // non-contiguous group corrupted `group_sets`'s span-based boundary comparison).
        let times = [0.0, 0.5, 1.0];
        let sim = |i: usize, j: usize| if i == 1 || j == 1 { 0.0 } else { 1.0 };
        let params = LevelParams {
            max_gap_secs: 5.0,
            min_similarity: 0.5,
            max_lookahead: 2,
        };
        let groups = group_tight(times.len(), |i, j| secs(&times, i, j), sim, params);
        assert_eq!(
            groups,
            vec![0, 0, 0],
            "frame 2 should link back to frame 0, skipping 1"
        );
    }

    #[test]
    fn set_groups_are_always_a_union_of_tight_groups() {
        // Tight: [0,0,1,1,2]. Set-level similarity always true, gap always in budget -- every
        // tight group should merge into one set, but a tight group must never be split.
        let tight = vec![0usize, 0, 1, 1, 2];
        let sim = |_i: usize, _j: usize| 1.0;
        let gap = |_i: usize, _j: usize| 0.0;
        let params = LevelParams {
            max_gap_secs: 100.0,
            min_similarity: 0.5,
            max_lookahead: 1,
        };
        let sets = group_sets(&tight, gap, sim, params);
        assert_eq!(sets, vec![0, 0, 0, 0, 0]);

        // Now verify the invariant holds even when set-level linking is picky: every frame in
        // the same tight group must always land in the same set group, for any parameters.
        let sim_picky = |i: usize, j: usize| if (i, j) == (1, 2) { 0.0 } else { 1.0 };
        let sets2 = group_sets(&tight, gap, sim_picky, params);
        for tg in 0..=2 {
            let members: Vec<usize> = tight
                .iter()
                .enumerate()
                .filter(|(_, &t)| t == tg)
                .map(|(i, _)| i)
                .collect();
            let first_set = sets2[members[0]];
            for &m in &members {
                assert_eq!(sets2[m], first_set, "tight group {tg} split across sets");
            }
        }
    }

    #[test]
    fn lookahead_fold_followed_by_a_new_group_leaves_no_gap_in_ids() {
        // Reproduces the exact scenario an adversarial review found under production defaults
        // (max_lookahead: 2): frame 2 skip-links back to frame 0 over frame 1 (the "tolerate one
        // interleaved frame" case), folding frame 1's already-allocated id away. Frames 3 and 4
        // then start new, unrelated groups. Before the compaction fix, this produced raw ids
        // [0, 0, 0, 2, 3] -- id 1 allocated then folded away and never appearing again -- which
        // panicked `group_sets`'s dense-id assumption. Output ids must be contiguous 0..k with no
        // gaps, and `group_sets` must run on the result without panicking.
        let sim = |i: usize, j: usize| match (i, j) {
            (0, 2) => 1.0, // frame 2 skip-links back to frame 0
            _ => 0.0,      // every other adjacent pair is dissimilar (each starts its own group)
        };
        let gap = |_i: usize, _j: usize| 0.0; // always within budget
        let params = LevelParams {
            max_gap_secs: 100.0,
            min_similarity: 0.5,
            max_lookahead: 2,
        };
        let groups = group_tight(5, gap, sim, params);
        assert_eq!(
            groups,
            vec![0, 0, 0, 1, 2],
            "ids must be dense, no skipped values"
        );

        // Must not panic (debug_assert_eq! in group_sets would fire on a non-dense id).
        let set_sim = |_i: usize, _j: usize| 0.0;
        let sets = group_sets(&groups, gap, set_sim, params);
        assert_eq!(sets.len(), 5);
    }

    #[test]
    fn empty_input_returns_empty() {
        assert_eq!(
            group_tight(
                0,
                |_, _| 0.0,
                |_, _| 0.0,
                LevelParams {
                    max_gap_secs: 1.0,
                    min_similarity: 0.5,
                    max_lookahead: 1,
                }
            ),
            Vec::<usize>::new()
        );
    }
}
