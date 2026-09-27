//! Optionally collapses litter-style burst sets into one representative embedding (their mean)
//! before clustering, so a 10-frame burst of the same pose doesn't dominate a DBSCAN neighborhood
//! and skew `eps`/silhouette selection relative to subjects with only a couple of photos. This
//! module doesn't depend on `spikes/litter` (spikes can't depend on other spikes) -- a caller
//! supplies burst-group ids from wherever it got them (e.g. re-running litter's own grouping, or a
//! future production `#36` integration), as a plain `&[usize]` parallel to the embeddings.

use crate::embed::cosine_similarity;

/// One entry per input embedding: `representative` is the index of the collapsed group's mean
/// this embedding was folded into (all members of the same burst share one `representative`
/// index, into `collapsed` below).
pub struct Collapsed {
    /// One mean embedding per distinct `group_id`, in first-seen order.
    pub collapsed: Vec<Vec<f32>>,
    /// `representative[i]` indexes into `collapsed` for input embedding `i`.
    pub representative: Vec<usize>,
}

/// `embeddings.len() == group_ids.len()`; asserted, since a mismatch would silently misattribute
/// which burst an embedding belongs to.
pub fn collapse_by_group(embeddings: &[Vec<f32>], group_ids: &[usize]) -> Collapsed {
    assert_eq!(embeddings.len(), group_ids.len());
    if embeddings.is_empty() {
        return Collapsed {
            collapsed: Vec::new(),
            representative: Vec::new(),
        };
    }
    let dim = embeddings[0].len();

    let mut group_order: Vec<usize> = Vec::new();
    let mut group_index: std::collections::HashMap<usize, usize> = Default::default();
    for &gid in group_ids {
        group_index.entry(gid).or_insert_with(|| {
            let idx = group_order.len();
            group_order.push(gid);
            idx
        });
    }

    let mut sums = vec![vec![0.0f64; dim]; group_order.len()];
    let mut counts = vec![0usize; group_order.len()];
    let mut representative = Vec::with_capacity(embeddings.len());

    for (emb, &gid) in embeddings.iter().zip(group_ids) {
        let idx = group_index[&gid];
        representative.push(idx);
        counts[idx] += 1;
        for (s, &v) in sums[idx].iter_mut().zip(emb) {
            *s += v as f64;
        }
    }

    let collapsed = sums
        .into_iter()
        .zip(&counts)
        .map(|(sum, &count)| sum.into_iter().map(|s| (s / count as f64) as f32).collect())
        .collect();

    Collapsed {
        collapsed,
        representative,
    }
}

/// Expands per-collapsed-group cluster assignments back onto every original input index, via
/// `representative` from `collapse_by_group` -- so a caller can score/display results per original
/// photo even though clustering ran on the smaller collapsed set.
pub fn expand_assignments<T: Clone>(
    collapsed_assignments: &[T],
    representative: &[usize],
) -> Vec<T> {
    representative
        .iter()
        .map(|&idx| collapsed_assignments[idx].clone())
        .collect()
}

/// Sanity helper for the ablation CLI: mean pairwise cosine similarity within a collapsed group,
/// used to spot-check that a burst's members really were similar enough to be worth collapsing
/// (not exposed as a hard gate -- the caller decides what to do with a low value).
pub fn within_group_mean_similarity(
    embeddings: &[Vec<f32>],
    group_ids: &[usize],
    target_group: usize,
) -> Option<f64> {
    let members: Vec<&Vec<f32>> = embeddings
        .iter()
        .zip(group_ids)
        .filter(|(_, &g)| g == target_group)
        .map(|(e, _)| e)
        .collect();
    if members.len() < 2 {
        return None;
    }
    let mut sum = 0.0;
    let mut count = 0usize;
    for i in 0..members.len() {
        for j in (i + 1)..members.len() {
            sum += cosine_similarity(members[i], members[j]);
            count += 1;
        }
    }
    Some(sum / count as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapse_by_group_averages_same_group_embeddings() {
        let embeddings = vec![vec![0.0, 0.0], vec![2.0, 0.0], vec![10.0, 10.0]];
        let group_ids = vec![0, 0, 1];
        let result = collapse_by_group(&embeddings, &group_ids);
        assert_eq!(result.collapsed.len(), 2);
        assert_eq!(result.collapsed[0], vec![1.0, 0.0]);
        assert_eq!(result.collapsed[1], vec![10.0, 10.0]);
        assert_eq!(result.representative, vec![0, 0, 1]);
    }

    #[test]
    fn collapse_by_group_handles_empty_input() {
        let result = collapse_by_group(&[], &[]);
        assert!(result.collapsed.is_empty());
        assert!(result.representative.is_empty());
    }

    #[test]
    fn collapse_by_group_singleton_groups_are_unchanged() {
        let embeddings = vec![vec![1.0, 2.0], vec![3.0, 4.0]];
        let group_ids = vec![0, 1];
        let result = collapse_by_group(&embeddings, &group_ids);
        assert_eq!(result.collapsed, embeddings);
    }

    #[test]
    fn expand_assignments_maps_back_to_original_indices() {
        let collapsed_assignments = vec!["subject-a", "subject-b"];
        let representative = vec![0, 0, 1, 1, 1];
        let expanded = expand_assignments(&collapsed_assignments, &representative);
        assert_eq!(
            expanded,
            vec![
                "subject-a",
                "subject-a",
                "subject-b",
                "subject-b",
                "subject-b"
            ]
        );
    }

    #[test]
    fn within_group_mean_similarity_is_none_for_singleton() {
        let embeddings = vec![vec![1.0, 0.0]];
        let group_ids = vec![0];
        assert!(within_group_mean_similarity(&embeddings, &group_ids, 0).is_none());
    }

    #[test]
    fn within_group_mean_similarity_computes_pairwise_mean() {
        let embeddings = vec![vec![1.0, 0.0], vec![0.0, 1.0], vec![-1.0, 0.0]];
        let group_ids = vec![0, 0, 0];
        let sim = within_group_mean_similarity(&embeddings, &group_ids, 0).unwrap();
        // pairs: (1,0)-(0,1)=0.0, (1,0)-(-1,0)=-1.0, (0,1)-(-1,0)=0.0 -> mean = -1/3
        assert!((sim - (-1.0 / 3.0)).abs() < 1e-9);
    }
}
