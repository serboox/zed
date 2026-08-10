use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Names the moments a launch passes through, so that "the editor takes a while
/// to open" can be answered with a number per phase rather than a guess. Off
/// unless `ZED_STARTUP_TIMING` is set, since the point is to measure a launch,
/// not to slow every launch down by measuring it.
///
/// The editor's own log carries whole seconds only, which is too coarse to tell
/// a slow phase from a fast one, and its frame profiler starts collecting after
/// the window exists -- by which time everything measured here has happened.
static MARKS: OnceLock<Mutex<Vec<(&'static str, Duration)>>> = OnceLock::new();

fn wanted() -> bool {
    static WANTED: OnceLock<bool> = OnceLock::new();
    *WANTED.get_or_init(|| std::env::var_os("ZED_STARTUP_TIMING").is_some())
}

/// Records that the launch has reached `what`, measured from process start.
pub fn mark(what: &'static str, since_start: Duration) {
    if !wanted() {
        return;
    }
    if let Ok(mut marks) = MARKS.get_or_init(|| Mutex::new(Vec::new())).lock() {
        marks.push((what, since_start));
    }
}

/// Records that the launch has reached `what`, given the instant the process
/// started.
pub fn mark_since(what: &'static str, start: Instant) {
    if !wanted() {
        return;
    }
    mark(what, start.elapsed());
}

/// Writes what was recorded, one phase per line, with the time each phase took
/// and the time reached at its end. Safe to call more than once; a launch that
/// recorded nothing writes nothing.
pub fn report() {
    if !wanted() {
        return;
    }
    let Some(marks) = MARKS.get() else { return };
    let Ok(marks) = marks.lock() else { return };
    if marks.is_empty() {
        return;
    }
    let mut previous = Duration::ZERO;
    log::info!("startup timing, milliseconds from process start:");
    for (what, reached) in marks.iter() {
        let took = reached.saturating_sub(previous);
        log::info!(
            "  {:>7.1} ms  +{:>7.1} ms  {what}",
            reached.as_secs_f64() * 1000.,
            took.as_secs_f64() * 1000.,
        );
        previous = *reached;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_are_dropped_unless_the_launch_asked_to_be_measured() {
        // The variable is read once per process and this test runs without it,
        // so nothing should be recorded however many marks are made.
        mark("a phase nobody asked about", Duration::from_millis(5));
        assert!(
            MARKS
                .get()
                .is_none_or(|marks| marks.lock().map(|marks| marks.is_empty()).unwrap_or(true)),
            "an unmeasured launch must not carry a list of marks around"
        );
    }
}
