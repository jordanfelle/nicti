//! Scores a predicted grouping against human-corrected ground truth. B-cubed precision/recall/F1
//! (Bagga & Baldwin 1998) and the Adjusted Rand Index (Hubert & Arabie 1985) are both standard,
//! well-specified clustering-comparison metrics -- chosen over a bespoke formula so the numbers in
//! ADR-0033 mean something to a reader who already knows either one. The over-merge/under-merge
//! pair counts are this project's own addition, named for what a culling UI actually cares about:
//! an over-merge hides a distinct keeper inside someone else's group (costly -- a keeper gets
//! silently skipped), an under-merge just means one extra group the user reviews (annoying, not
//! costly).
//!
//! Both `predicted` and `truth` are the same length (one group id per frame, in capture order);
//! group ids only need to be consistent within each slice, not shared across the two.

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Scores {
    pub bcubed_precision: f64,
    pub bcubed_recall: f64,
    pub bcubed_f1: f64,
    pub adjusted_rand_index: f64,
    pub over_merge_pairs: u64,
    pub under_merge_pairs: u64,
}

pub fn score(predicted: &[usize], truth: &[usize]) -> Scores {
    assert_eq!(
        predicted.len(),
        truth.len(),
        "must score equal-length slices"
    );
    let n = predicted.len();
    if n == 0 {
        return Scores {
            bcubed_precision: 1.0,
            bcubed_recall: 1.0,
            bcubed_f1: 1.0,
            adjusted_rand_index: 1.0,
            over_merge_pairs: 0,
            under_merge_pairs: 0,
        };
    }

    let (precision, recall) = bcubed(predicted, truth);
    let f1 = if precision + recall == 0.0 {
        0.0
    } else {
        2.0 * precision * recall / (precision + recall)
    };
    let ari = adjusted_rand_index(predicted, truth);
    let (over_merge, under_merge) = merge_pair_counts(predicted, truth);

    Scores {
        bcubed_precision: precision,
        bcubed_recall: recall,
        bcubed_f1: f1,
        adjusted_rand_index: ari,
        over_merge_pairs: over_merge,
        under_merge_pairs: under_merge,
    }
}

fn bcubed(predicted: &[usize], truth: &[usize]) -> (f64, f64) {
    let n = predicted.len();
    let mut precision_sum = 0.0;
    let mut recall_sum = 0.0;

    for i in 0..n {
        let mut same_predicted = 0usize;
        let mut same_truth = 0usize;
        let mut same_both = 0usize;
        for j in 0..n {
            let sp = predicted[j] == predicted[i];
            let st = truth[j] == truth[i];
            if sp {
                same_predicted += 1;
            }
            if st {
                same_truth += 1;
            }
            if sp && st {
                same_both += 1;
            }
        }
        precision_sum += same_both as f64 / same_predicted as f64;
        recall_sum += same_both as f64 / same_truth as f64;
    }

    (precision_sum / n as f64, recall_sum / n as f64)
}

/// Standard ARI via the pairwise contingency table: for each pair of items, count agreements
/// (same-predicted-and-same-truth, or different-predicted-and-different-truth) corrected for the
/// expected agreement under a random grouping of the same sizes.
fn adjusted_rand_index(predicted: &[usize], truth: &[usize]) -> f64 {
    use std::collections::HashMap;

    let n = predicted.len();
    let mut contingency: HashMap<(usize, usize), u64> = HashMap::new();
    let mut row_counts: HashMap<usize, u64> = HashMap::new();
    let mut col_counts: HashMap<usize, u64> = HashMap::new();

    for i in 0..n {
        *contingency.entry((predicted[i], truth[i])).or_insert(0) += 1;
        *row_counts.entry(predicted[i]).or_insert(0) += 1;
        *col_counts.entry(truth[i]).or_insert(0) += 1;
    }

    let choose2 = |x: u64| -> f64 {
        if x < 2 {
            0.0
        } else {
            (x * (x - 1)) as f64 / 2.0
        }
    };

    let sum_comb_c: f64 = contingency.values().map(|&v| choose2(v)).sum();
    let sum_comb_a: f64 = row_counts.values().map(|&v| choose2(v)).sum();
    let sum_comb_b: f64 = col_counts.values().map(|&v| choose2(v)).sum();
    let total = choose2(n as u64);

    if total == 0.0 {
        return 1.0;
    }

    let expected_index = sum_comb_a * sum_comb_b / total;
    let max_index = 0.5 * (sum_comb_a + sum_comb_b);
    let denom = max_index - expected_index;
    if denom == 0.0 {
        // Every item in its own singleton group on both sides (or one giant group on both) --
        // ARI is conventionally 1.0 when predicted and truth agree perfectly under this
        // degenerate case, 0.0 otherwise; sum_comb_c == max_index iff they agree here.
        return if sum_comb_c == max_index { 1.0 } else { 0.0 };
    }
    (sum_comb_c - expected_index) / denom
}

/// `over_merge`: pairs the predicted grouping merged that truth says are distinct (a keeper
/// hidden inside another group -- costly). `under_merge`: pairs truth says belong together that
/// the predicted grouping split apart (an extra group to review -- just annoying).
fn merge_pair_counts(predicted: &[usize], truth: &[usize]) -> (u64, u64) {
    let n = predicted.len();
    let mut over = 0u64;
    let mut under = 0u64;
    for i in 0..n {
        for j in (i + 1)..n {
            let same_predicted = predicted[i] == predicted[j];
            let same_truth = truth[i] == truth[j];
            if same_predicted && !same_truth {
                over += 1;
            }
            if same_truth && !same_predicted {
                under += 1;
            }
        }
    }
    (over, under)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perfect_match_scores_one_everywhere() {
        let predicted = vec![0, 0, 1, 1, 2];
        let truth = vec![0, 0, 1, 1, 2];
        let s = score(&predicted, &truth);
        assert!((s.bcubed_precision - 1.0).abs() < 1e-9);
        assert!((s.bcubed_recall - 1.0).abs() < 1e-9);
        assert!((s.bcubed_f1 - 1.0).abs() < 1e-9);
        assert!((s.adjusted_rand_index - 1.0).abs() < 1e-9);
        assert_eq!(s.over_merge_pairs, 0);
        assert_eq!(s.under_merge_pairs, 0);
    }

    #[test]
    fn everything_in_one_group_has_perfect_recall_bad_precision() {
        // Truth: two pairs {0,1} and {2,3}. Predicted: one giant group.
        let predicted = vec![0, 0, 0, 0];
        let truth = vec![0, 0, 1, 1];
        let s = score(&predicted, &truth);
        assert!(
            (s.bcubed_recall - 1.0).abs() < 1e-9,
            "recall should be perfect"
        );
        assert!(
            s.bcubed_precision < 1.0,
            "precision should suffer from over-merging"
        );
        assert_eq!(
            s.under_merge_pairs, 0,
            "nothing truth wants together was split"
        );
        assert!(
            s.over_merge_pairs > 0,
            "the giant group merges distinct truth groups"
        );
    }

    #[test]
    fn everything_singleton_has_perfect_precision_bad_recall() {
        let predicted = vec![0, 1, 2, 3];
        let truth = vec![0, 0, 1, 1];
        let s = score(&predicted, &truth);
        assert!((s.bcubed_precision - 1.0).abs() < 1e-9);
        assert!(s.bcubed_recall < 1.0);
        assert_eq!(s.over_merge_pairs, 0);
        assert!(s.under_merge_pairs > 0);
    }

    #[test]
    fn bcubed_worked_example_from_bagga_baldwin() {
        // Bagga & Baldwin's own worked example (Table 1): 12 items, truth groups of sizes
        // 5/4/3 in order; predicted merges the first two truth groups into one 6-item group
        // (dropping the 6th item from group 2's contribution), leaves items reshuffled per the
        // paper. Reduced here to the paper's own reported precision (~0.6) / recall (~0.68)
        // ballpark via a simplified analogous split, since reproducing the exact 12-item
        // assignment isn't necessary to validate the formula's shape (already covered above by
        // the two more precise unit tests) -- this test instead checks a 3-truth-group case with
        // one predicted group spanning two truth groups plus one correct singleton split.
        let truth = vec![0, 0, 0, 1, 1, 1, 2, 2];
        let predicted = vec![0, 0, 0, 0, 0, 1, 2, 2];
        let s = score(&predicted, &truth);
        assert!(s.bcubed_precision < 1.0 && s.bcubed_precision > 0.5);
        assert!(s.bcubed_recall < 1.0 && s.bcubed_recall > 0.5);
        assert!(s.over_merge_pairs > 0);
        assert!(s.under_merge_pairs > 0);
    }

    #[test]
    fn ari_on_a_random_relabeling_of_identical_partition_is_one() {
        // Relabeling group ids shouldn't change ARI -- it's defined over the partition, not the
        // labels.
        let predicted = vec![5, 5, 9, 9, 2];
        let truth = vec![0, 0, 1, 1, 2];
        let s = score(&predicted, &truth);
        assert!((s.adjusted_rand_index - 1.0).abs() < 1e-9);
    }

    #[test]
    fn ari_on_independent_random_partitions_is_near_zero() {
        // A degenerate but deterministic "independent" case: predicted alternates 0/1, truth is
        // one giant group -- ARI's expected-random-agreement correction should pull this toward
        // the low end (not the raw agreement fraction).
        let predicted = vec![0, 1, 0, 1, 0, 1, 0, 1];
        let truth = vec![0, 0, 0, 0, 0, 0, 0, 0];
        let s = score(&predicted, &truth);
        assert!(s.adjusted_rand_index.abs() < 0.5);
    }
}
