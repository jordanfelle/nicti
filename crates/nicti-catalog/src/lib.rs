//! Catalog store extension point (ADR-0004 §7/§8). `CatalogStore` settles identity and
//! versioning only, via `Module` — the schema, embedded database engine (#67:
//! pglite-rs/rusqlite/DuckDB/LMDB), and import/ingest pipeline are owned by #22.

use nicti_claw::{Module, Registry};

/// A catalog store backend. No query/write methods are defined here yet — that signature
/// belongs to #22, once #67's database-engine choice lands.
pub trait CatalogStore: Module {}

/// Registry of catalog store modules, keyed by namespaced id.
pub type StoreRegistry = Registry<dyn CatalogStore>;

#[cfg(test)]
mod tests {
    use super::*;
    use nicti_claw::Descriptor;
    use serde_json::Value;
    use std::sync::Arc;

    struct Dummy;

    impl Module for Dummy {
        fn id(&self) -> &str {
            "nicti.catalog.dummy"
        }

        fn schema_version(&self) -> u32 {
            1
        }

        fn migrate_params(&self, _from_version: u32, params: Value) -> Option<Value> {
            Some(params)
        }
    }

    impl CatalogStore for Dummy {}

    fn make_dummy() -> Arc<dyn CatalogStore> {
        Arc::new(Dummy)
    }

    #[test]
    fn dummy_registers_and_resolves_as_trait_object() {
        let mut registry: StoreRegistry = Registry::new();
        registry
            .register(
                Descriptor {
                    id: "nicti.catalog.dummy",
                    schema_version: 1,
                },
                make_dummy,
            )
            .expect("registration should succeed");

        let resolved = registry
            .get("nicti.catalog.dummy")
            .expect("dummy catalog store is registered");
        assert_eq!(resolved.id(), "nicti.catalog.dummy");
    }
}
