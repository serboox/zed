use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow};
use futures::StreamExt as _;
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Task};
use project::Project;
use serde_json::Value;

use crate::configurations_file::{self, Configuration, FileContents, Kind};

/// Said whenever the files have been read again, so a view can keep up with what
/// somebody typed into them by hand.
pub struct ConfigurationsChanged;

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
    _watching: Vec<Task<()>>,
}

impl EventEmitter<ConfigurationsChanged> for ConfigurationsStore {}

impl ConfigurationsStore {
    pub fn new(project: &Entity<Project>, cx: &mut Context<Self>) -> Self {
        let fs = project.read(cx).fs().clone();
        let project_root = project
            .read(cx)
            .visible_worktrees(cx)
            .next()
            .map(|worktree| worktree.read(cx).abs_path().to_path_buf());

        let mut store = Self {
            project_root,
            fs,
            tasks: FileContents::default(),
            scenarios: FileContents::default(),
            _watching: Vec::new(),
        };
        store.watch_the_files(cx);
        store
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
