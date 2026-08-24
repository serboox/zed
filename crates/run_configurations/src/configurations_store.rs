use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow};
use futures::StreamExt as _;
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, Global, Subscription, Task, WeakEntity,
};
use project::Project;
use serde_json::Value;
use settings::{Settings, SettingsStore};

use crate::configurations_file::{self, Configuration, FileContents, Kind};
use crate::run_configurations_settings::RunConfigurationsSettings;
use task::TaskTemplate;

/// Said whenever the files have been read again, so a view can keep up with what
/// somebody typed into them by hand.
pub struct ConfigurationsChanged;

/// One store per project, so the window that lists configurations, the windows
/// that run them and the switcher in the toolbar all read the same files -- read
/// once, and warm by the time anything asks. A store made where it is needed is
/// still empty when the surface it feeds is drawn.
/// Held against a weak handle to the project, so a project that is closed takes
/// its store -- and the two file watchers inside it -- away with it.
struct StoreForProject(Vec<(WeakEntity<Project>, Entity<ConfigurationsStore>)>);

impl Global for StoreForProject {}

pub fn store_for(project: &Entity<Project>, cx: &mut App) -> Entity<ConfigurationsStore> {
    let mut kept = match cx.has_global::<StoreForProject>() {
        true => cx.remove_global::<StoreForProject>(),
        false => StoreForProject(Vec::new()),
    };
    kept.0.retain(|(theirs, _)| theirs.upgrade().is_some());
    let found = kept.0.iter().find_map(|(theirs, store)| {
        (theirs.entity_id() == project.entity_id()).then(|| store.clone())
    });
    let store = match found {
        Some(store) => store,
        None => {
            let store = cx.new(|cx| ConfigurationsStore::new(project, cx));
            kept.0.push((project.downgrade(), store.clone()));
            store
        }
    };
    cx.set_global(kept);
    store
}

/// The project's run configurations, as its two files hold them.
///
/// The files are the truth: this reads them, writes back into them, and watches
/// them, so a configuration clicked together in the editor and one typed into the
/// file are the same thing.
pub struct ConfigurationsStore {
    project_root: Option<PathBuf>,
    fs: Arc<dyn fs::Fs>,
    tasks: FileContents,
    scenarios: FileContents,
    /// The ways that were run without ever being written down -- from the gutter,
    /// or from the window that lists them -- newest first. They live as long as the
    /// window does and never reach the project's files, so a one-off run leaves
    /// nothing behind for anybody else to read. Pinning one puts it in the file.
    temporary: Vec<TaskTemplate>,
    _watching: Vec<Task<()>>,
    /// Held so the store keeps noticing a lowered limit for as long as it
    /// lives itself, rather than only until whoever first ran something in
    /// it goes away.
    _settings_subscription: Subscription,
}

/// How many ways run on the spot are worth remembering when the reader has
/// not set `most_temporaries_kept` in their own settings. Older ones fall off
/// the end either way: this is a handful of recent runs, not a history.
pub const MOST_TEMPORARIES_KEPT: usize = 5;

impl EventEmitter<ConfigurationsChanged> for ConfigurationsStore {}

impl ConfigurationsStore {
    pub fn new(project: &Entity<Project>, cx: &mut Context<Self>) -> Self {
        let fs = project.read(cx).fs().clone();
        let project_root = project
            .read(cx)
            .visible_worktrees(cx)
            .next()
            .map(|worktree| worktree.read(cx).abs_path().to_path_buf());

        let mut store = Self::bare(project_root, fs, cx);
        store.watch_the_files(cx);
        store
    }

    /// A store with nothing read from its own files yet, wired up to react at
    /// once if the reader lowers how many ways run on the spot are worth
    /// keeping while some are already remembered.
    fn bare(project_root: Option<PathBuf>, fs: Arc<dyn fs::Fs>, cx: &mut Context<Self>) -> Self {
        let settings_subscription = cx.observe_global::<SettingsStore>(|store, cx| {
            store.forget_past_the_limit(cx);
        });
        Self {
            project_root,
            fs,
            tasks: FileContents::default(),
            scenarios: FileContents::default(),
            temporary: Vec::new(),
            _watching: Vec::new(),
            _settings_subscription: settings_subscription,
        }
    }

    /// Follows both files. Their contents arrive here whenever they change on
    /// disk, whoever changed them -- the editor's own form, or the reader's hands.
    fn watch_the_files(&mut self, cx: &mut Context<Self>) {
        let Some(project_root) = self.project_root.clone() else {
            return;
        };
        for kind in [Kind::Task, Kind::Debug] {
            let path = configurations_file::file_path(&project_root, kind);
            let (mut contents, watching) = settings::watch_config_file(
                cx.background_executor(),
                self.fs.clone(),
                path.clone(),
            );
            // The watcher itself is held so it lives as long as this store.
            self._watching.push(watching);
            self._watching.push(cx.spawn(async move |store, cx| {
                while let Some(text) = contents.next().await {
                    let read = configurations_file::read(kind, &text);
                    if store
                        .update(cx, |store, cx| {
                            match kind {
                                Kind::Task => store.tasks = read,
                                Kind::Debug => store.scenarios = read,
                            }
                            cx.emit(ConfigurationsChanged);
                            cx.notify();
                        })
                        .is_err()
                    {
                        return;
                    }
                }
            }));
        }
    }

    /// The ways run on the spot, newest first.
    pub fn temporary(&self) -> &[TaskTemplate] {
        &self.temporary
    }

    /// Remembers a way that was run without being written down. The same way run
    /// again moves back to the front rather than being kept twice, and once there
    /// are more than the reader's chosen limit the oldest falls off the end.
    pub fn remember_temporary(&mut self, task: TaskTemplate, cx: &mut Context<Self>) {
        self.temporary
            .retain(|kept| !(kept.label == task.label && kept.command == task.command));
        self.temporary.insert(0, task);
        self.temporary.truncate(Self::most_temporaries_kept(cx));
        cx.emit(ConfigurationsChanged);
        cx.notify();
    }

    /// Drops whatever no longer fits the moment the reader lowers the limit,
    /// rather than leaving the extra ones until the next thing is run on the
    /// spot. A limit of zero empties the list entirely, and does it without
    /// underflowing: `Vec::truncate` is a no-op past the end and a full clear
    /// at zero either way.
    fn forget_past_the_limit(&mut self, cx: &mut Context<Self>) {
        let most_kept = Self::most_temporaries_kept(cx);
        if self.temporary.len() > most_kept {
            self.temporary.truncate(most_kept);
            cx.emit(ConfigurationsChanged);
            cx.notify();
        }
    }

    /// How many ways run on the spot are worth keeping right now, from
    /// settings.
    fn most_temporaries_kept(cx: &App) -> usize {
        RunConfigurationsSettings::get_global(cx).most_temporaries_kept
    }

    /// Takes a way run on the spot out of the list without keeping it.
    pub fn forget_temporary(&mut self, at: usize, cx: &mut Context<Self>) {
        if at >= self.temporary.len() {
            return;
        }
        self.temporary.remove(at);
        cx.emit(ConfigurationsChanged);
        cx.notify();
    }

    /// Writes a way run on the spot into the project's own file, where everybody
    /// else reads it, and stops holding it as a temporary one.
    pub fn pin_temporary(&mut self, at: usize, cx: &mut Context<Self>) -> Task<Result<()>> {
        let Some(task) = self.temporary.get(at).cloned() else {
            return Task::ready(Err(anyhow!("that way is no longer in the list")));
        };
        let entry = match configurations_file::task_as_written(&task) {
            Ok(entry) => entry,
            Err(error) => return Task::ready(Err(error)),
        };
        let writing = self.save(Kind::Task, None, entry, cx);
        self.temporary.remove(at);
        cx.emit(ConfigurationsChanged);
        cx.notify();
        writing
    }

    /// A store with no project behind it, for testing the list of ways run on the
    /// spot -- which is held in memory and has nothing to do with the files.
    #[cfg(test)]
    fn empty_for_test(fs: Arc<dyn fs::Fs>, cx: &mut Context<Self>) -> Self {
        Self::bare(None, fs, cx)
    }

    pub fn project_root(&self) -> Option<&PathBuf> {
        self.project_root.as_ref()
    }

    pub fn file_path(&self, kind: Kind) -> Option<PathBuf> {
        self.project_root
            .as_ref()
            .map(|root| configurations_file::file_path(root, kind))
    }

    pub fn of_kind(&self, kind: Kind) -> &FileContents {
        match kind {
            Kind::Task => &self.tasks,
            Kind::Debug => &self.scenarios,
        }
    }

    /// Everything both files hold, tasks first, in the order they are written.
    pub fn all(&self) -> impl Iterator<Item = &Configuration> {
        self.tasks
            .configurations
            .iter()
            .chain(self.scenarios.configurations.iter())
    }

    pub fn get(&self, kind: Kind, at: usize) -> Option<&Configuration> {
        self.of_kind(kind).configurations.get(at)
    }

    /// Puts `entry` in the file, replacing what `replacing` was read as, or adding
    /// it to the end when there is nothing to replace.
    ///
    /// The file is read again immediately before writing, so an edit does not undo
    /// whatever else was typed into the file in the meantime -- and the entry to
    /// replace is looked for by what it says, since the reader may have moved it
    /// since the view read it.
    pub fn save(
        &self,
        kind: Kind,
        replacing: Option<(usize, Value)>,
        entry: Value,
        cx: &App,
    ) -> Task<Result<()>> {
        let Some(path) = self.file_path(kind) else {
            return Task::ready(Err(anyhow!(
                "this project has nowhere to keep its configurations: it has no folder open"
            )));
        };
        let fs = self.fs.clone();
        let empty = kind.empty_file().to_string();
        cx.background_spawn(async move {
            let text = fs.load(&path).await.unwrap_or(empty);
            let at = match replacing {
                Some((at, original)) => Some(place_of(&text, at, &original, &path)?),
                None => None,
            };
            let written = configurations_file::text_with(&text, at, &entry);
            configurations_file::write(&fs, &path, &written).await
        })
    }

    /// Moves the configuration read as `original` one place earlier or later in
    /// its file, which is the order everything that lists them shows.
    ///
    /// The file is read again first, and the entry is found by what it says
    /// rather than by where it was: the reader may have moved it by hand since
    /// this was drawn, and writing by the old index would move somebody else.
    pub fn move_it(
        &self,
        kind: Kind,
        at: usize,
        original: Value,
        later: bool,
        cx: &App,
    ) -> Task<Result<()>> {
        let Some(path) = self.file_path(kind) else {
            return Task::ready(Err(anyhow!("there is no file to move it in")));
        };
        let fs = self.fs.clone();
        cx.background_spawn(async move {
            let text = fs.load(&path).await.unwrap_or_default();
            let at = place_of(&text, at, &original, &path)?;
            let read = configurations_file::read(kind, &text);
            let to = match later {
                true => at + 1,
                false => at.checked_sub(1).context("it is already the first one")?,
            };
            let theirs = read
                .configurations
                .get(to)
                .context("it is already the last one")?
                .as_written
                .clone();
            // Written as a swap of the two entries rather than as a removal and an
            // insertion: the file's own text is edited in place, so everything
            // around the two -- comments, spacing, whatever else the reader put
            // there -- is left exactly as it was.
            let text = configurations_file::text_with(&text, Some(at), &theirs);
            let text = configurations_file::text_with(&text, Some(to), &original);
            configurations_file::write(&fs, &path, &text).await
        })
    }

    /// Takes the configuration that was read as `original` out of its file.
    pub fn remove(&self, kind: Kind, at: usize, original: Value, cx: &App) -> Task<Result<()>> {
        let Some(path) = self.file_path(kind) else {
            return Task::ready(Err(anyhow!("there is no file to take it out of")));
        };
        let fs = self.fs.clone();
        cx.background_spawn(async move {
            let text = fs.load(&path).await.unwrap_or_default();
            let at = place_of(&text, at, &original, &path)?;
            let written = configurations_file::text_without(&text, at);
            configurations_file::write(&fs, &path, &written).await
        })
    }
}

/// Where the configuration read as `original` is in `text` now. Nothing is written
/// when it cannot be found: the file has been changed by hand since it was read,
/// and writing by the old index would go over another configuration.
fn place_of(text: &str, at: usize, original: &Value, path: &std::path::Path) -> Result<usize> {
    configurations_file::place_of(text, at, original).with_context(|| {
        format!(
            "this configuration is no longer in {}, which was changed by hand. Nothing was written.",
            path.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, UpdateGlobal as _};

    fn a_store(cx: &mut TestAppContext) -> Entity<ConfigurationsStore> {
        let fs = fs::FakeFs::new(cx.executor());
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
        cx.new(|cx| ConfigurationsStore::empty_for_test(fs, cx))
    }

    fn a_way(label: &str) -> TaskTemplate {
        TaskTemplate {
            label: label.to_string(),
            command: "go".to_string(),
            args: vec!["run".to_string(), ".".to_string()],
            ..TaskTemplate::default()
        }
    }

    /// Sets how many ways run on the spot are worth keeping, as if the
    /// reader had written it into their own settings.
    fn set_most_temporaries_kept(cx: &mut TestAppContext, most_kept: usize) {
        cx.update(|cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |content| {
                    content.run_configurations = Some(settings::RunConfigurationsSettingsContent {
                        most_temporaries_kept: Some(most_kept),
                        ..Default::default()
                    });
                });
            });
        });
        cx.executor().run_until_parked();
    }

    /// A handful of recent runs, not a history: the newest is at the front and the
    /// oldest falls off the end.
    #[gpui::test]
    fn only_a_handful_of_ways_run_on_the_spot_are_remembered(cx: &mut TestAppContext) {
        let store = a_store(cx);
        store.update(cx, |store, cx| {
            for at in 0..MOST_TEMPORARIES_KEPT + 2 {
                store.remember_temporary(a_way(&format!("run {at}")), cx);
            }
            let kept: Vec<&str> = store
                .temporary()
                .iter()
                .map(|task| task.label.as_str())
                .collect();
            assert_eq!(
                kept,
                vec!["run 6", "run 5", "run 4", "run 3", "run 2"],
                "the newest first, and the two oldest gone"
            );
        });
    }

    /// The same way run again is one entry, moved back to the front.
    #[gpui::test]
    fn the_same_way_run_again_is_remembered_once(cx: &mut TestAppContext) {
        let store = a_store(cx);
        store.update(cx, |store, cx| {
            store.remember_temporary(a_way("first"), cx);
            store.remember_temporary(a_way("second"), cx);
            store.remember_temporary(a_way("first"), cx);
            let kept: Vec<&str> = store
                .temporary()
                .iter()
                .map(|task| task.label.as_str())
                .collect();
            assert_eq!(kept, vec!["first", "second"]);
        });
    }

    /// Forgetting one takes it out and leaves the rest where they were.
    #[gpui::test]
    fn a_way_can_be_forgotten(cx: &mut TestAppContext) {
        let store = a_store(cx);
        store.update(cx, |store, cx| {
            store.remember_temporary(a_way("first"), cx);
            store.remember_temporary(a_way("second"), cx);
            store.forget_temporary(0, cx);
            assert_eq!(store.temporary().len(), 1);
            assert_eq!(store.temporary()[0].label, "first");
            store.forget_temporary(7, cx);
            assert_eq!(
                store.temporary().len(),
                1,
                "an index past the end does nothing"
            );
        });
    }

    /// Left unset, the limit is still the same five it always was.
    #[gpui::test]
    fn nobody_setting_a_limit_still_means_five(cx: &mut TestAppContext) {
        let store = a_store(cx);
        store.update(cx, |store, cx| {
            for at in 0..MOST_TEMPORARIES_KEPT + 2 {
                store.remember_temporary(a_way(&format!("run {at}")), cx);
            }
            assert_eq!(
                store.temporary().len(),
                MOST_TEMPORARIES_KEPT,
                "nobody chose a limit, so the old default of five still applies"
            );
        });
    }

    /// A reader who wants more history than the default should get to keep it.
    #[gpui::test]
    fn a_higher_limit_keeps_more(cx: &mut TestAppContext) {
        let store = a_store(cx);
        set_most_temporaries_kept(cx, MOST_TEMPORARIES_KEPT + 3);
        store.update(cx, |store, cx| {
            for at in 0..MOST_TEMPORARIES_KEPT + 3 {
                store.remember_temporary(a_way(&format!("run {at}")), cx);
            }
            assert_eq!(
                store.temporary().len(),
                MOST_TEMPORARIES_KEPT + 3,
                "a raised limit keeps everything run so far"
            );
        });
    }

    /// Lowering the limit while more are already remembered drops the extra
    /// ones at once -- not the next time something is run on the spot.
    #[gpui::test]
    fn lowering_the_limit_drops_the_extra_at_once(cx: &mut TestAppContext) {
        let store = a_store(cx);
        store.update(cx, |store, cx| {
            for at in 0..MOST_TEMPORARIES_KEPT {
                store.remember_temporary(a_way(&format!("run {at}")), cx);
            }
        });
        set_most_temporaries_kept(cx, 2);
        store.update(cx, |store, _cx| {
            assert_eq!(
                store.temporary().len(),
                2,
                "the extra ones are gone without anything else being run"
            );
        });
    }

    /// A limit of zero means none of them are worth keeping, and running
    /// something on the spot while it is zero does not panic.
    #[gpui::test]
    fn a_limit_of_zero_remembers_nothing(cx: &mut TestAppContext) {
        let store = a_store(cx);
        set_most_temporaries_kept(cx, 0);
        store.update(cx, |store, cx| {
            store.remember_temporary(a_way("first"), cx);
            store.remember_temporary(a_way("second"), cx);
            assert_eq!(
                store.temporary().len(),
                0,
                "a limit of zero keeps none of them"
            );
        });
    }

    /// Dropping the limit to zero after some were already remembered clears
    /// them at once too, and does not panic doing it.
    #[gpui::test]
    fn a_limit_lowered_to_zero_clears_what_was_kept(cx: &mut TestAppContext) {
        let store = a_store(cx);
        store.update(cx, |store, cx| {
            store.remember_temporary(a_way("first"), cx);
            store.remember_temporary(a_way("second"), cx);
        });
        set_most_temporaries_kept(cx, 0);
        store.update(cx, |store, _cx| {
            assert_eq!(store.temporary().len(), 0);
        });
    }
}
