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
}

impl Collection {
    pub fn new(name: String) -> Self {
        Self {
            id: Uuid::new_v4(),
            name,
            description: None,
            variables: Vec::new(),
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
