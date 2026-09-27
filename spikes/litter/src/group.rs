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
    group_id
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
        // frame 0 and frame 2 are similar to each other and within budget via max_lookahead=2.
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
            vec![0, 1, 0],
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
