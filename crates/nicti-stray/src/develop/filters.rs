//! `FilterList`: LRC's AI filters (ADR-0061 Q4 / #157). Real usage is ~99.7% Denoise, but the
//! importer must read each `Filters[].Title` rather than assume: People/Reflection Removal belong to
//! #51 and Super Resolution to #174. None has a nicti stage with params yet (`nicti.denoise` takes
//! none), so this only *counts* them for the report and leaves the verbatim text in provenance.

use super::{field, field_str, items, Tx};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FilterCounts {
    pub denoise: u64,
    /// People Removal / Reflection Removal / Distraction Removal -- #51.
    pub removal: u64,
    /// Super Resolution -- #174.
    pub super_resolution: u64,
    pub other: u64,
}

impl FilterCounts {
    pub fn add(&mut self, other: &FilterCounts) {
        self.denoise += other.denoise;
        self.removal += other.removal;
        self.super_resolution += other.super_resolution;
        self.other += other.other;
    }
}

pub(super) fn apply(tx: &mut Tx) {
    tx.take("AllowFilters");
    // Read through the root directly: `take` borrows `tx` mutably for the value's lifetime.
    let mut counts = FilterCounts::default();
    if let Some(list) = field(tx.root, "FilterList") {
        let entries = field(list, "Filters").map(items).unwrap_or_default();
        for entry in entries {
            let title = field_str(entry, "Title").unwrap_or("").to_ascii_lowercase();
            if title.contains("denoise") {
                counts.denoise += 1;
            } else if title.contains("removal") {
                counts.removal += 1;
            } else if title.contains("super resolution") {
                counts.super_resolution += 1;
            } else {
                counts.other += 1;
            }
        }
    }
    tx.take("FilterList");
    tx.filters = counts;
}

#[cfg(test)]
mod tests {
    use crate::develop::{translate, Context};

    #[test]
    fn each_filter_title_routes_to_its_own_counter() {
        let t = translate(
            r#"s = { AllowFilters = true, FilterList = { Filters = {
                 { Title = "Denoise", Enabled = true },
                 { Title = "People Removal" },
                 { Title = "Super Resolution" },
                 { Title = "Reflection Removal" },
                 { Title = "Something New" } } } }"#,
            &Context::default(),
        )
        .unwrap();
        assert_eq!(t.filters.denoise, 1);
        assert_eq!(t.filters.removal, 2);
        assert_eq!(t.filters.super_resolution, 1);
        assert_eq!(t.filters.other, 1);
        assert!(t.untranslated.is_empty());
    }

    #[test]
    fn no_filter_list_counts_nothing() {
        let t = translate("s = { Exposure2012 = 1 }", &Context::default()).unwrap();
        assert_eq!(t.filters, Default::default());
    }
}
