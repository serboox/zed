use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Set to anything to have each frame the page shows checked for being wholly
/// white, and the ones that are written to the log.
const PROBE: &str = "ZED_HTML_FLASH_PROBE";
/// Set to a number of wheel ticks to have the page scroll itself that many times,
/// sixty a second, once it has loaded, and to report how it went. This is how a
/// flash is looked for without a hand on the mouse.
const SELFTEST: &str = "ZED_HTML_FLASH_SELFTEST";

const TICK: Duration = Duration::from_millis(16);
const SETTLE: Duration = Duration::from_secs(2);

/// Looks for the preview going blank while it scrolls: a frame the page shows
/// that is white from edge to edge, which a page of any content does not paint.
pub(crate) struct FlashProbe {
    /// Told to the thread that wakes the page up, so that it stops with the page.
    stopped: Arc<AtomicBool>,
    ticks_left: Cell<u32>,
    selftest: bool,
    settled_at: Cell<Option<Instant>>,
    next_tick: Cell<Instant>,
    sealed_at: Cell<Option<Instant>>,
    last_shown: Cell<Option<Instant>>,
    frames: Cell<u64>,
    blank_frames: Cell<u64>,
    longest_gap: Cell<Duration>,
    longest_card_wait: Cell<Duration>,
    reported: Cell<bool>,
}

impl FlashProbe {
    pub(crate) fn from_environment() -> Option<Self> {
        let ticks = std::env::var(SELFTEST)
            .ok()
            .and_then(|ticks| ticks.parse::<u32>().ok());
        if ticks.is_none() && std::env::var_os(PROBE).is_none() {
            return None;
        }
        Some(Self {
            stopped: Arc::new(AtomicBool::new(false)),
            ticks_left: Cell::new(ticks.unwrap_or(0)),
            selftest: ticks.is_some(),
            settled_at: Cell::new(None),
            next_tick: Cell::new(Instant::now()),
            sealed_at: Cell::new(None),
            last_shown: Cell::new(None),
            frames: Cell::new(0),
            blank_frames: Cell::new(0),
            longest_gap: Cell::new(Duration::ZERO),
            longest_card_wait: Cell::new(Duration::ZERO),
            reported: Cell::new(false),
        })
    }

    /// How many ticks a self-scrolling page is going to ask for, so the thread
    /// that wakes it up knows how long to go on.
    pub(crate) fn ticks_asked_for(&self) -> u32 {
        self.ticks_left.get()
    }

    pub(crate) fn stop_flag(&self) -> Arc<AtomicBool> {
        self.stopped.clone()
    }

    pub(crate) fn scrolls_itself(&self) -> bool {
        self.selftest
    }

    /// Whether a wheel tick is due now. The first is held back until the page
    /// has been loaded for a moment, so that what is measured is scrolling and
    /// not a page still arriving.
    pub(crate) fn tick_is_due(&self, loading: bool) -> bool {
        if !self.selftest || self.ticks_left.get() == 0 || loading {
            return false;
        }
        let now = Instant::now();
        let settled_at = match self.settled_at.get() {
            Some(settled_at) => settled_at,
            None => {
                self.settled_at.set(Some(now + SETTLE));
                return false;
            }
        };
        if now < settled_at || now < self.next_tick.get() {
            return false;
        }
        self.next_tick.set(now + TICK);
        self.ticks_left.set(self.ticks_left.get() - 1);
        true
    }

    pub(crate) fn painted(&self) {
        self.sealed_at.set(Some(Instant::now()));
    }

    /// A frame has been handed to the window. `blank` says whether every pixel
    /// sampled from it was white.
    pub(crate) fn shown(&self, blank: bool, shared: bool) {
        let now = Instant::now();
        self.frames.set(self.frames.get() + 1);
        let card_wait = self
            .sealed_at
            .take()
            .map(|sealed_at| now - sealed_at)
            .unwrap_or_default();
        self.longest_card_wait
            .set(self.longest_card_wait.get().max(card_wait));
        let gap = self
            .last_shown
            .replace(Some(now))
            .map(|last| now - last)
            .unwrap_or_default();
        self.longest_gap.set(self.longest_gap.get().max(gap));
        if blank {
            self.blank_frames.set(self.blank_frames.get() + 1);
            log::warn!(
                "the page showed a frame that is white from edge to edge: the card took {} ms \
                 over it and {} ms passed since the last frame",
                card_wait.as_millis(),
                gap.as_millis()
            );
        }
        self.report_if_done(shared);
    }

    /// Writes the result once every tick has been sent and the page has had a
    /// second to answer the last one.
    pub(crate) fn report_if_done(&self, shared: bool) {
        if self.selftest
            && self.ticks_left.get() == 0
            && Instant::now() > self.next_tick.get() + Duration::from_secs(1)
            && !self.reported.replace(true)
        {
            log::info!(
                "flash selftest: {} frames, {} white, longest gap {} ms, longest wait for the \
                 card {} ms, frames {}",
                self.frames.get(),
                self.blank_frames.get(),
                self.longest_gap.get().as_millis(),
                self.longest_card_wait.get().as_millis(),
                if shared { "shared" } else { "copied" }
            );
        }
    }
}

impl Drop for FlashProbe {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
    }
}
