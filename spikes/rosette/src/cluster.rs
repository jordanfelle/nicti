//! Subject clustering over a set of embeddings. `spikes/litter/src/group.rs`'s two-level grouping
//! is deliberately *not* reused here: it's sequence-constrained (only ever links a frame to a
//! nearby one in capture order), which is exactly wrong for subject grouping -- the same subject
//! can reappear at any point across a whole shoot, hours apart, with no timestamp constraint. This
//! module implements DBSCAN on cosine distance instead, following the Fursee paper's own pipeline
//! shape (per the issue's own comment): DBSCAN with a silhouette-coefficient-guided `eps` sweep,
//! so no fixed cluster count needs to be chosen up front.
//!
//! **HDBSCAN, mentioned in the plan, isn't implemented this pass.** No pure-Rust HDBSCAN crate
//! passed a quick license check (`hdbscan`/`petal-clustering`'s exact license strings weren't
//! independently re-verified against `deny.toml` this pass), and implementing a from-scratch
//! HDBSCAN (mutual reachability + minimum spanning tree + condensed cluster tree) is a
//! meaningfully larger undertaking than DBSCAN -- deferred, recorded under "Not adopted" in
//! ADR-0035, not silently dropped.

/// One point's group assignment: `Some(id)` for a real cluster, `None` for DBSCAN noise (an
/// outlier not close enough to any cluster) -- kept distinct from a real singleton cluster, since
/// a caller (the CLI, `metrics::score`) may want to treat noise specially (e.g. show it as
/// "unclustered" rather than its own group in the contact sheet).
pub type Assignment = Option<usize>;

/// `min_samples`: DBSCAN's density threshold -- a point needs at least this many neighbors
/// (including itself) within `eps` to seed a cluster. Fixed per the plan's decision rule (a swept
/// `eps`, a fixed `min_samples`), not part of the sweep itself.
pub struct DbscanParams {
    pub eps: f64,
    pub min_samples: usize,
}

/// Runs DBSCAN over `n` points using `dist(i, j)` (expected symmetric, `dist(i, i) == 0.0` -- not
/// enforced, since a caller-provided distance function is trusted the same way `spikes/litter`'s
/// `group.rs` trusts its own `sim`/`gap` closures). Returns one `Assignment` per point, indices
/// matching the input order.
pub fn dbscan(
    n: usize,
    dist: impl Fn(usize, usize) -> f64,
    params: &DbscanParams,
) -> Vec<Assignment> {
    const UNVISITED: i64 = -1;
    const NOISE: i64 = -2;

    let mut labels = vec![UNVISITED; n];
    let mut next_cluster = 0i64;

    let neighbors =
        |i: usize| -> Vec<usize> { (0..n).filter(|&j| dist(i, j) <= params.eps).collect() };

    for i in 0..n {
        if labels[i] != UNVISITED {
            continue;
        }
        let neighbors_i = neighbors(i);
        if neighbors_i.len() < params.min_samples {
            labels[i] = NOISE;
            continue;
        }

        let cluster_id = next_cluster;
        next_cluster += 1;
        labels[i] = cluster_id;

        let mut seeds: Vec<usize> = neighbors_i.into_iter().filter(|&j| j != i).collect();
        let mut seen_in_seeds: std::collections::HashSet<usize> = seeds.iter().copied().collect();
        let mut idx = 0;
        while idx < seeds.len() {
            let j = seeds[idx];
            idx += 1;
            if labels[j] == NOISE {
                labels[j] = cluster_id;
            }
            if labels[j] != UNVISITED {
                continue;
            }
            labels[j] = cluster_id;
            let neighbors_j = neighbors(j);
            if neighbors_j.len() >= params.min_samples {
                for k in neighbors_j {
                    if seen_in_seeds.insert(k) {
                        seeds.push(k);
                    }
                }
            }
        }
    }

    labels
        .into_iter()
        .map(|l| {
            if l == NOISE || l == UNVISITED {
                None
            } else {
                Some(l as usize)
            }
        })
        .collect()
}

/// The mean silhouette coefficient over every non-noise point, in `[-1.0, 1.0]` (higher =
/// better-separated clusters) -- standard definition (Rousseeuw 1987): for point `i`,
/// `(b - a) / max(a, b)`, where `a` is the mean distance to i's own cluster (excluding itself) and
/// `b` is the mean distance to the nearest *other* cluster. Points whose own cluster has only one
/// member (`a` undefined) score `0.0`, the conventional edge-case value. `None` if fewer than two
/// non-noise clusters exist (silhouette is undefined with 0 or 1 clusters).
pub fn mean_silhouette(
    n: usize,
    dist: impl Fn(usize, usize) -> f64,
    assignments: &[Assignment],
) -> Option<f64> {
    let cluster_ids: std::collections::BTreeSet<usize> =
        assignments.iter().filter_map(|a| *a).collect();
    if cluster_ids.len() < 2 {
        return None;
    }

    let mut total = 0.0;
    let mut counted = 0usize;
    for i in 0..n {
        let Some(ci) = assignments[i] else { continue };
        let mut own_sum = 0.0;
        let mut own_count = 0usize;
        let mut other_means: std::collections::HashMap<usize, (f64, usize)> = Default::default();

        for (j, cj_opt) in assignments.iter().enumerate().take(n) {
            if i == j {
                continue;
            }
            let Some(cj) = *cj_opt else { continue };
            let d = dist(i, j);
            if cj == ci {
                own_sum += d;
                own_count += 1;
            } else {
                let entry = other_means.entry(cj).or_insert((0.0, 0));
                entry.0 += d;
                entry.1 += 1;
            }
        }

        let s = if own_count == 0 {
            0.0
        } else {
            let a = own_sum / own_count as f64;
            let b = other_means
                .values()
                .map(|&(sum, count)| sum / count as f64)
                .fold(f64::INFINITY, f64::min);
            if b.is_infinite() {
                0.0
            } else if a == 0.0 && b == 0.0 {
                // `(b - a) / a.max(b)` would otherwise be `0.0 / 0.0 = NaN` here -- flagged by an
                // adversarial review as a latent gap in this `pub` function's public API (not
                // reachable through this crate's own `dbscan_with_eps_sweep`, since two
                // DBSCAN-produced clusters can't have an exact 0.0 average inter-cluster distance
                // without density-reachability having already merged them, but a caller feeding
                // externally-derived assignments -- e.g. a future #36 integration -- could hit
                // this). A NaN score corrupts the running mean via `total += s` and can "stick" as
                // the eps sweep's `best` result forever, since `NaN > x` is always `false`. Every
                // point in this point's own cluster and every other cluster it's compared against
                // is equidistant at zero -- conventionally neither well- nor poorly-separated.
                0.0
            } else {
                (b - a) / a.max(b)
            }
        };
        total += s;
        counted += 1;
    }

    if counted == 0 {
        None
    } else {
        Some(total / counted as f64)
    }
}

/// Sweeps `eps` over `candidates` (ascending cosine-distance thresholds), keeping `min_samples`
/// fixed, and returns the assignment + `eps` that maximizes mean silhouette -- the "silhouette-
/// coefficient-guided adaptive hyperparameter selection" the Fursee paper describes, so no fixed
/// cluster count needs to be chosen up front. Falls back to the *last* (loosest) candidate's
/// result if every sweep point yields fewer than 2 clusters (silhouette undefined throughout),
/// rather than panicking or returning an arbitrary early candidate.
pub fn dbscan_with_eps_sweep(
    n: usize,
    dist: impl Fn(usize, usize) -> f64 + Copy,
    min_samples: usize,
    eps_candidates: &[f64],
) -> (Vec<Assignment>, f64) {
    assert!(
        !eps_candidates.is_empty(),
        "eps_candidates must be non-empty"
    );

    let mut best: Option<(Vec<Assignment>, f64, f64)> = None; // (assignment, eps, silhouette)
    for &eps in eps_candidates {
        let params = DbscanParams { eps, min_samples };
        let assignment = dbscan(n, dist, &params);
        if let Some(score) = mean_silhouette(n, dist, &assignment) {
            let is_better = best.as_ref().map(|(_, _, s)| score > *s).unwrap_or(true);
            if is_better {
                best = Some((assignment, eps, score));
            }
        }
    }

    match best {
        Some((assignment, eps, _)) => (assignment, eps),
        None => {
            let eps = *eps_candidates.last().unwrap();
            let assignment = dbscan(n, dist, &DbscanParams { eps, min_samples });
            (assignment, eps)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three tight synthetic "blobs" in 1D distance space (points 0-2, 3-5, 6-8, each blob's own
    /// members at distance ~0.05 from each other, ~1.0 from every other blob's members) -- DBSCAN
    /// should recover exactly three clusters at a reasonable eps.
    fn blob_dist(i: usize, j: usize) -> f64 {
        let blob = |k: usize| k / 3;
        if i == j {
            0.0
        } else if blob(i) == blob(j) {
            0.05
        } else {
            1.0
        }
    }

    #[test]
    fn dbscan_recovers_three_synthetic_blobs() {
        let assignments = dbscan(
            9,
            blob_dist,
            &DbscanParams {
                eps: 0.5,
                min_samples: 2,
            },
        );
        let blob = |k: usize| k / 3;
        for i in 0..9 {
            for j in 0..9 {
                if blob(i) == blob(j) {
                    assert_eq!(
                        assignments[i], assignments[j],
                        "points {i} and {j} are the same blob, must share a cluster"
                    );
                } else {
                    assert_ne!(
                        assignments[i], assignments[j],
                        "points {i} and {j} are different blobs, must not share a cluster"
                    );
                }
            }
        }
    }

    #[test]
    fn dbscan_marks_isolated_point_as_noise() {
        // Point 9 is far from every blob and every other point -- min_samples=2 means it can
        // never seed its own cluster alone.
        let dist = |i: usize, j: usize| -> f64 {
            if i == 9 || j == 9 {
                if i == j {
                    0.0
                } else {
                    10.0
                }
            } else {
                blob_dist(i, j)
            }
        };
        let assignments = dbscan(
            10,
            dist,
            &DbscanParams {
                eps: 0.5,
                min_samples: 2,
            },
        );
        assert_eq!(assignments[9], None, "an isolated point must be noise");
    }

    #[test]
    fn mean_silhouette_zero_distance_everywhere_is_zero_not_nan() {
        // Regression test for an adversarial-review-caught bug: two clusters whose members are
        // all mutually zero-distance (own-cluster mean `a` and nearest-other-cluster mean `b` both
        // exactly 0.0) used to compute `0.0 / 0.0 = NaN`, which then corrupts the running mean and
        // can "stick" as a sweep's chosen result forever (`NaN > x` is always false).
        let assignments: Vec<Assignment> = vec![Some(0), Some(0), Some(1), Some(1)];
        let zero_dist = |_i: usize, _j: usize| -> f64 { 0.0 };
        let score = mean_silhouette(4, zero_dist, &assignments).expect("2 clusters present");
        assert!(!score.is_nan(), "score must not be NaN: {score}");
        assert_eq!(score, 0.0);
    }

    #[test]
    fn dbscan_eps_too_small_yields_all_noise() {
        // eps smaller than even within-blob distance -- nothing can seed a cluster.
        let assignments = dbscan(
            9,
            blob_dist,
            &DbscanParams {
                eps: 0.01,
                min_samples: 2,
            },
        );
        assert!(assignments.iter().all(|a| a.is_none()));
    }

    #[test]
    fn dbscan_eps_too_large_merges_everything() {
        // eps larger than the cross-blob distance -- everything becomes one cluster.
        let assignments = dbscan(
            9,
            blob_dist,
            &DbscanParams {
                eps: 2.0,
                min_samples: 2,
            },
        );
        let first = assignments[0];
        assert!(first.is_some());
        assert!(assignments.iter().all(|a| *a == first));
    }

    #[test]
    fn silhouette_prefers_well_separated_clusters_over_merged_ones() {
        let well_separated = dbscan(
            9,
            blob_dist,
            &DbscanParams {
                eps: 0.5,
                min_samples: 2,
            },
        );
        let merged = dbscan(
            9,
            blob_dist,
            &DbscanParams {
                eps: 2.0,
                min_samples: 2,
            },
        );
        let s_separated = mean_silhouette(9, blob_dist, &well_separated).expect("3 clusters");
        // `merged` collapses to one cluster -- silhouette is undefined (None), confirming the
        // sweep would never pick it over a real multi-cluster result.
        assert!(mean_silhouette(9, blob_dist, &merged).is_none());
        assert!(
            s_separated > 0.5,
            "well-separated blobs should score highly: {s_separated}"
        );
    }

    #[test]
    fn eps_sweep_picks_the_well_separated_operating_point() {
        let candidates = [0.01, 0.2, 0.5, 1.0, 2.0, 5.0];
        let (assignment, chosen_eps) = dbscan_with_eps_sweep(9, blob_dist, 2, &candidates);
        // Both 0.2 and 0.5 cleanly separate the three blobs here (within-blob distance is 0.05,
        // cross-blob is 1.0), so they tie on silhouette -- the sweep keeps the first (smallest)
        // tied candidate. The real invariant under test is "picked an eps that cleanly separates
        // the blobs", not a specific tied value.
        assert!(
            [0.2, 0.5].contains(&chosen_eps),
            "should land on an eps that separates the 3 blobs cleanly, got {chosen_eps}"
        );
        let blob = |k: usize| k / 3;
        for i in 0..9 {
            for j in (i + 1)..9 {
                assert_eq!(blob(i) == blob(j), assignment[i] == assignment[j]);
            }
        }
    }

    #[test]
    fn eps_sweep_falls_back_to_loosest_candidate_when_silhouette_is_never_defined() {
        // Every candidate either yields all-noise or one giant cluster -- silhouette is undefined
        // throughout, so the sweep must fall back to the last (loosest) candidate rather than
        // panicking.
        let candidates = [0.0001, 100.0];
        let (assignment, chosen_eps) = dbscan_with_eps_sweep(9, blob_dist, 2, &candidates);
        assert_eq!(chosen_eps, 100.0);
        assert!(assignment.iter().all(|a| a.is_some()));
    }
}
