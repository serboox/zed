use std::sync::{Arc, Weak};
use std::time::Instant;

use collections::HashMap;
use gpui::{App, Global, SharedString};

/// One piece of work the editor is doing that a reader may be waiting on.
pub struct RunningWork {
    /// What it is, in the reader's terms: "Importing people.csv", not
    /// "ImportTask".
    pub name: SharedString,
    /// How far it has got, where that is known. Nothing where the work has no
    /// countable end -- a scan finds out how much there is as it goes, and a
    /// bar that guesses is worse than no bar.
    pub how_far: Option<f32>,
    pub started: Instant,
    /// Asks the work to stop, where stopping is a real thing it can do.
    ///
    /// `None` is the honest answer for work that cannot be stopped, and it is
    /// why this is an `Option` rather than a closure that shrugs: loading the
    /// editor's own modules cannot be abandoned half way, and a cross beside
    /// it that did nothing would be worse than no cross at all.
    pub stop: Option<Arc<dyn Fn(&mut App) + Send + Sync>>,
    /// Dead once the registration is dropped, which is how work that ended
    /// without anyone saying so leaves the list.
    alive: Weak<()>,
}

/// What the editor is doing right now, so that one panel can show all of it.
///
/// Registered by whoever starts the work, because only they know what it is
/// called and whether it can be stopped. The panel reads; it never guesses.
#[derive(Default)]
pub struct RunningWorkRegistry {
    work: HashMap<usize, RunningWork>,
    next: usize,
}

/// A registration that ends when it is dropped.
///
/// Held by whoever started the work, so that work which ends -- normally, by
/// failing, or by having its task dropped mid-flight -- leaves the list with
/// nobody having to remember to say so. A panel listing work that finished ten
/// minutes ago is worse than a panel listing nothing.
pub struct RunningWorkHandle {
    id: usize,
    /// Never read, and that is the point: the registry holds a weak reference
    /// to it, so this being dropped is what ends the registration.
    _alive: Arc<()>,
}

impl RunningWorkRegistry {
    fn global(cx: &mut App) -> &mut Self {
        cx.default_global::<Self>()
    }

    /// Registers work, and hands back the registration that ends it.
    pub fn add(
        cx: &mut App,
        name: SharedString,
        stop: Option<Arc<dyn Fn(&mut App) + Send + Sync>>,
    ) -> RunningWorkHandle {
        let alive = Arc::new(());
        let registry = Self::global(cx);
        let id = registry.next;
        registry.next += 1;
        registry.work.insert(
            id,
            RunningWork {
                name,
                how_far: None,
                started: Instant::now(),
                stop,
                alive: Arc::downgrade(&alive),
            },
        );
        cx.refresh_windows();
        RunningWorkHandle { id, _alive: alive }
    }

    /// Everything still running, oldest first, so a list of it does not
    /// reorder itself under the reader's pointer as progress changes.
    ///
    /// Work whose registration has been dropped is left out here rather than
    /// removed, because reading must not need `&mut App` -- a panel reads this
    /// while drawing. It is removed on the next registration or stop.
    pub fn all(cx: &App) -> Vec<(usize, &RunningWork)> {
        let Some(registry) = cx.try_global::<Self>() else {
            return Vec::new();
        };
        let mut listed: Vec<(usize, &RunningWork)> = registry
            .work
            .iter()
            .filter(|(_, work)| work.alive.strong_count() > 0)
            .map(|(id, work)| (*id, work))
            .collect();
        listed.sort_by_key(|(id, work)| (work.started, *id));
        listed
    }

    /// Asks one piece of work to stop. Does nothing where it cannot be
    /// stopped, or where it has already ended -- both of which a reader can
    /// reach by clicking a cross a moment too late.
    pub fn stop(cx: &mut App, id: usize) {
        let stop = cx
            .try_global::<Self>()
            .and_then(|registry| registry.work.get(&id))
            .and_then(|work| work.stop.clone());
        Self::global(cx).forget_what_ended();
        if let Some(stop) = stop {
            stop(cx);
        }
    }

    /// How far along everything is, as one number between zero and one.
    ///
    /// Work with no countable end is left out of the average rather than
    /// counted as nothing: a scan that cannot say how much is left would
    /// otherwise hold the number down for as long as it ran, which reads as no
    /// progress at all.
    pub fn how_far_along(cx: &App) -> Option<f32> {
        let known: Vec<f32> = Self::all(cx)
            .iter()
            .filter_map(|(_, work)| work.how_far)
            .collect();
        if known.is_empty() {
            return None;
        }
        Some(known.iter().sum::<f32>() / known.len() as f32)
    }

    fn forget_what_ended(&mut self) {
        self.work.retain(|_, work| work.alive.strong_count() > 0);
    }
}

impl Global for RunningWorkRegistry {}

impl RunningWorkHandle {
    /// Reports progress, for work that can say how far it has got.
    pub fn how_far(&self, how_far: f32, cx: &mut App) {
        if let Some(work) = RunningWorkRegistry::global(cx).work.get_mut(&self.id) {
            work.how_far = Some(how_far.clamp(0.0, 1.0));
        }
        cx.refresh_windows();
    }
}
