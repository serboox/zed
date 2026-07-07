use anyhow::Result;
use db_client::provider::RowSink;
use gpui::{App, Task, prelude::*};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExecuteToFileFormat {
    Csv,
    Tsv,
}

impl ExecuteToFileFormat {
    pub fn default_file_name(self) -> &'static str {
        match self {
            ExecuteToFileFormat::Csv => "query-result.csv",
            ExecuteToFileFormat::Tsv => "query-result.tsv",
        }
    }

    /// Infers the export format from the file extension the user actually
    /// picked in the save dialog, so typing "results.tsv" produces
    /// tab-separated output instead of always defaulting to CSV.
    pub fn for_path(path: &Path) -> Self {
        match path.extension().and_then(|ext| ext.to_str()) {
            Some(ext) if ext.eq_ignore_ascii_case("tsv") => ExecuteToFileFormat::Tsv,
            _ => ExecuteToFileFormat::Csv,
        }
    }

    fn separator(self) -> char {
        match self {
            ExecuteToFileFormat::Csv => ',',
            ExecuteToFileFormat::Tsv => '\t',
        }
    }

    fn format_cell(self, cell: &str) -> String {
        match self {
            // Mirrors ResultView::export_csv's quoting rule exactly, so a
            // streamed export and the grid's own "Save as CSV" produce
            // byte-identical formatting for the same data.
            ExecuteToFileFormat::Csv => {
                if cell.contains(',') || cell.contains('"') || cell.contains('\n') {
                    format!("\"{}\"", cell.replace('"', "\"\""))
                } else {
                    cell.to_string()
                }
            }
            ExecuteToFileFormat::Tsv => cell.to_string(),
        }
    }
}

/// Streams query rows straight to a file as they arrive, so exporting a
/// result set far larger than the grid's row cap never holds the whole
/// thing in memory. `cancelled` is checked before every write; once set, the
/// next write fails, which unwinds the streaming query and lets the caller
/// delete the partial file.
pub struct FileRowSink {
    writer: BufWriter<File>,
    format: ExecuteToFileFormat,
    cancelled: Arc<AtomicBool>,
    rows_written: u64,
}

impl FileRowSink {
    pub fn create(
        path: &Path,
        format: ExecuteToFileFormat,
        cancelled: Arc<AtomicBool>,
    ) -> std::io::Result<Self> {
        let file = File::create(path)?;
        Ok(Self {
            writer: BufWriter::new(file),
            format,
            cancelled,
            rows_written: 0,
        })
    }

    fn check_cancelled(&self) -> Result<()> {
        if self.cancelled.load(Ordering::Relaxed) {
            anyhow::bail!("export cancelled");
        }
        Ok(())
    }

    /// Flushes buffered writes to disk. Must be called (and its error
    /// checked) before treating a stream as successfully completed --
    /// `BufWriter`'s `Drop` impl flushes too, but silently discards any I/O
    /// error at that point.
    pub fn finish(mut self) -> Result<u64> {
        self.writer.flush()?;
        Ok(self.rows_written)
    }
}

impl RowSink for FileRowSink {
    fn write_columns(&mut self, columns: &[String]) -> Result<()> {
        self.check_cancelled()?;
        let sep = self.format.separator();
        let line = columns
            .iter()
            .map(|c| self.format.format_cell(c))
            .collect::<Vec<_>>()
            .join(&sep.to_string());
        writeln!(self.writer, "{line}")?;
        Ok(())
    }

    fn write_row(&mut self, row: &[Option<String>]) -> Result<()> {
        self.check_cancelled()?;
        let sep = self.format.separator();
        let cells: Vec<String> = row
            .iter()
            .map(|cell| self.format.format_cell(cell.as_deref().unwrap_or("")))
            .collect();
        writeln!(self.writer, "{}", cells.join(&sep.to_string()))?;
        self.rows_written += 1;
        Ok(())
    }
}

/// Runs a query and streams its full result to `output_path` in the
/// background, returning the row count on success. On any failure
/// (including cancellation), the partial file is removed so a failed or
/// cancelled export never leaves a truncated file that looks complete.
pub fn spawn_execute_to_file(
    provider: Arc<dyn db_client::provider::DbProvider>,
    database: String,
    sql: String,
    output_path: PathBuf,
    format: ExecuteToFileFormat,
    cancelled: Arc<AtomicBool>,
    cx: &App,
) -> Task<Result<u64, String>> {
    cx.background_spawn(async move {
        let mut sink = FileRowSink::create(&output_path, format, cancelled)
            .map_err(|error| error.to_string())?;
        let stream_result = provider
            .execute_query_streaming(&database, &sql, &mut sink)
            .await;
        match stream_result {
            Ok(_) => sink.finish().map_err(|error| error.to_string()),
            Err(error) => {
                drop(sink);
                std::fs::remove_file(&output_path).ok();
                Err(error.to_string())
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use db_client::provider::DbProvider;
    use db_client::schema::{ColumnInfo, DatabaseInfo, QueryResult, TableInfo};

    struct ManyRowsProvider {
        row_count: usize,
    }

    #[async_trait]
    impl DbProvider for ManyRowsProvider {
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
        async fn execute_query(&self, _database: &str, _sql: &str) -> Result<QueryResult> {
            unreachable!("this fake only exercises execute_query_streaming")
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
            sink.write_columns(&["id".to_string(), "note".to_string()])?;
            for i in 0..self.row_count {
                sink.write_row(&[Some(i.to_string()), Some(format!("row {i}, with a comma"))])?;
            }
            Ok(self.row_count as u64)
        }
    }

    #[test]
    fn for_path_infers_tsv_from_extension_and_defaults_to_csv() {
        assert_eq!(
            ExecuteToFileFormat::for_path(Path::new("/tmp/results.tsv")),
            ExecuteToFileFormat::Tsv
        );
        assert_eq!(
            ExecuteToFileFormat::for_path(Path::new("/tmp/RESULTS.TSV")),
            ExecuteToFileFormat::Tsv,
            "extension matching must be case-insensitive"
        );
        assert_eq!(
            ExecuteToFileFormat::for_path(Path::new("/tmp/results.csv")),
            ExecuteToFileFormat::Csv
        );
        assert_eq!(
            ExecuteToFileFormat::for_path(Path::new("/tmp/results")),
            ExecuteToFileFormat::Csv,
            "a path with no extension must fall back to CSV"
        );
    }

    #[test]
    fn csv_row_sink_quotes_commas_and_reports_the_true_row_count() {
        let dir = std::env::temp_dir().join(format!(
            "db_client_execute_to_file_test_{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.csv");
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut sink = FileRowSink::create(&path, ExecuteToFileFormat::Csv, cancelled).unwrap();
        sink.write_columns(&["id".to_string(), "note".to_string()])
            .unwrap();
        sink.write_row(&[Some("1".to_string()), Some("has, a comma".to_string())])
            .unwrap();
        let rows = sink.finish().unwrap();
        assert_eq!(rows, 1);

        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents, "id,note\n1,\"has, a comma\"\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[gpui::test]
    async fn streaming_beyond_the_grid_row_cap_writes_every_row_to_disk(
        cx: &mut gpui::TestAppContext,
    ) {
        // Proves execute_query_streaming's bypass of the grid's row cap
        // actually reaches the file, not just the trait default (which would
        // silently re-cap at MAX_RESULT_ROWS via execute_query).
        let row_count = 5_000;
        let provider: Arc<dyn db_client::provider::DbProvider> =
            Arc::new(ManyRowsProvider { row_count });
        let dir = std::env::temp_dir().join(format!(
            "db_client_execute_to_file_test_{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.csv");
        let cancelled = Arc::new(AtomicBool::new(false));

        let task = cx.update(|cx| {
            spawn_execute_to_file(
                provider,
                String::new(),
                "SELECT * FROM huge_table".to_string(),
                path.clone(),
                ExecuteToFileFormat::Csv,
                cancelled,
                cx,
            )
        });
        let result = task.await;
        assert_eq!(result, Ok(row_count as u64));

        let contents = std::fs::read_to_string(&path).unwrap();
        let mut lines = contents.lines();
        assert_eq!(lines.next(), Some("id,note"));
        assert_eq!(
            lines.count(),
            row_count,
            "every row beyond MAX_RESULT_ROWS must still reach the file"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[gpui::test]
    async fn cancelling_mid_stream_deletes_the_partial_file(cx: &mut gpui::TestAppContext) {
        let provider: Arc<dyn db_client::provider::DbProvider> =
            Arc::new(ManyRowsProvider { row_count: 10_000 });
        let dir = std::env::temp_dir().join(format!(
            "db_client_execute_to_file_test_{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.csv");
        let cancelled = Arc::new(AtomicBool::new(false));
        // Flip the flag before the stream even starts consuming rows -- the
        // fake provider writes synchronously in one go, so "mid-stream" for a
        // fake this fast means "before the first write" in practice; the
        // real MySQL/Postgres streams yield to the executor between rows
        // (a network round trip per batch), which is where a real cancel
        // click would land instead.
        cancelled.store(true, Ordering::Relaxed);

        let task = cx.update(|cx| {
            spawn_execute_to_file(
                provider,
                String::new(),
                "SELECT * FROM huge_table".to_string(),
                path.clone(),
                ExecuteToFileFormat::Csv,
                cancelled,
                cx,
            )
        });
        let result = task.await;
        assert!(
            result.is_err(),
            "a cancelled export must report failure, not silent success"
        );
        assert!(
            !path.exists(),
            "a cancelled export must delete its partial file, not leave a truncated one behind"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
