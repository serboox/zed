use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::request::RequestId;

/// A flat chronological log of every sent request, independent of any
/// per-request response cache -- a request can be edited or deleted after
/// being sent, but its history entries stay put, so `request_id` is kept
/// only as a best-effort link back (`None` once the request no longer
/// exists, which the UI treats the same as "request deleted").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub id: Uuid,
    pub request_id: Option<RequestId>,
    pub method: String,
    pub url: String,
    pub status: Option<u16>,
    pub sent_at_unix_ms: u64,
}

impl HistoryEntry {
    pub fn new(
        request_id: RequestId,
        method: String,
        url: String,
        status: Option<u16>,
        sent_at_unix_ms: u64,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            request_id: Some(request_id),
            method,
            url,
            status,
            sent_at_unix_ms,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_history_entry_carries_the_request_id_and_given_fields() {
        let request_id = Uuid::new_v4();
        let entry = HistoryEntry::new(
            request_id,
            "GET".into(),
            "https://api.example.com".into(),
            Some(200),
            1_700_000_000_000,
        );
        assert_eq!(entry.request_id, Some(request_id));
        assert_eq!(entry.method, "GET");
        assert_eq!(entry.status, Some(200));
        assert_eq!(entry.sent_at_unix_ms, 1_700_000_000_000);
    }
}
