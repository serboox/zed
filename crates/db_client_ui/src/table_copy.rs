use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use db_client::connection::DatabaseDriver;
use db_client::provider::{DbProvider, RowSink};
use db_client::schema::ColumnInfo;
use gpui::{App, AppContext as _, Task};

const COPY_BATCH_SIZE: usize = 500;

fn quote_value(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// A coarse type category a source column's native type is bucketed into
/// before being re-expressed in the target driver's own syntax. Cross-driver
/// type systems don't line up 1:1, so this is deliberately approximate --
/// exact within a driver (see `map_column_type`), best-effort across drivers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TypeCategory {
    Integer,
    BigInteger,
    Float,
    Decimal,
    Boolean,
    Date,
    DateTime,
    Text,
    Unknown,
}

fn categorize(source_type: &str) -> TypeCategory {
    let lower = source_type.to_ascii_lowercase();
    if lower.contains("bigint") {
        TypeCategory::BigInteger
    } else if lower.contains("int") {
        TypeCategory::Integer
    } else if lower.contains("bool") {
        TypeCategory::Boolean
    } else if lower.contains("double") || lower.contains("float") || lower.contains("real") {
        TypeCategory::Float
    } else if lower.contains("decimal") || lower.contains("numeric") {
        TypeCategory::Decimal
    } else if lower.contains("timestamp") || lower.contains("datetime") {
        TypeCategory::DateTime
    } else if lower == "date" {
        TypeCategory::Date
    } else if lower.contains("char") || lower.contains("text") || lower.contains("clob") {
        TypeCategory::Text
    } else {
        TypeCategory::Unknown
    }
}

fn type_for_category(target: DatabaseDriver, category: TypeCategory) -> &'static str {
    match (target, category) {
        (DatabaseDriver::PostgreSQL, TypeCategory::Integer) => "integer",
        (DatabaseDriver::PostgreSQL, TypeCategory::BigInteger) => "bigint",
        (DatabaseDriver::PostgreSQL, TypeCategory::Float) => "double precision",
        (DatabaseDriver::PostgreSQL, TypeCategory::Decimal) => "numeric",
        (DatabaseDriver::PostgreSQL, TypeCategory::Boolean) => "boolean",
        (DatabaseDriver::PostgreSQL, TypeCategory::Date) => "date",
        (DatabaseDriver::PostgreSQL, TypeCategory::DateTime) => "timestamp",
        (DatabaseDriver::PostgreSQL, TypeCategory::Text) => "text",

        (DatabaseDriver::MySQL, TypeCategory::Integer) => "int",
        (DatabaseDriver::MySQL, TypeCategory::BigInteger) => "bigint",
        (DatabaseDriver::MySQL, TypeCategory::Float) => "double",
        (DatabaseDriver::MySQL, TypeCategory::Decimal) => "decimal(18,4)",
        (DatabaseDriver::MySQL, TypeCategory::Boolean) => "tinyint(1)",
        (DatabaseDriver::MySQL, TypeCategory::Date) => "date",
        (DatabaseDriver::MySQL, TypeCategory::DateTime) => "datetime",
        (DatabaseDriver::MySQL, TypeCategory::Text) => "text",

        (DatabaseDriver::SQLite, TypeCategory::Integer | TypeCategory::BigInteger) => "integer",
        (DatabaseDriver::SQLite, TypeCategory::Float | TypeCategory::Decimal) => "real",
        (DatabaseDriver::SQLite, TypeCategory::Boolean) => "integer",
        (DatabaseDriver::SQLite, TypeCategory::Date | TypeCategory::DateTime) => "text",
        (DatabaseDriver::SQLite, TypeCategory::Text) => "text",

        // ClickHouse, Redis, MongoDB, and any unmapped combination fall back
        // to a generic string type, flagged as inexact by the caller.
        _ => "text",
    }
}

/// Maps `source_type` (as reported by the source driver's introspection) to
/// the closest equivalent type in `target`'s own SQL dialect. Returns the
/// mapped type and whether the mapping is a known-good equivalence (`true`)
/// or a generic fallback the caller should warn about (`false`).
///
/// A same-driver copy always passes the source type through unchanged and
/// exact -- the common case, and the only one that is guaranteed lossless.
pub fn map_column_type(
    source: DatabaseDriver,
    target: DatabaseDriver,
    source_type: &str,
) -> (String, bool) {
    if source == target {
        return (source_type.to_string(), true);
    }
    let category = categorize(source_type);
    let mapped = type_for_category(target, category);
    (mapped.to_string(), category != TypeCategory::Unknown)
}

/// Generates a `CREATE TABLE` statement for `target` from `columns`, mapping
/// each column's type from `source`. Returns the statement plus one warning
/// string per column whose type fell back to a generic mapping.
pub fn generate_create_table_sql(
    source: DatabaseDriver,
    target: DatabaseDriver,
    table: &str,
    columns: &[ColumnInfo],
) -> (String, Vec<String>) {
    let mut warnings = Vec::new();
    let column_defs = columns
        .iter()
        .map(|column| {
            let (mapped_type, exact) = map_column_type(source, target, &column.data_type);
            if !exact {
                warnings.push(format!(
                    "Column \"{}\" ({}) has no exact {target} equivalent -- using \"{mapped_type}\".",
                    column.name, column.data_type
                ));
            }
            let nullable = if column.is_nullable { "" } else { " NOT NULL" };
            format!(
                "{} {mapped_type}{nullable}",
                target.quote_identifier(&column.name)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "CREATE TABLE {} ({column_defs})",
        target.quote_identifier(table)
    );
    (sql, warnings)
}

/// Whether `existing`'s columns look compatible enough with `source`'s to
/// safely copy rows into (same count and, position-for-position, the same
/// names) -- not a type check, since cross-driver type names never match
/// exactly by design.
pub fn columns_look_compatible(source: &[ColumnInfo], existing: &[ColumnInfo]) -> bool {
    source.len() == existing.len()
        && source
            .iter()
            .zip(existing)
            .all(|(a, b)| a.name.eq_ignore_ascii_case(&b.name))
}

/// Builds one multi-row `INSERT` statement covering every row in `rows`
/// (callers are expected to pass one already-sized batch, not the whole
/// table, so a single statement never grows unbounded).
pub fn build_copy_insert_statement(
    target: DatabaseDriver,
    table: &str,
    columns: &[String],
    rows: &[Vec<Option<String>>],
) -> Option<String> {
    if rows.is_empty() || columns.is_empty() {
        return None;
    }
    let columns_sql = columns
        .iter()
        .map(|name| target.quote_identifier(name))
        .collect::<Vec<_>>()
        .join(", ");
    let value_groups = rows
        .iter()
        .map(|row| {
            let cells = row
                .iter()
                .map(|cell| match cell {
                    Some(value) => quote_value(value),
                    None => "NULL".to_string(),
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("({cells})")
        })
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "INSERT INTO {} ({columns_sql}) VALUES {value_groups}",
        target.quote_identifier(table)
    ))
}

/// Buffers streamed source rows into fixed-size batches and hands each full
/// batch to the target connection over a channel, so the (synchronous)
/// `RowSink` callback never has to await an insert itself -- the streaming
/// query and the target inserts run as two independent, concurrently
/// progressing tasks (see `spawn_table_copy`).
struct TableCopySink {
    buffer: Vec<Vec<Option<String>>>,
    sender: smol::channel::Sender<Vec<Vec<Option<String>>>>,
    cancelled: Arc<AtomicBool>,
}

impl RowSink for TableCopySink {
    fn write_columns(&mut self, _columns: &[String]) -> Result<()> {
        Ok(())
    }

    fn write_row(&mut self, row: &[Option<String>]) -> Result<()> {
        if self.cancelled.load(Ordering::Relaxed) {
            anyhow::bail!("table copy cancelled");
        }
        self.buffer.push(row.to_vec());
        if self.buffer.len() >= COPY_BATCH_SIZE {
            let batch = std::mem::take(&mut self.buffer);
            self.sender
                .send_blocking(batch)
                .map_err(|_| anyhow::anyhow!("target insert task is no longer receiving"))?;
        }
        Ok(())
    }
}

/// Copies every row of `source_table` (on `source_provider`) into
/// `target_table` (on `target_provider`), streaming through a bounded
/// channel so the full result set is never held in memory. If
/// `existing_target_columns` is `None`, a `CREATE TABLE` is issued first
/// (type-mapped from `source_driver` to `target_driver`); if `Some`, the
/// table is assumed to already exist and rows are inserted directly.
/// Cancelling stops issuing further inserts but does not undo rows already
/// written -- unlike a partial exported file, a partial copy of live data in
/// another connection is not something this action silently reverts.
#[allow(clippy::too_many_arguments)]
pub fn spawn_table_copy(
    source_provider: Arc<dyn DbProvider>,
    source_database: String,
    source_table: String,
    source_driver: DatabaseDriver,
    source_columns: Vec<ColumnInfo>,
    target_provider: Arc<dyn DbProvider>,
    target_database: String,
    target_table: String,
    target_driver: DatabaseDriver,
    existing_target_columns: Option<Vec<ColumnInfo>>,
    cancelled: Arc<AtomicBool>,
    cx: &App,
) -> Task<Result<u64, String>> {
    cx.background_spawn(async move {
        if let Some(existing) = &existing_target_columns {
            if !columns_look_compatible(&source_columns, existing) {
                return Err(format!(
                    "\"{target_table}\" already exists with a different column layout \
                     ({} column(s) vs {} on the source) -- copy aborted rather than risk \
                     writing rows into the wrong columns.",
                    existing.len(),
                    source_columns.len()
                ));
            }
        } else {
            let (create_sql, _warnings) = generate_create_table_sql(
                source_driver,
                target_driver,
                &target_table,
                &source_columns,
            );
            target_provider
                .execute_query(&target_database, &create_sql)
                .await
                .map_err(|error| error.to_string())?;
        }

        let column_names: Vec<String> = source_columns.iter().map(|c| c.name.clone()).collect();
        let (sender, receiver) = smol::channel::bounded(4);
        let mut sink = TableCopySink {
            buffer: Vec::new(),
            sender,
            cancelled: cancelled.clone(),
        };

        let select_sql = format!(
            "SELECT * FROM {}",
            source_driver.quote_identifier(&source_table)
        );
        let producer = async move {
            let result = source_provider
                .execute_query_streaming(&source_database, &select_sql, &mut sink)
                .await;
            let leftover = std::mem::take(&mut sink.buffer);
            if !leftover.is_empty() && result.is_ok() {
                sink.sender.send(leftover).await.ok();
            }
            drop(sink);
            result.map_err(|error| error.to_string())
        };
        let consumer = async move {
            let mut total = 0u64;
            while let Ok(batch) = receiver.recv().await {
                let Some(statement) = build_copy_insert_statement(
                    target_driver,
                    &target_table,
                    &column_names,
                    &batch,
                ) else {
                    continue;
                };
                target_provider
                    .execute_query(&target_database, &statement)
                    .await
                    .map_err(|error| error.to_string())?;
                total += batch.len() as u64;
            }
            Ok::<u64, String>(total)
        };

        let (produced, consumed) = futures::join!(producer, consumer);
        produced?;
        consumed
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use db_client::schema::{DatabaseInfo, QueryResult, TableInfo};

    fn column(name: &str, data_type: &str, is_nullable: bool) -> ColumnInfo {
        ColumnInfo {
            name: name.to_string(),
            data_type: data_type.to_string(),
            is_nullable,
            column_key: None,
            default_value: None,
            extra: String::new(),
        }
    }

    #[test]
    fn same_driver_copy_passes_types_through_unchanged() {
        let (mapped, exact) =
            map_column_type(DatabaseDriver::MySQL, DatabaseDriver::MySQL, "varchar(255)");
        assert_eq!(mapped, "varchar(255)");
        assert!(exact);
    }

    #[test]
    fn cross_driver_common_types_map_to_known_equivalents() {
        let (mapped, exact) =
            map_column_type(DatabaseDriver::MySQL, DatabaseDriver::PostgreSQL, "int(11)");
        assert_eq!(mapped, "integer");
        assert!(exact);

        let (mapped, exact) = map_column_type(
            DatabaseDriver::PostgreSQL,
            DatabaseDriver::MySQL,
            "timestamp",
        );
        assert_eq!(mapped, "datetime");
        assert!(exact);
    }

    #[test]
    fn unrecognized_type_falls_back_to_text_and_is_flagged() {
        let (mapped, exact) =
            map_column_type(DatabaseDriver::PostgreSQL, DatabaseDriver::MySQL, "jsonb");
        assert_eq!(mapped, "text");
        assert!(
            !exact,
            "an unmapped type must be reported as a fallback, not silently exact"
        );
    }

    #[test]
    fn create_table_sql_quotes_identifiers_and_marks_nullability() {
        let columns = vec![
            column("id", "int", false),
            column("name", "varchar(255)", true),
        ];
        let (sql, warnings) = generate_create_table_sql(
            DatabaseDriver::MySQL,
            DatabaseDriver::MySQL,
            "people",
            &columns,
        );
        assert_eq!(
            sql,
            "CREATE TABLE `people` (`id` int NOT NULL, `name` varchar(255))"
        );
        assert!(warnings.is_empty(), "a same-driver copy must never warn");
    }

    #[test]
    fn create_table_sql_warns_on_fallback_types() {
        let columns = vec![column("payload", "jsonb", true)];
        let (_, warnings) = generate_create_table_sql(
            DatabaseDriver::PostgreSQL,
            DatabaseDriver::MySQL,
            "events",
            &columns,
        );
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("payload"));
    }

    #[test]
    fn compatible_columns_require_same_count_and_names_in_order() {
        let source = vec![column("id", "int", false), column("name", "text", true)];
        let same = vec![
            column("id", "integer", false),
            column("name", "varchar", true),
        ];
        assert!(columns_look_compatible(&source, &same));

        let reordered = vec![column("name", "text", true), column("id", "int", false)];
        assert!(!columns_look_compatible(&source, &reordered));

        let fewer = vec![column("id", "int", false)];
        assert!(!columns_look_compatible(&source, &fewer));
    }

    #[test]
    fn insert_statement_quotes_strings_and_uses_null_for_none() {
        let rows = vec![
            vec![Some("1".to_string()), Some("O'Brien".to_string())],
            vec![Some("2".to_string()), None],
        ];
        let statement = build_copy_insert_statement(
            DatabaseDriver::PostgreSQL,
            "people",
            &["id".to_string(), "name".to_string()],
            &rows,
        )
        .expect("non-empty rows must produce a statement");
        assert_eq!(
            statement,
            "INSERT INTO \"people\" (\"id\", \"name\") VALUES ('1', 'O''Brien'), ('2', NULL)"
        );
    }

    #[test]
    fn insert_statement_is_none_for_an_empty_batch() {
        assert!(
            build_copy_insert_statement(DatabaseDriver::MySQL, "t", &["id".to_string()], &[])
                .is_none()
        );
    }

    struct RecordingProvider {
        rows: Vec<Vec<Option<String>>>,
        inserted: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl DbProvider for RecordingProvider {
        async fn ping(&self) -> Result<()> {
            Ok(())
        }
        async fn list_databases(&self) -> Result<Vec<DatabaseInfo>> {
            Ok(Vec::new())
        }
        async fn list_tables(&self, _database: &str) -> Result<Vec<TableInfo>> {
            Ok(Vec::new())
        }
        async fn describe_table(&self, _database: &str, _table: &str) -> Result<Vec<ColumnInfo>> {
            Ok(Vec::new())
        }
        async fn execute_query(&self, _database: &str, sql: &str) -> Result<QueryResult> {
            self.inserted.lock().unwrap().push(sql.to_string());
            Ok(QueryResult {
                columns: Vec::new(),
                rows: Vec::new(),
                rows_affected: 0,
                execution_time_ms: 0,
            })
        }
        async fn get_table_ddl(&self, _database: &str, _table: &str) -> Result<String> {
            Ok(String::new())
        }
        async fn execute_query_streaming(
            &self,
            _database: &str,
            _sql: &str,
            sink: &mut dyn RowSink,
        ) -> Result<u64> {
            sink.write_columns(&["id".to_string()])?;
            for row in &self.rows {
                sink.write_row(row)?;
            }
            Ok(self.rows.len() as u64)
        }
    }

    #[gpui::test]
    async fn spawn_table_copy_creates_the_table_then_batches_every_row(
        cx: &mut gpui::TestAppContext,
    ) {
        let rows: Vec<Vec<Option<String>>> = (0..1200).map(|i| vec![Some(i.to_string())]).collect();
        let source: Arc<dyn DbProvider> = Arc::new(RecordingProvider {
            rows: rows.clone(),
            inserted: Arc::new(std::sync::Mutex::new(Vec::new())),
        });
        let inserted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let target: Arc<dyn DbProvider> = Arc::new(RecordingProvider {
            rows: Vec::new(),
            inserted: inserted.clone(),
        });
        let cancelled = Arc::new(AtomicBool::new(false));

        let task = cx.update(|cx| {
            spawn_table_copy(
                source,
                "src_db".to_string(),
                "people".to_string(),
                DatabaseDriver::MySQL,
                vec![column("id", "int", false)],
                target,
                "dst_db".to_string(),
                "people".to_string(),
                DatabaseDriver::MySQL,
                None,
                cancelled,
                cx,
            )
        });
        let total = task.await.expect("copy must succeed");
        assert_eq!(total, 1200);

        let statements = inserted.lock().unwrap().clone();
        assert_eq!(statements[0], "CREATE TABLE `people` (`id` int NOT NULL)");
        let insert_statements = &statements[1..];
        assert_eq!(
            insert_statements.len(),
            3,
            "1200 rows at a 500-row batch size must produce 3 INSERTs (500, 500, 200)"
        );
        assert!(insert_statements[0].contains("VALUES"));
    }

    #[gpui::test]
    async fn spawn_table_copy_skips_create_table_when_columns_already_match(
        cx: &mut gpui::TestAppContext,
    ) {
        let source: Arc<dyn DbProvider> = Arc::new(RecordingProvider {
            rows: vec![vec![Some("1".to_string())]],
            inserted: Arc::new(std::sync::Mutex::new(Vec::new())),
        });
        let inserted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let target: Arc<dyn DbProvider> = Arc::new(RecordingProvider {
            rows: Vec::new(),
            inserted: inserted.clone(),
        });
        let cancelled = Arc::new(AtomicBool::new(false));

        let task = cx.update(|cx| {
            spawn_table_copy(
                source,
                "src_db".to_string(),
                "people".to_string(),
                DatabaseDriver::MySQL,
                vec![column("id", "int", false)],
                target,
                "dst_db".to_string(),
                "people".to_string(),
                DatabaseDriver::MySQL,
                Some(vec![column("id", "int", false)]),
                cancelled,
                cx,
            )
        });
        task.await.expect("copy must succeed");

        let statements = inserted.lock().unwrap().clone();
        assert_eq!(
            statements.len(),
            1,
            "no CREATE TABLE when columns already match"
        );
        assert!(statements[0].starts_with("INSERT INTO"));
    }

    #[gpui::test]
    async fn spawn_table_copy_rejects_an_incompatible_existing_table(
        cx: &mut gpui::TestAppContext,
    ) {
        let source: Arc<dyn DbProvider> = Arc::new(RecordingProvider {
            rows: vec![vec![Some("1".to_string())]],
            inserted: Arc::new(std::sync::Mutex::new(Vec::new())),
        });
        let inserted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let target: Arc<dyn DbProvider> = Arc::new(RecordingProvider {
            rows: Vec::new(),
            inserted: inserted.clone(),
        });
        let cancelled = Arc::new(AtomicBool::new(false));

        let task = cx.update(|cx| {
            spawn_table_copy(
                source,
                "src_db".to_string(),
                "people".to_string(),
                DatabaseDriver::MySQL,
                vec![column("id", "int", false)],
                target,
                "dst_db".to_string(),
                "people".to_string(),
                DatabaseDriver::MySQL,
                Some(vec![
                    column("id", "int", false),
                    column("extra", "text", true),
                ]),
                cancelled,
                cx,
            )
        });
        let error = task
            .await
            .expect_err("mismatched column count must be rejected");
        assert!(error.contains("different column layout"));
        assert!(
            inserted.lock().unwrap().is_empty(),
            "must not have inserted anything"
        );
    }

    #[gpui::test]
    async fn cancelling_mid_copy_stops_issuing_further_inserts(cx: &mut gpui::TestAppContext) {
        let rows: Vec<Vec<Option<String>>> = (0..2000).map(|i| vec![Some(i.to_string())]).collect();
        let source: Arc<dyn DbProvider> = Arc::new(RecordingProvider {
            rows,
            inserted: Arc::new(std::sync::Mutex::new(Vec::new())),
        });
        let inserted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let target: Arc<dyn DbProvider> = Arc::new(RecordingProvider {
            rows: Vec::new(),
            inserted: inserted.clone(),
        });
        let cancelled = Arc::new(AtomicBool::new(true));

        let task = cx.update(|cx| {
            spawn_table_copy(
                source,
                "src_db".to_string(),
                "people".to_string(),
                DatabaseDriver::MySQL,
                vec![column("id", "int", false)],
                target,
                "dst_db".to_string(),
                "people".to_string(),
                DatabaseDriver::MySQL,
                Some(vec![column("id", "int", false)]),
                cancelled,
                cx,
            )
        });
        let error = task
            .await
            .expect_err("a cancelled copy must report failure");
        assert!(error.contains("cancelled"));
        assert!(
            inserted.lock().unwrap().is_empty(),
            "a copy cancelled before it starts must not have inserted any rows"
        );
    }
}
