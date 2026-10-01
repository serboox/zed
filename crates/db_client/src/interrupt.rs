use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// How hard a running statement is asked to stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interrupt {
    /// The server is asked to abandon the statement and keep the session.
    Statement,
    /// The server is asked to end the whole session, which also rolls back
    /// whatever transaction it held. Used when the statement ignores the first
    /// request.
    Session,
}

/// What an interrupt found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interrupted {
    /// The server was told to stop the statement.
    Requested,
    /// Nothing was running on this connection, or it ended on its own first.
    NothingRunning,
    /// This kind of connection cannot ask its server to stop a statement.
    Unsupported,
}

/// A statement that was told to stop before it began, or whose connection was
/// ended on purpose and so must not be tried again on a new one.
#[derive(Debug)]
pub struct StatementCancelled;

impl std::fmt::Display for StatementCancelled {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("The statement was cancelled")
    }
}

impl std::error::Error for StatementCancelled {}

/// The statement a connection is running right now, as the server can find it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningStatement {
    /// A comment put at the front of the statement's text. The server lists the
    /// statements it runs with their text, so this finds exactly this
    /// statement, on whichever physical connection it was sent over, without
    /// asking each connection for its own number.
    pub tag: String,
}

/// Keeps track of what a connection is running, so that something other than
/// the call that runs it can stop it.
///
/// A connection runs its statements one after another. Stopping one has to
/// touch neither the call running it nor the statements waiting behind it for
/// the connection, and must not hit a statement that begins a moment after the
/// request: the generation says which statements the request was meant for.
#[derive(Debug)]
pub struct StatementTracker {
    /// Moves on every cancel. A statement that arrived under an earlier number
    /// is one the cancel was meant for.
    generation: AtomicU64,
    next_tag: AtomicU64,
    /// Different for every tracker, so that two connections, in this process or
    /// another, never hand the server the same tag.
    owner: String,
    running: Mutex<Option<RunningStatement>>,
}

/// A statement's place in line, taken before it waits for the connection.
#[derive(Debug, Clone, Copy)]
pub struct Arrival {
    generation: u64,
}

/// Held while a statement runs; says so to the tracker for as long as it lives.
#[derive(Debug)]
pub struct RunningGuard<'a> {
    tracker: &'a StatementTracker,
    arrival: Arrival,
    tag: String,
}

impl StatementTracker {
    pub fn new() -> Self {
        Self {
            generation: AtomicU64::new(0),
            next_tag: AtomicU64::new(0),
            owner: uuid::Uuid::new_v4().simple().to_string(),
            running: Mutex::new(None),
        }
    }

    /// Takes a place in line. Called before waiting for the connection, so a
    /// cancel that comes while the statement waits reaches it.
    pub fn arrive(&self) -> Arrival {
        Arrival {
            generation: self.generation.load(Ordering::SeqCst),
        }
    }

    /// Marks the statement as running, once it has the connection. Refused
    /// when a cancel came after it arrived: it never reaches the server.
    pub fn start(&self, arrival: Arrival) -> Result<RunningGuard<'_>, StatementCancelled> {
        self.start_with(arrival, || {})
    }

    /// [`Self::start`], with a hook that runs between the check and the record,
    /// so that a test can put a cancel exactly there.
    fn start_with(
        &self,
        arrival: Arrival,
        between_the_check_and_the_record: impl FnOnce(),
    ) -> Result<RunningGuard<'_>, StatementCancelled> {
        // Checked and recorded under the same lock that `cancel` reads under, so
        // a cancel cannot slip in between and miss a statement that then starts.
        let mut running = self.lock();
        if self.generation.load(Ordering::SeqCst) != arrival.generation {
            return Err(StatementCancelled);
        }
        between_the_check_and_the_record();
        let tag = format!(
            "zed-run-{}-{}",
            self.owner,
            self.next_tag.fetch_add(1, Ordering::SeqCst)
        );
        *running = Some(RunningStatement { tag: tag.clone() });
        Ok(RunningGuard {
            tracker: self,
            arrival,
            tag,
        })
    }

    /// Ends the statements that arrived before now, and says which one is
    /// running, if one is.
    pub fn cancel(&self) -> Option<RunningStatement> {
        let running = self.lock();
        self.generation.fetch_add(1, Ordering::SeqCst);
        running.clone()
    }

    /// The statement running now, without cancelling anything.
    pub fn running(&self) -> Option<RunningStatement> {
        self.lock().clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<RunningStatement>> {
        self.running
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Default for StatementTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl RunningGuard<'_> {
    pub fn tag(&self) -> &str {
        &self.tag
    }

    /// Whether a cancel has come since this statement arrived. A statement
    /// whose connection died after that must not be sent again on a new one.
    pub fn was_cancelled(&self) -> bool {
        self.tracker.generation.load(Ordering::SeqCst) != self.arrival.generation
    }
}

impl Drop for RunningGuard<'_> {
    fn drop(&mut self) {
        let mut running = self.tracker.lock();
        if running
            .as_ref()
            .is_some_and(|statement| statement.tag == self.tag)
        {
            *running = None;
        }
    }
}

/// How long opening the connection that stops a statement, and each question it
/// asks, may take.
pub const CONTROL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// How long the server's list of running statements is searched for the one to
/// stop. A statement is in the list from the moment the server starts it, so a
/// search that finds nothing for this long means it ended, or never got there.
pub const HOW_LONG_TO_LOOK_FOR_THE_STATEMENT: std::time::Duration =
    std::time::Duration::from_secs(3);

pub const BETWEEN_LOOKS: std::time::Duration = std::time::Duration::from_millis(150);

/// The comment that marks a statement for [`StatementTracker`], put in front of
/// its text.
pub fn tag_comment(tag: Option<&str>) -> String {
    tag.map(|tag| format!("/* {tag} */ ")).unwrap_or_default()
}

/// The server's SQLSTATE for a statement that was stopped on request:
/// PostgreSQL's `query_canceled` and `admin_shutdown`, MySQL's
/// `ER_QUERY_INTERRUPTED`.
const INTERRUPTION_SQLSTATES: &[&str] = &["57014", "57P01", "70100"];

/// Whether `error` is what stopping a statement on request looks like, from
/// the call that was running it.
pub fn is_interruption(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        if cause.downcast_ref::<StatementCancelled>().is_some() {
            return true;
        }
        let Some(sqlx::Error::Database(database)) = cause.downcast_ref::<sqlx::Error>() else {
            return false;
        };
        database
            .code()
            .as_deref()
            .is_some_and(|code| INTERRUPTION_SQLSTATES.contains(&code))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_statement_that_arrived_before_a_cancel_never_starts() {
        let tracker = StatementTracker::new();
        let waiting = tracker.arrive();
        assert_eq!(tracker.cancel(), None, "nothing was running yet");
        assert!(tracker.start(waiting).is_err());
    }

    #[test]
    fn a_statement_that_arrives_after_a_cancel_runs() {
        let tracker = StatementTracker::new();
        tracker.cancel();
        let later = tracker.arrive();
        assert!(tracker.start(later).is_ok());
    }

    #[test]
    fn the_running_statement_is_named_for_the_server_and_forgotten_when_it_ends() {
        let tracker = StatementTracker::new();
        let guard = tracker.start(tracker.arrive()).expect("it starts");
        let tag = guard.tag().to_string();
        assert!(tag.starts_with("zed-run-"));
        assert_eq!(tracker.running(), Some(RunningStatement { tag }));
        drop(guard);
        assert_eq!(tracker.running(), None);
    }

    #[test]
    fn a_cancel_names_the_statement_it_stops_and_marks_it_cancelled() {
        let tracker = StatementTracker::new();
        let guard = tracker.start(tracker.arrive()).expect("it starts");
        assert!(!guard.was_cancelled());
        let stopped = tracker.cancel().expect("it was running");
        assert_eq!(stopped.tag, guard.tag());
        assert!(guard.was_cancelled(), "its connection must not be retried");
    }

    #[test]
    fn two_statements_never_share_a_tag() {
        let tracker = StatementTracker::new();
        let first = tracker
            .start(tracker.arrive())
            .expect("it starts")
            .tag()
            .to_string();
        let second = tracker
            .start(tracker.arrive())
            .expect("it starts")
            .tag()
            .to_string();
        assert_ne!(first, second);
    }

    #[test]
    fn a_finished_statement_does_not_clear_the_one_that_followed() {
        let tracker = StatementTracker::new();
        let first = tracker.start(tracker.arrive()).expect("it starts");
        let second = tracker.start(tracker.arrive()).expect("it starts");
        drop(first);
        assert_eq!(
            tracker.running().map(|statement| statement.tag),
            Some(second.tag().to_string())
        );
    }

    #[test]
    fn a_cancel_that_comes_between_the_check_and_the_record_still_sees_the_statement() {
        let tracker = std::sync::Arc::new(StatementTracker::new());
        let arrival = tracker.arrive();
        let canceller = std::cell::RefCell::new(None);
        let guard = tracker
            .start_with(arrival, || {
                let tracker = tracker.clone();
                *canceller.borrow_mut() = Some(std::thread::spawn(move || tracker.cancel()));
                // Long enough for the cancel to finish if nothing stops it.
                std::thread::sleep(std::time::Duration::from_millis(200));
            })
            .expect("it started before any cancel");
        let named = canceller
            .borrow_mut()
            .take()
            .expect("the cancel was sent")
            .join()
            .expect("the cancel ran");
        assert_eq!(
            named.map(|statement| statement.tag),
            Some(guard.tag().to_string()),
            "the cancel missed a statement that started"
        );
        assert!(guard.was_cancelled());
    }

    #[test]
    fn the_tag_goes_in_front_as_a_comment_and_nothing_without_one() {
        assert_eq!(tag_comment(Some("zed-run-1")), "/* zed-run-1 */ ");
        assert_eq!(tag_comment(None), "");
    }

    #[test]
    fn a_cancelled_statement_reads_as_an_interruption() {
        assert!(is_interruption(&anyhow::Error::new(StatementCancelled)));
        assert!(is_interruption(
            &anyhow::Error::new(StatementCancelled).context("Query execution failed")
        ));
        assert!(!is_interruption(&anyhow::anyhow!("syntax error")));
    }
}
