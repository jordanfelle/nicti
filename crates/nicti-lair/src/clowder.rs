//! #23's collections: a `manual` collection is an ordered, hand-picked asset set; a `smart`
//! collection's membership is never stored -- it's a saved [`crate::hunt::Filter`], resolved by
//! calling [`crate::CatalogStore::hunt`] with it. Both kinds share one tree (a collection can
//! nest under another, manual or smart, purely for organization) -- a clowder is a group of cats.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollectionKind {
    Manual,
    Smart,
}

impl CollectionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CollectionKind::Manual => "manual",
            CollectionKind::Smart => "smart",
        }
    }
}

impl std::str::FromStr for CollectionKind {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "manual" => Ok(CollectionKind::Manual),
            "smart" => Ok(CollectionKind::Smart),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Collection {
    pub id: i64,
    pub parent_id: Option<i64>,
    pub kind: CollectionKind,
    pub name: String,
}
