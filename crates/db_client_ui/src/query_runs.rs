use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use db_client::ConnectionId;
use futures::channel::oneshot;

/// A statement that has been sent to a server and not yet answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunningQuery {
    pub id: u64,
    pub connection: ConnectionId,
    pub database: String,
    /// The first line of the statement, for lists.
    pub summary: String,
    pub started: Instant,
    /// A stop was asked for and the server has not yet ended the statement.
    pub stopping: bool,
}

struct Run {
    query: RunningQuery,
    /// Fired when the caller must stop waiting for the answer: the server was
    /// asked to stop the statement and did not, or cannot be asked.
    abandon: Option<oneshot::Sender<()>>,
}

/// Every statement being run, across connections, shared between the calls that
/// run them and whatever draws or stops them.
#[derive(Default)]
pub struct RunningQueries {
    runs: Mutex<Vec<Run>>,
    next_id: AtomicU64,
    /// Statements that were asked to stop and finished anyway, before the
    /// request reached them. The stop that asked says so instead of claiming it
    /// stopped something.
    completed_despite_stop: Mutex<std::collections::HashSet<u64>>,
}

/// A statement counted as running for as long as this is held, including when
/// the task holding it is dropped before the statement ends.
pub struct RunGuard {
    queries: Arc<RunningQueries>,
    id: u64,
}

impl RunningQueries {
    /// Counts a statement as running. The receiver resolves when the caller has
    /// to give up on it.
    pub fn begin(
        self: &Arc<Self>,
        connection: ConnectionId,
        database: &str,
        sql: &str,
    ) -> (RunGuard, oneshot::Receiver<()>) {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (abandon, abandoned) = oneshot::channel();
        self.lock().push(Run {
            query: RunningQuery {
                id,
                connection,
                database: database.to_string(),
                summary: summary_of(sql),
                started: Instant::now(),
                stopping: false,
            },
            abandon: Some(abandon),
        });
        (
            RunGuard {
                queries: self.clone(),
                id,
            },
            abandoned,
        )
    }

    pub fn all(&self) -> Vec<RunningQuery> {
        self.lock().iter().map(|run| run.query.clone()).collect()
    }

    pub fn of_connection(&self, connection: ConnectionId) -> Vec<RunningQuery> {
        self.lock()
            .iter()
            .filter(|run| run.query.connection == connection)
            .map(|run| run.query.clone())
            .collect()
    }

    pub fn is_running(&self, id: u64) -> bool {
        self.lock().iter().any(|run| run.query.id == id)
    }

    pub fn is_stopping(&self, id: u64) -> bool {
        self.lock()
            .iter()
            .any(|run| run.query.id == id && run.query.stopping)
    }

    /// Marks the statements of `connection` as being stopped, and says which.
    pub fn mark_stopping(&self, connection: ConnectionId) -> Vec<u64> {
        self.lock()
            .iter_mut()
            .filter(|run| run.query.connection == connection)
            .map(|run| {
                run.query.stopping = true;
                run.query.id
            })
            .collect()
    }

    /// Notes that statement `id` answered successfully although it was asked to
    /// stop.
    pub fn note_completed_despite_stop(&self, id: u64) {
        self.completed_despite_stop
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id);
    }

    /// Whether statement `id` finished despite the stop, forgetting it.
    pub fn take_completed_despite_stop(&self, id: u64) -> bool {
        self.completed_despite_stop
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&id)
    }

    /// Tells the callers of `ids` to stop waiting for their statements.
    pub fn abandon(&self, ids: &[u64]) {
        for run in self.lock().iter_mut() {
            if ids.contains(&run.query.id)
                && let Some(abandon) = run.abandon.take()
            {
                abandon.send(()).ok();
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Run>> {
        self.runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl RunGuard {
    pub fn id(&self) -> u64 {
        self.id
    }
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        self.queries.lock().retain(|run| run.query.id != self.id);
    }
}

/// The first line of a statement that says something, for a list.
fn summary_of(sql: &str) -> String {
    const LONGEST_SUMMARY: usize = 120;
    let line = sql
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with("--") && !line.starts_with('#'))
        .unwrap_or_default();
    if line.chars().count() <= LONGEST_SUMMARY {
        return line.to_string();
    }
    let cut: String = line.chars().take(LONGEST_SUMMARY).collect();
    format!("{cut}…")
}

/// The answer of a statement that was stopped on request, whatever the server
/// sent back: a statement the server was told to stop can still finish with
/// rows, and those rows are not wanted.
#[derive(Debug)]
pub struct QueryCancelled;

impl std::fmt::Display for QueryCancelled {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Cancelled")
    }
}

impl std::error::Error for QueryCancelled {}

/// Whether `error` says the statement was stopped on request.
pub fn is_cancelled(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.downcast_ref::<QueryCancelled>().is_some())
}

/// What stopping a connection's statements did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CancelReport {
    /// How many statements were running when the stop was asked for.
    pub stopped: usize,
    /// How many of them had already finished when the request reached them.
    pub finished_first: usize,
    /// The server was asked to stop them, and said it would. Otherwise the
    /// caller only stopped waiting, and the server may well finish the work.
    pub server_stopped: bool,
    /// The server ended the whole session because the statement ignored the
    /// first request.
    pub session_ended: bool,
    /// The transaction the connection held was rolled back.
    pub rolled_back: bool,
    /// Why the rollback could not be done, when it could not.
    pub rollback_failed: Option<String>,
}

impl CancelReport {
    /// One or two sentences for the reader, saying what is true and no more:
    /// whether the server stopped the statement, and what became of the
    /// transaction.
    pub fn describe(&self) -> String {
        let mut text = match (self.server_stopped, self.session_ended) {
            _ if self.stopped > 0 && self.finished_first >= self.stopped => {
                "Not stopped: the statement had already finished.".to_string()
            }
            (true, false) => "Cancelled. The server stopped the statement.".to_string(),
            (true, true) => {
                "Cancelled. The statement ignored the request, so the server ended the session."
                    .to_string()
            }
            (false, _) => {
                "Stopped waiting. This server cannot be asked to stop a statement, so it may \
                 still be running it."
                    .to_string()
            }
        };
        if self.rolled_back {
            text.push_str(" The transaction was rolled back.");
        }
        if let Some(reason) = &self.rollback_failed {
            text.push_str(&format!(
                " The transaction could not be rolled back: {reason}"
            ));
        }
        text
    }
}

/// A server that runs a statement until it is stopped, for the tests of
/// everything that stops one.
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use anyhow::Result;
    use db_client::DbProvider;
    use db_client::interrupt::{Interrupt, Interrupted};
    use db_client::schema::{ColumnInfo, DatabaseInfo, QueryResult, TableInfo};

    pub struct HangingProvider {
        can_interrupt: bool,
        /// Whether a request to stop the statement does nothing, so that only
        /// ending the session ends it.
        ignores_the_statement_stop: bool,
        /// Every stop it was asked for, in order.
        pub interrupts: Arc<Mutex<Vec<Interrupt>>>,
        /// Every statement it was given, in order.
        pub statements: Arc<Mutex<Vec<String>>>,
        release: Mutex<Option<smol::channel::Sender<()>>>,
        pub transaction_open_since: Mutex<Option<Instant>>,
        pub rollbacks: Arc<AtomicUsize>,
        /// What the statement answers when it is released: by default the
        /// error a server gives for a statement it stopped.
        answer_when_released: Mutex<Option<std::result::Result<(), String>>>,
        /// Replaces the open transaction with a new one at the moment the stop
        /// arrives, the way another console opening one would.
        replace_the_transaction_on_interrupt: std::sync::atomic::AtomicBool,
    }

    impl HangingProvider {
        pub fn new(can_interrupt: bool, ignores_the_statement_stop: bool) -> Arc<Self> {
            Arc::new(Self {
                can_interrupt,
                ignores_the_statement_stop,
                interrupts: Arc::new(Mutex::new(Vec::new())),
                statements: Arc::new(Mutex::new(Vec::new())),
                release: Mutex::new(None),
                transaction_open_since: Mutex::new(None),
                rollbacks: Arc::new(AtomicUsize::new(0)),
                answer_when_released: Mutex::new(None),
                replace_the_transaction_on_interrupt: std::sync::atomic::AtomicBool::new(false),
            })
        }

        /// The statement ends with a result when released, as one that was
        /// already done would.
        pub fn finish_when_released(&self) {
            *self
                .answer_when_released
                .lock()
                .expect("the lock is not poisoned") = Some(Ok(()));
        }

        /// The statement ends with `message`, an error of its own that has
        /// nothing to do with being stopped.
        pub fn fail_when_released(&self, message: &str) {
            *self
                .answer_when_released
                .lock()
                .expect("the lock is not poisoned") = Some(Err(message.to_string()));
        }

        pub fn replace_the_transaction_when_stopped(&self) {
            self.replace_the_transaction_on_interrupt
                .store(true, Ordering::SeqCst);
        }

        pub fn hold_a_transaction(&self) {
            *self
                .transaction_open_since
                .lock()
                .expect("the lock is not poisoned") = Some(Instant::now());
        }
    }

    #[async_trait::async_trait]
    impl DbProvider for HangingProvider {
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
            self.statements
                .lock()
                .expect("the lock is not poisoned")
                .push(sql.trim().trim_end_matches(';').to_string());
            let (sender, receiver) = smol::channel::bounded(1);
            *self.release.lock().expect("the lock is not poisoned") = Some(sender);
            receiver.recv().await.ok();
            match self
                .answer_when_released
                .lock()
                .expect("the lock is not poisoned")
                .clone()
            {
                Some(Ok(())) => Ok(QueryResult {
                    columns: Vec::new(),
                    rows: Vec::new(),
                    rows_affected: 0,
                    execution_time_ms: 0,
                    timing: None,
                    raw_documents: None,
                }),
                Some(Err(message)) => Err(anyhow::anyhow!(message)),
                None => Err(anyhow::Error::new(db_client::interrupt::StatementCancelled)),
            }
        }
        async fn get_table_ddl(&self, _database: &str, _table: &str) -> Result<String> {
            Ok(String::new())
        }
        fn can_interrupt(&self) -> bool {
            self.can_interrupt
        }
        async fn interrupt(&self, how: Interrupt) -> Result<Interrupted> {
            self.interrupts
                .lock()
                .expect("the lock is not poisoned")
                .push(how);
            if self
                .replace_the_transaction_on_interrupt
                .load(Ordering::SeqCst)
            {
                *self
                    .transaction_open_since
                    .lock()
                    .expect("the lock is not poisoned") =
                    Some(Instant::now() + std::time::Duration::from_secs(1));
            }
            if (how == Interrupt::Session || !self.ignores_the_statement_stop)
                && let Some(release) = self
                    .release
                    .lock()
                    .expect("the lock is not poisoned")
                    .take()
            {
                release.try_send(()).ok();
            }
            Ok(Interrupted::Requested)
        }
        fn holds_transactions(&self) -> bool {
            true
        }
        fn transaction_open_since(&self) -> Option<Instant> {
            *self
                .transaction_open_since
                .lock()
                .expect("the lock is not poisoned")
        }
        async fn rollback_transaction(&self) -> Result<()> {
            self.rollbacks.fetch_add(1, Ordering::SeqCst);
            *self
                .transaction_open_since
                .lock()
                .expect("the lock is not poisoned") = None;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection() -> ConnectionId {
        ConnectionId::new_v4()
    }

    #[test]
    fn a_statement_counts_as_running_until_its_guard_is_dropped() {
        let queries = Arc::new(RunningQueries::default());
        let id = connection();
        let (guard, _abandoned) = queries.begin(id, "shop", "SELECT 1");
        assert!(queries.is_running(guard.id()));
        assert_eq!(queries.of_connection(id).len(), 1);
        let running = guard.id();
        drop(guard);
        assert!(!queries.is_running(running));
        assert!(queries.all().is_empty());
    }

    #[test]
    fn stopping_marks_only_the_statements_of_that_connection() {
        let queries = Arc::new(RunningQueries::default());
        let (first, second) = (connection(), connection());
        let (mine, _a) = queries.begin(first, "", "SELECT 1");
        let (other, _b) = queries.begin(second, "", "SELECT 2");
        assert_eq!(queries.mark_stopping(first), vec![mine.id()]);
        assert!(queries.is_stopping(mine.id()));
        assert!(!queries.is_stopping(other.id()));
    }

    #[test]
    fn abandoning_wakes_the_caller_that_waits() {
        let queries = Arc::new(RunningQueries::default());
        let (guard, mut abandoned) = queries.begin(connection(), "", "SELECT 1");
        assert!(matches!(abandoned.try_recv(), Ok(None)));
        queries.abandon(&[guard.id()]);
        assert!(matches!(abandoned.try_recv(), Ok(Some(()))));
    }

    #[test]
    fn a_statement_that_finished_despite_the_stop_is_remembered_once() {
        let queries = RunningQueries::default();
        assert!(!queries.take_completed_despite_stop(7));
        queries.note_completed_despite_stop(7);
        assert!(queries.take_completed_despite_stop(7));
        assert!(!queries.take_completed_despite_stop(7), "and only once");
    }

    #[test]
    fn a_summary_is_the_first_line_that_says_something() {
        assert_eq!(
            summary_of("-- a note\n\n  SELECT *\nFROM t"),
            "SELECT *".to_string()
        );
        assert_eq!(
            summary_of("# note\nUPDATE t SET a = 1"),
            "UPDATE t SET a = 1"
        );
        let long = format!("SELECT {}", "a, ".repeat(100));
        assert!(summary_of(&long).chars().count() <= 121);
        assert!(summary_of(&long).ends_with('…'));
    }

    #[test]
    fn a_cancelled_error_is_found_through_its_context() {
        let error = anyhow::Error::new(QueryCancelled).context("Query execution failed");
        assert!(is_cancelled(&error));
        assert!(!is_cancelled(&anyhow::anyhow!("syntax error")));
    }

    #[test]
    fn the_report_says_what_is_true_of_the_server_and_of_the_transaction() {
        let stopped = CancelReport {
            stopped: 1,
            server_stopped: true,
            ..CancelReport::default()
        };
        assert_eq!(
            stopped.describe(),
            "Cancelled. The server stopped the statement."
        );
        let rolled_back = CancelReport {
            rolled_back: true,
            ..stopped.clone()
        };
        assert_eq!(
            rolled_back.describe(),
            "Cancelled. The server stopped the statement. The transaction was rolled back."
        );
        let abandoned = CancelReport {
            stopped: 1,
            ..CancelReport::default()
        };
        assert!(abandoned.describe().starts_with("Stopped waiting."));
        assert!(!abandoned.describe().contains("Cancelled."));
        let late = CancelReport {
            stopped: 1,
            finished_first: 1,
            server_stopped: true,
            ..CancelReport::default()
        };
        assert_eq!(
            late.describe(),
            "Not stopped: the statement had already finished."
        );
        let ended = CancelReport {
            session_ended: true,
            ..stopped
        };
        assert!(ended.describe().contains("ended the session"));
        let failed = CancelReport {
            rollback_failed: Some("the connection is gone".into()),
            server_stopped: true,
            ..CancelReport::default()
        };
        assert!(failed.describe().contains("could not be rolled back"));
    }
}
