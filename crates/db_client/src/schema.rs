use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FkInfo {
    pub name: String,
    pub from_column: String,
    pub to_table: String,
    pub to_column: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseInfo {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TableKind {
    Table,
    View,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableInfo {
    pub name: String,
    pub kind: TableKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnInfo {
    pub name: String,
    pub data_type: String,
    pub is_nullable: bool,
    pub column_key: Option<String>,
    pub default_value: Option<String>,
    pub extra: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexInfo {
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
    pub index_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckConstraintInfo {
    pub name: String,
    pub expression: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcedureInfo {
    pub name: String,
    pub kind: ProcedureKind,
    pub definition: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ProcedureKind {
    Procedure,
    Function,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggerInfo {
    pub name: String,
    pub event: String,
    pub timing: String,
    pub table_name: String,
    pub definition: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserInfo {
    pub name: String,
    pub host: String,
    pub grants: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SequenceInfo {
    pub name: String,
    pub current_value: Option<i64>,
    pub increment: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventInfo {
    pub name: String,
    pub status: Option<String>,
    pub definition: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
    pub rows_affected: u64,
    pub execution_time_ms: u64,
    /// Phase breakdown of `execution_time_ms`, when the provider measured
    /// one. `#[serde(default)]` so cached/persisted results from before this
    /// field existed still deserialize. Absent (not zeroed) for providers
    /// that only ever measured the total.
    #[serde(default)]
    pub timing: Option<QueryTiming>,
    // Pretty-printed source documents, one per row, in the same order as
    // `rows`. Only populated by document-shaped providers (currently
    // MongoDB); `columns`/`rows` still carry the flattened projection so
    // every other consumer (export, cell editing, other providers) is
    // unaffected by this field's presence.
    #[serde(default)]
    pub raw_documents: Option<Vec<String>>,
}

/// Phase breakdown for one query's `execution_time_ms`. Every field here is
/// something the provider actually measured -- never a guess -- so a
/// provider that cannot cheaply separate a phase must leave it `None`/absent
/// rather than fabricate a split of the total.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
pub struct QueryTiming {
    /// Time spent obtaining a live connection from the pool, including the
    /// liveness probe/reconnect run when the existing one had gone stale.
    pub pool_wait_ms: u64,
    /// Time from submitting the query to the server until it started
    /// returning data (reads), or until the server finished it (writes).
    pub execute_ms: u64,
    /// Time spent pulling and decoding rows after the first one arrived.
    /// `None` for writes and for reads that returned zero rows, since there
    /// was nothing left to stream.
    pub streaming_ms: Option<u64>,
}

impl QueryTiming {
    /// Sum of every measured phase. Compared against `execution_time_ms` in
    /// tests as a sanity check that the phases don't double-count or drop
    /// time relative to the total the caller already measured.
    pub fn total_ms(&self) -> u64 {
        self.pool_wait_ms + self.execute_ms + self.streaming_ms.unwrap_or(0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableStructure {
    pub table: TableInfo,
    pub columns: Vec<ColumnInfo>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn total_ms_sums_every_measured_phase() {
        let timing = QueryTiming {
            pool_wait_ms: 3,
            execute_ms: 12,
            streaming_ms: Some(5),
        };
        assert_eq!(timing.total_ms(), 20);
    }

    #[test]
    fn total_ms_treats_absent_streaming_as_zero() {
        let timing = QueryTiming {
            pool_wait_ms: 2,
            execute_ms: 8,
            streaming_ms: None,
        };
        assert_eq!(timing.total_ms(), 10);
    }

    #[test]
    fn query_result_deserializes_without_a_timing_field() {
        let json = r#"{
            "columns": ["id"],
            "rows": [],
            "rows_affected": 0,
            "execution_time_ms": 42
        }"#;
        let result: QueryResult = serde_json::from_str(json).expect("legacy shape must parse");
        assert!(result.timing.is_none());
    }
}
