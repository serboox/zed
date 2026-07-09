use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::collection::CollectionId;

pub type FolderId = Uuid;

/// A folder node in a collection's request tree. Mirrors
/// `db_client::connection::Folder` (id/parent_id/order shape, reused verbatim
/// by the folder drag-and-drop/reorder code lifted from `db_client_ui`), plus
/// a `collection_id` since every folder here is always scoped to exactly one
/// collection rather than sitting in a single flat connection list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Folder {
    pub id: FolderId,
    pub collection_id: CollectionId,
    pub name: String,
    #[serde(default)]
    pub parent_id: Option<FolderId>,
    #[serde(default)]
    pub order: i64,
}

impl Folder {
    pub fn new(
        collection_id: CollectionId,
        name: String,
        parent_id: Option<FolderId>,
        order: i64,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            collection_id,
            name,
            parent_id,
            order,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_folder_is_scoped_to_its_collection() {
        let collection_id = Uuid::new_v4();
        let folder = Folder::new(collection_id, "Auth".to_string(), None, 0);
        assert_eq!(folder.collection_id, collection_id);
        assert!(folder.parent_id.is_none());
    }

    #[test]
    fn round_trips_through_serde_with_defaults_for_missing_optional_fields() {
        let json = format!(
            r#"{{"id":"{}","collection_id":"{}","name":"Legacy"}}"#,
            Uuid::new_v4(),
            Uuid::new_v4()
        );
        let folder: Folder = serde_json::from_str(&json).unwrap();
        assert_eq!(folder.name, "Legacy");
        assert!(folder.parent_id.is_none());
        assert_eq!(folder.order, 0);
    }
}
