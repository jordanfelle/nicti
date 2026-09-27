//! Two train/holdout splits, per ADR-0053: **event** (primary) and **temporal** (secondary). An
//! image-level split would leak synced batch edits across train and holdout -- LRC users commonly
//! select a whole folder and apply one develop setting to all of them, so two images from the same
//! folder can be near-duplicate label pairs. Splitting by folder keeps a whole event on one side.
//! Generic over the row type (`catalog::KeeperRow` and `dataset::FeatureRow` both use this) via
//! accessor closures, rather than each row type re-implementing the same folder-hash logic.

/// Deterministically assigns `folder_id` to train (true) or holdout (false) at roughly the given
/// `holdout_fraction`, via a fixed-seed integer mix -- no RNG dependency, so the same folder always
/// lands on the same side across repeated runs. Not cryptographic; only needs to decorrelate
/// sequential folder ids well enough that the split fraction holds up in aggregate.
pub fn folder_in_train(folder_id: i64, holdout_fraction: f64) -> bool {
    let mut x = folder_id as u64;
    // SplitMix64's mixing step -- a standard, well-decorrelating integer hash, not just `folder_id
    // % N` (which would clump adjacent, often chronologically-adjacent, folder ids together).
    x = x.wrapping_add(0x9E3779B97F4A7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D049BB133111EB);
    x ^= x >> 31;
    let frac = (x as f64) / (u64::MAX as f64);
    frac >= holdout_fraction
}

/// Splits `rows` by folder id at `holdout_fraction` (e.g. `0.2` for an 80/20 split). Every row from
/// the same folder always lands on the same side.
pub fn by_folder<T: Clone>(
    rows: &[T],
    folder_id: impl Fn(&T) -> i64,
    holdout_fraction: f64,
) -> (Vec<T>, Vec<T>) {
    rows.iter()
        .cloned()
        .partition(|r| folder_in_train(folder_id(r), holdout_fraction))
}

/// Splits `rows` by capture time: the oldest `1.0 - holdout_fraction` become train, the newest
/// `holdout_fraction` become holdout -- the realistic personal-model deployment case (train on
/// history so far, test on what comes next). Rows with no capture time sort first (oldest),
/// matching `Option`'s natural `None < Some` ordering.
pub fn temporal<T: Clone>(
    rows: &[T],
    capture_time: impl Fn(&T) -> Option<String>,
    holdout_fraction: f64,
) -> (Vec<T>, Vec<T>) {
    let mut sorted: Vec<T> = rows.to_vec();
    sorted.sort_by_key(|a| capture_time(a));
    let holdout_count = ((sorted.len() as f64) * holdout_fraction).round() as usize;
    let split_at = sorted.len().saturating_sub(holdout_count);
    let holdout = sorted.split_off(split_at);
    (sorted, holdout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::KeeperRow;
    use crate::sliders::Sliders;

    fn row(folder_id: i64, image_id: i64, capture_time: Option<&str>) -> KeeperRow {
        KeeperRow {
            image_id,
            folder_id,
            capture_time: capture_time.map(String::from),
            lrc_path: format!("C:/x/{image_id}.NEF"),
            sliders: Sliders::default(),
            iso: None,
            shutter_speed: None,
            aperture: None,
        }
    }

    #[test]
    fn by_folder_keeps_every_row_of_one_folder_on_the_same_side() {
        let rows: Vec<_> = (0..50)
            .flat_map(|f| (0..5).map(move |i| row(f, f * 10 + i, None)))
            .collect();
        let (train, holdout) = by_folder(&rows, |r| r.folder_id, 0.2);
        for folder in 0..50 {
            let in_train = train.iter().any(|r| r.folder_id == folder);
            let in_holdout = holdout.iter().any(|r| r.folder_id == folder);
            assert!(
                in_train ^ in_holdout,
                "folder {folder} split across both sides"
            );
        }
        assert_eq!(train.len() + holdout.len(), rows.len());
    }

    #[test]
    fn by_folder_is_deterministic() {
        let rows: Vec<_> = (0..20).map(|i| row(i, i, None)).collect();
        let (t1, h1) = by_folder(&rows, |r| r.folder_id, 0.3);
        let (t2, h2) = by_folder(&rows, |r| r.folder_id, 0.3);
        assert_eq!(
            t1.iter().map(|r| r.folder_id).collect::<Vec<_>>(),
            t2.iter().map(|r| r.folder_id).collect::<Vec<_>>()
        );
        assert_eq!(
            h1.iter().map(|r| r.folder_id).collect::<Vec<_>>(),
            h2.iter().map(|r| r.folder_id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn by_folder_lands_roughly_at_the_requested_fraction() {
        let rows: Vec<_> = (0..1000).map(|i| row(i, i, None)).collect();
        let (_train, holdout) = by_folder(&rows, |r| r.folder_id, 0.2);
        let frac = holdout.len() as f64 / rows.len() as f64;
        assert!(
            (frac - 0.2).abs() < 0.05,
            "holdout fraction {frac} too far from 0.2"
        );
    }

    #[test]
    fn temporal_puts_the_newest_rows_in_holdout() {
        let rows = vec![
            row(1, 1, Some("2026-01-01")),
            row(1, 2, Some("2026-06-01")),
            row(1, 3, Some("2026-09-01")),
            row(1, 4, Some("2026-09-15")),
            row(1, 5, Some("2026-09-20")),
        ];
        let (train, holdout) = temporal(&rows, |r| r.capture_time.clone(), 0.4);
        assert_eq!(train.len(), 3);
        assert_eq!(holdout.len(), 2);
        assert!(train.iter().all(|r| r.image_id <= 3));
        assert!(holdout.iter().all(|r| r.image_id >= 4));
    }
}
