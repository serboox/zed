use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::environment::Variable;

pub type CollectionId = Uuid;

/// The top-level grouping in the API client tree. Folders and requests are
/// always scoped to exactly one collection (see `Folder::collection_id` and
/// `Request::collection_id`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Collection {
    pub id: CollectionId,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub variables: Vec<Variable>,
    /// Position among sibling top-level collections, lowest first. Defaults
    /// to 0 for documents written before this field existed, matching
    /// `Folder::order`/`Request::order`'s own backward-compatible default.
    #[serde(default)]
    pub order: i64,
}

/// How a tree of collections is ordered on screen.
///
/// Remembered between sessions next to the collections themselves, the way the
/// dragged order is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TreeOrder {
    /// By name, from the start. The order a reader expects of a list of names.
    #[default]
    Name,
    /// By name, from the end.
    NameReversed,
    /// Wherever things were dragged to.
    Manual,
}

impl TreeOrder {
    pub fn label(self) -> &'static str {
        match self {
            TreeOrder::Name => "Name (A-Z)",
            TreeOrder::NameReversed => "Name (Z-A)",
            TreeOrder::Manual => "Manually",
        }
    }
}

impl Collection {
    pub fn new(name: String) -> Self {
        Self {
            id: Uuid::new_v4(),
            name,
            description: None,
            variables: Vec::new(),
            order: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_collection_has_no_description_or_variables() {
        let collection = Collection::new("Payments API".to_string());
        assert_eq!(collection.name, "Payments API");
        assert!(collection.description.is_none());
        assert!(collection.variables.is_empty());
    }

    #[test]
    fn round_trips_through_serde_with_defaults_for_missing_optional_fields() {
        let json = r#"{"id":"00000000-0000-0000-0000-000000000001","name":"Legacy"}"#;
        let collection: Collection = serde_json::from_str(json).unwrap();
        assert_eq!(collection.name, "Legacy");
        assert!(collection.description.is_none());
        assert!(collection.variables.is_empty());
    }
}
