//! Re-consent (ADR-0214 section 2): an update that gains authority disables the module until the
//! user accepts it again. "Gains authority" is evaluated **field by field**, not just by
//! capability kind: a new host in an existing `network` capability, a new filesystem scope, a new
//! extension point (an implicit hook) or a looser budget is exactly as much a widening as a wholly
//! new capability. Removing or narrowing anything, or rewording a justification, never is.

use crate::manifest::{Budget, Capability, ExtensionPoint, FsScope, Manifest, NetworkScope};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Widening {
    /// A network capability for a scope the old manifest did not have.
    NetworkScopeAdded(NetworkScope),
    /// A host added to a network capability the old manifest already had.
    NetworkHostAdded {
        scope: NetworkScope,
        host: String,
    },
    FilesystemScopeAdded(FsScope),
    GpuAdded,
    ExtensionPointAdded(ExtensionPoint),
    /// A resource budget raised (or lifted to the host default from a tighter value).
    BudgetLoosened {
        field: &'static str,
        from: u64,
        to: u64,
    },
}

fn network_hosts(m: &Manifest, scope: NetworkScope) -> Option<&[crate::manifest::Host]> {
    m.capabilities.iter().find_map(|c| match c {
        Capability::Network {
            scope: s,
            allowed_hosts,
            ..
        } if *s == scope => Some(allowed_hosts.as_slice()),
        _ => None,
    })
}

fn has_fs(m: &Manifest, scope: FsScope) -> bool {
    m.capabilities
        .iter()
        .any(|c| matches!(c, Capability::Filesystem { scope: s, .. } if *s == scope))
}

fn has_gpu(m: &Manifest) -> bool {
    m.capabilities
        .iter()
        .any(|c| matches!(c, Capability::Gpu { .. }))
}

fn loosened(field: &'static str, old: u64, new: u64, out: &mut Vec<Widening>) {
    if new > old {
        out.push(Widening::BudgetLoosened {
            field,
            from: old,
            to: new,
        });
    }
}

fn budget_widenings(old: &Budget, new: &Budget, out: &mut Vec<Widening>) {
    loosened(
        "fuel_limit",
        old.effective_fuel(),
        new.effective_fuel(),
        out,
    );
    loosened(
        "epoch_budget_ms",
        old.effective_epoch_budget_ms(),
        new.effective_epoch_budget_ms(),
        out,
    );
    loosened(
        "memory_mb_max",
        u64::from(old.effective_memory_mb()),
        u64::from(new.effective_memory_mb()),
        out,
    );
}

/// Everything `new` may do that `old` could not. Empty means the update needs no re-consent.
pub fn widenings(old: &Manifest, new: &Manifest) -> Vec<Widening> {
    let mut out = Vec::new();
    for point in &new.extension_points {
        if !old.extension_points.contains(point) {
            out.push(Widening::ExtensionPointAdded(*point));
        }
    }
    for cap in &new.capabilities {
        match cap {
            Capability::Network {
                scope,
                allowed_hosts,
                ..
            } => match network_hosts(old, *scope) {
                None => out.push(Widening::NetworkScopeAdded(*scope)),
                Some(old_hosts) => {
                    for host in allowed_hosts {
                        if !old_hosts.contains(host) {
                            out.push(Widening::NetworkHostAdded {
                                scope: *scope,
                                host: host.as_str().to_owned(),
                            });
                        }
                    }
                }
            },
            Capability::Filesystem { scope, .. } => {
                if !has_fs(old, *scope) {
                    out.push(Widening::FilesystemScopeAdded(*scope));
                }
            }
            Capability::Gpu { .. } => {
                if !has_gpu(old) {
                    out.push(Widening::GpuAdded);
                }
            }
        }
    }
    budget_widenings(&old.budget, &new.budget, &mut out);
    out
}

pub fn needs_reconsent(old: &Manifest, new: &Manifest) -> bool {
    !widenings(old, new).is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{parse, tests::GOOD};

    fn m(text: &str) -> Manifest {
        parse(text).unwrap()
    }

    fn variant(replace: &str, by: &str) -> Manifest {
        assert!(GOOD.contains(replace), "{replace}");
        m(&GOOD.replace(replace, by))
    }

    #[test]
    fn an_identical_manifest_or_a_pure_version_bump_needs_nothing() {
        let old = m(GOOD);
        assert!(widenings(&old, &old).is_empty());
        let bumped = variant("version = \"1.2.0\"", "version = \"1.3.0\"");
        assert!(!needs_reconsent(&old, &bumped));
    }

    #[test]
    fn rewording_a_justification_is_not_a_widening() {
        let old = m(GOOD);
        let new = variant(
            "Downloads the segmentation weights",
            "Fetches the segmentation weights",
        );
        assert!(!needs_reconsent(&old, &new));
    }

    #[test]
    fn a_new_host_in_an_existing_network_capability_is_a_widening() {
        let old = m(GOOD);
        let new = variant(
            "[\"cdn.vendor.example.com\"]",
            "[\"cdn.vendor.example.com\", \"mirror.example.org\"]",
        );
        assert_eq!(
            widenings(&old, &new),
            vec![Widening::NetworkHostAdded {
                scope: NetworkScope::WeightDownload,
                host: "mirror.example.org".into()
            }]
        );
        // Swapping a host for another one is also gaining a host.
        let swapped = variant("cdn.vendor.example.com", "other.example.org");
        assert!(needs_reconsent(&old, &swapped));
    }

    #[test]
    fn removing_a_host_or_a_capability_is_never_a_widening() {
        let wide = variant(
            "[\"cdn.vendor.example.com\"]",
            "[\"cdn.vendor.example.com\", \"mirror.example.org\"]",
        );
        let narrow = m(GOOD);
        assert!(!needs_reconsent(&wide, &narrow));
        let no_caps = m(
            "id = \"vendor.module_name\"\nversion = \"1.2.0\"\nhost_api_version = 1\nextension_points = [\"ModelProvider\"]\n",
        );
        assert!(!needs_reconsent(&narrow, &no_caps));
    }

    #[test]
    fn a_new_scope_kind_or_hook_is_a_widening() {
        let old = m(GOOD);
        let cloud = m(&format!(
            "{GOOD}\n[[capabilities]]\nkind = \"network\"\nscope = \"cloud-feature\"\nallowed_hosts = [\"api.example.com\"]\njustification = \"Opt-in cloud feature.\"\n"
        ));
        assert_eq!(
            widenings(&old, &cloud),
            vec![Widening::NetworkScopeAdded(NetworkScope::CloudFeature)]
        );
        let catalog_fs = variant("scope = \"user-selected\"", "scope = \"catalog-managed\"");
        assert_eq!(
            widenings(&old, &catalog_fs),
            vec![Widening::FilesystemScopeAdded(FsScope::CatalogManaged)]
        );
        let gpu = m(&format!(
            "{GOOD}\n[[capabilities]]\nkind = \"gpu\"\njustification = \"Runs on the GPU.\"\n"
        ));
        assert_eq!(widenings(&old, &gpu), vec![Widening::GpuAdded]);
        let hook = variant("[\"ModelProvider\"]", "[\"ModelProvider\", \"Exporter\"]");
        assert_eq!(
            widenings(&old, &hook),
            vec![Widening::ExtensionPointAdded(ExtensionPoint::Exporter)]
        );
    }

    #[test]
    fn loosening_a_budget_is_a_widening_and_tightening_is_not() {
        let tight = m(&format!(
            "{GOOD}\n[budget]\nfuel_limit = 1000\nmemory_mb_max = 64\n"
        ));
        let looser = m(&format!(
            "{GOOD}\n[budget]\nfuel_limit = 5000\nmemory_mb_max = 64\n"
        ));
        assert_eq!(
            widenings(&tight, &looser),
            vec![Widening::BudgetLoosened {
                field: "fuel_limit",
                from: 1000,
                to: 5000
            }]
        );
        assert!(!needs_reconsent(&looser, &tight));
        // Dropping an override entirely lifts the module to the host default: that is loosening.
        let lifted = m(GOOD);
        let w = widenings(&tight, &lifted);
        assert!(w.iter().any(|x| matches!(
            x,
            Widening::BudgetLoosened {
                field: "fuel_limit",
                ..
            }
        )));
        assert!(w.iter().any(|x| matches!(
            x,
            Widening::BudgetLoosened {
                field: "memory_mb_max",
                ..
            }
        )));
    }

    #[test]
    fn several_widenings_are_all_reported() {
        let old = m(GOOD);
        let new = m(&format!(
            "{}\n[[capabilities]]\nkind = \"gpu\"\njustification = \"Runs on the GPU.\"\n",
            GOOD.replace(
                "[\"cdn.vendor.example.com\"]",
                "[\"cdn.vendor.example.com\", \"b.example.org\"]"
            )
            .replace("[\"ModelProvider\"]", "[\"ModelProvider\", \"Exporter\"]")
        ));
        assert_eq!(widenings(&old, &new).len(), 3);
    }
}
