use std::time::{Duration, Instant};

/// Hands a pointer position on at most once in an interval, and never loses the
/// last one.
///
/// Every move of the pointer the page is told about costs the engine a search of
/// the whole document for what is under it, which on a long page takes tens of
/// milliseconds. A pointer reports itself far more often than that, so the page
/// is told where it is at a pace it can keep, and where it stopped is always
/// what it ends up told.
pub(crate) struct PointerPace<Position> {
    interval: Duration,
    sent_at: Option<Instant>,
    held: Option<Position>,
}

impl<Position: Copy> PointerPace<Position> {
    pub(crate) fn new(interval: Duration) -> Self {
        Self {
            interval,
            sent_at: None,
            held: None,
        }
    }

    /// The pointer is now at `position`. Gives back where to tell the page it is,
    /// if the interval has passed since it was last told.
    pub(crate) fn moved(&mut self, position: Position, now: Instant) -> Option<Position> {
        self.held = Some(position);
        self.due(now)
    }

    /// The position that has been held back, once its time has come.
    pub(crate) fn due(&mut self, now: Instant) -> Option<Position> {
        let due = self
            .sent_at
            .is_none_or(|sent_at| now.saturating_duration_since(sent_at) >= self.interval);
        if !due {
            return None;
        }
        let position = self.held.take()?;
        self.sent_at = Some(now);
        Some(position)
    }

    /// The held position, at once: for a click, which has to land where the
    /// pointer is and not where the page last heard of it.
    pub(crate) fn flush(&mut self, now: Instant) -> Option<Position> {
        let position = self.held.take()?;
        self.sent_at = Some(now);
        Some(position)
    }

    /// How long until the held position is due, or `None` when nothing is held.
    pub(crate) fn due_in(&self, now: Instant) -> Option<Duration> {
        self.held?;
        let sent_at = self.sent_at?;
        Some(
            self.interval
                .saturating_sub(now.saturating_duration_since(sent_at)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INTERVAL: Duration = Duration::from_millis(40);

    fn at(start: Instant, milliseconds: u64) -> Instant {
        start + Duration::from_millis(milliseconds)
    }

    #[test]
    fn the_first_position_goes_straight_through() {
        let start = Instant::now();
        let mut pace = PointerPace::new(INTERVAL);
        assert_eq!(pace.moved(1, start), Some(1));
        assert_eq!(pace.due_in(start), None);
    }

    #[test]
    fn positions_inside_the_interval_are_held_and_only_the_last_is_kept() {
        let start = Instant::now();
        let mut pace = PointerPace::new(INTERVAL);
        assert_eq!(pace.moved(1, start), Some(1));
        assert_eq!(pace.moved(2, at(start, 5)), None);
        assert_eq!(pace.moved(3, at(start, 10)), None);
        assert_eq!(pace.moved(4, at(start, 15)), None);
        assert_eq!(pace.due(at(start, 39)), None);
        assert_eq!(pace.due(at(start, 40)), Some(4));
        assert_eq!(
            pace.due(at(start, 80)),
            None,
            "a position is handed out once"
        );
    }

    #[test]
    fn a_position_after_the_interval_goes_straight_through() {
        let start = Instant::now();
        let mut pace = PointerPace::new(INTERVAL);
        assert_eq!(pace.moved(1, start), Some(1));
        assert_eq!(pace.moved(2, at(start, 60)), Some(2));
    }

    #[test]
    fn a_pointer_that_stops_is_still_heard_from() {
        let start = Instant::now();
        let mut pace = PointerPace::new(INTERVAL);
        pace.moved(1, start);
        assert_eq!(pace.moved(9, at(start, 3)), None);
        assert_eq!(pace.due_in(at(start, 3)), Some(Duration::from_millis(37)));
        assert_eq!(pace.due_in(at(start, 100)), Some(Duration::ZERO));
        assert_eq!(pace.due(at(start, 100)), Some(9));
        assert_eq!(pace.due_in(at(start, 100)), None);
    }

    #[test]
    fn a_click_flushes_what_is_held_and_restarts_the_interval() {
        let start = Instant::now();
        let mut pace = PointerPace::new(INTERVAL);
        pace.moved(1, start);
        pace.moved(2, at(start, 5));
        assert_eq!(pace.flush(at(start, 6)), Some(2));
        assert_eq!(
            pace.flush(at(start, 7)),
            None,
            "nothing is held after a flush"
        );
        assert_eq!(
            pace.moved(3, at(start, 20)),
            None,
            "the interval began at the flush"
        );
        assert_eq!(pace.due(at(start, 46)), Some(3));
    }

    #[test]
    fn a_hundred_and_fifty_moves_in_a_second_reach_the_page_about_twenty_five_times() {
        let start = Instant::now();
        let mut pace = PointerPace::new(INTERVAL);
        let mut told = Vec::new();
        for step in 0..150_u64 {
            let now = at(start, step * 1000 / 150);
            if let Some(position) = pace.moved(step, now) {
                told.push(position);
            }
        }
        assert!(
            (20..=30).contains(&told.len()),
            "{} positions were told to the page",
            told.len()
        );
        let last = pace.due(at(start, 2000));
        assert_eq!(last.or(told.last().copied()), Some(149));
    }
}
