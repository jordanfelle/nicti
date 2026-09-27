//! Deterministic sampling from `catalog::keepers`'s full row set down to a manageable working set:
//! caps each folder (event) so one large shoot can't dominate the training distribution, then
//! stops once the overall target count is reached. Deterministic on `catalog::keepers`'s own
//! `ORDER BY i.id_local` -- no RNG, so a re-run against the same catalog reproduces the same
//! sample exactly.

use std::collections::HashMap;

use crate::catalog::KeeperRow;

/// Caps each folder at `per_folder_cap` rows and the overall sample at `target_total`, taking rows
/// in the order they arrive (already `id_local`-ordered, i.e. import order within a folder).
pub fn select(rows: &[KeeperRow], per_folder_cap: usize, target_total: usize) -> Vec<KeeperRow> {
    let mut per_folder_count: HashMap<i64, usize> = HashMap::new();
    let mut selected = Vec::new();
    for row in rows {
        if selected.len() >= target_total {
            break;
        }
        let count = per_folder_count.entry(row.folder_id).or_insert(0);
        if *count >= per_folder_cap {
            continue;
        }
        *count += 1;
        selected.push(row.clone());
    }
    selected
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sliders::Sliders;

    fn row(folder_id: i64, image_id: i64) -> KeeperRow {
        KeeperRow {
            image_id,
            folder_id,
            capture_time: None,
            lrc_path: format!("C:/x/{image_id}.NEF"),
            sliders: Sliders {
                exposure2012: 1.0,
                ..Default::default()
            },
            iso: None,
            shutter_speed: None,
            aperture: None,
        }
    }

    #[test]
    fn caps_each_folder_independently() {
        let rows: Vec<_> = (0..10)
            .map(|i| row(1, i))
            .chain((0..10).map(|i| row(2, 100 + i)))
            .collect();
        let selected = select(&rows, 3, 100);
        let folder1 = selected.iter().filter(|r| r.folder_id == 1).count();
        let folder2 = selected.iter().filter(|r| r.folder_id == 2).count();
        assert_eq!(folder1, 3);
        assert_eq!(folder2, 3);
    }

    #[test]
    fn stops_at_the_overall_target() {
        let rows: Vec<_> = (0..10).map(|i| row(1, i)).collect();
        let selected = select(&rows, 100, 4);
        assert_eq!(selected.len(), 4);
    }

    #[test]
    fn is_deterministic_across_repeated_calls() {
        let rows: Vec<_> = (0..20).map(|i| row(i % 3, i)).collect();
        let a = select(&rows, 5, 10);
        let b = select(&rows, 5, 10);
        let a_ids: Vec<_> = a.iter().map(|r| r.image_id).collect();
        let b_ids: Vec<_> = b.iter().map(|r| r.image_id).collect();
        assert_eq!(a_ids, b_ids);
    }
}
