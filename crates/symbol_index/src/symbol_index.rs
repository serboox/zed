use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use gpui::{AppContext as _, Context, Entity, Subscription, Task};
use project::{Event as ProjectEvent, PathChange, Project, WorktreeId};
use semantic_index::definitions::Definition;
use semantic_index::inventory::Inventory;
use semantic_index::refresh;
use semantic_index::symbols::{Catalogue, Symbols};
use util::ResultExt as _;

/// Which of four states the index is in, so a caller can tell "nothing
/// matched" from "there is nothing to search yet" apart.
#[derive(Debug, Clone)]
pub enum State {
    /// The project has no local worktree to index, or opening the on-disk
    /// store itself failed before a build could even start.
    NotBuilt,
    /// The first build, or a later whole-project refresh, is running in the
    /// background right now. The catalogue answered from, if any, is the one
    /// left over from before this pass started.
    Building,
    /// Answers are current as of the last pass that finished.
    Ready {
        /// How many symbols the catalogue holds right now.
        symbols: usize,
        /// How many files the *last* pass parsed -- the honest number for
        /// telling a cheap save-triggered refresh from an expensive one.
        files_parsed_last_pass: usize,
    },
    /// The index could not be built at all, and never has been. `reason` is
    /// the error, rendered for a person to read.
    Failed { reason: Arc<str> },
}

/// What one background pass over the project is for.
#[derive(Clone)]
enum Pass {
    /// The very first build: a full walk and parse of every file.
    InitialBuild,
    /// One file just saved.
    OneFile(PathBuf),
    /// Something outside the editor's own knowledge could have changed many
    /// files at once -- a branch switch, an external tool, several saves
    /// arriving faster than the index could keep up with.
    WholeProject,
}

/// One project's symbol index: builds itself once, keeps itself in line with
/// what is saved to disk, and answers name queries from what it has built,
/// without needing a language server running at all.
///
/// This is the wiring the plan's M2 gate is stated against -- that symbol
/// search works with the language server switched off -- not a search
/// surface of its own. A caller reads [`State::state`] to know what an empty
/// answer from [`Self::candidates`] means, and calls [`Self::candidates`] to
/// get one.
pub struct SymbolIndex {
    root: PathBuf,
    worktree_id: Option<WorktreeId>,
    cores: usize,
    state: State,
    catalogue: Option<Catalogue>,
    /// Owned by whichever background pass is presently touching them, and by
    /// nothing else at any other time -- taken out when a pass starts, and
    /// always handed back when it finishes, so two passes can never both hold
    /// a handle to the same files at once.
    stores: Option<(Symbols, Inventory)>,
    /// Set when a refresh was asked for while a pass was already running.
    /// Coalesced rather than queued in detail: whatever it was, a whole-
    /// project refresh is a safe superset of it, and is what runs once the
    /// current pass hands the stores back.
    refresh_pending: bool,
    /// The task presently running a pass, held so that dropping this entity
    /// (the project closing) drops it too, rather than a finished background
    /// pass trying to write its result into an entity that is gone.
    _task: Option<Task<()>>,
    _subscription: Subscription,
}

impl SymbolIndex {
    /// Opens (or creates) `project`'s symbol index and starts the first
    /// build in the background. `project` must already have its worktrees;
    /// this reads them once, here, and never again -- see the doc on
    /// [`Self::handle_project_event`] for why a subscription callback must
    /// not do the same.
    pub fn new(project: Entity<Project>, cx: &mut Context<Self>) -> Self {
        Self::new_with_index_dir(project, index_directory(), cx)
    }

    fn new_with_index_dir(
        project: Entity<Project>,
        index_dir: PathBuf,
        cx: &mut Context<Self>,
    ) -> Self {
        let primary = project
            .read(cx)
            .visible_worktrees(cx)
            .find(|worktree| worktree.read(cx).is_local())
            .map(|worktree| {
                let worktree = worktree.read(cx);
                (worktree.abs_path().to_path_buf(), worktree.id())
            });

        // Every core but one, matching the plan's own convention for the
        // build passes this crate wraps -- see `semantic_index::measure`.
        let cores = cx.background_executor().num_cpus().saturating_sub(1).max(1);
        let subscription = cx.subscribe(&project, Self::handle_project_event);

        let mut this = Self {
            root: PathBuf::new(),
            worktree_id: None,
            cores,
            state: State::NotBuilt,
            catalogue: None,
            stores: None,
            refresh_pending: false,
            _task: None,
            _subscription: subscription,
        };

        let Some((root, worktree_id)) = primary else {
            // Nothing local to index: not an error, just nothing to do. A
            // remote-only or worktree-less project stays `NotBuilt` forever.
            return this;
        };
        this.root = root;
        this.worktree_id = Some(worktree_id);

        match open_stores(&index_dir, &this.root) {
            Ok(stores) => {
                this.stores = Some(stores);
                this.start_pass(Pass::InitialBuild, cx);
            }
            Err(error) => {
                this.state = State::Failed {
                    reason: format!("{error:#}").into(),
                };
            }
        }
        this
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    /// At most `most` definitions whose name contains `query`'s letters in
    /// order, read from whatever the catalogue already holds in memory.
    /// Empty both while there is nothing built yet and when nothing matches;
    /// call [`Self::state`] to tell the two apart.
    pub fn candidates(&self, query: &str, most: usize) -> Vec<Definition> {
        self.catalogue
            .as_ref()
            .map(|catalogue| catalogue.candidates(query, most))
            .unwrap_or_default()
    }

    /// Reacts to what the project already emits, rather than polling.
    ///
    /// `WorktreeUpdatedEntries` is the event that actually fires when a
    /// buffer is saved to disk: the worktree's own file watcher notices the
    /// write and reports it here, together with everything else that can
    /// change a file's contents without the editor being told directly (a
    /// branch switch, an external tool). One real save reports as exactly one
    /// entry in the set; a branch switch reports as many at once, which is
    /// what tells the two apart below.
    ///
    /// This function must never read or update `project` itself: it runs
    /// while `project` is in the middle of emitting the very event this
    /// receives, and touching it again here -- even just reading it -- would
    /// hit the "updating an entity that is already being updated" panic the
    /// task's own warning is about. Everything this needs (`self.root`,
    /// `self.worktree_id`) was captured once, in the constructor, instead.
    fn handle_project_event(
        &mut self,
        _project: Entity<Project>,
        event: &ProjectEvent,
        cx: &mut Context<Self>,
    ) {
        let ProjectEvent::WorktreeUpdatedEntries(worktree_id, changes) = event else {
            return;
        };
        if Some(*worktree_id) != self.worktree_id {
            return;
        }

        let mut real_paths: Vec<PathBuf> = Vec::new();
        let mut any_removed = false;
        for (path, _, change) in changes.iter() {
            match change {
                // Reported only during the worktree's own initial scan, which
                // this entity's own initial build already accounts for.
                PathChange::Loaded => continue,
                PathChange::Removed => any_removed = true,
                PathChange::Added | PathChange::Updated | PathChange::AddedOrUpdated => {}
            }
            real_paths.push(self.root.join(path.as_std_path()));
        }
        if real_paths.is_empty() {
            return;
        }

        // Exactly one file, and nothing removed: cheap enough to be the save
        // this event exists to report, so it gets the one-file pass. Anything
        // broader -- several files at once, or any removal, which the
        // one-file pass has no way to record -- goes through the
        // whole-project pass instead.
        if real_paths.len() == 1 && !any_removed {
            let path = real_paths
                .into_iter()
                .next()
                .expect("checked non-empty above");
            self.start_pass(Pass::OneFile(path), cx);
        } else {
            self.start_pass(Pass::WholeProject, cx);
        }
    }

    /// Starts `pass` in the background, or -- if a pass is already running --
    /// records that another one is owed once this one finishes.
    ///
    /// A pass that is genuinely superseded is not raced against the one
    /// already running: `semantic_index`'s build and refresh passes are
    /// plain blocking calls with no point inside them a cooperative cancel
    /// could land on, so dropping a `Task` that wraps one would not actually
    /// stop the database write already in flight -- only the bookkeeping
    /// that was waiting to hear about it. Queuing instead of racing is what
    /// keeps two passes from ever touching the same store at once, which is
    /// the actual hazard the task's own warning about deadlocking the
    /// database is about.
    fn start_pass(&mut self, pass: Pass, cx: &mut Context<Self>) {
        if self._task.is_some() {
            self.refresh_pending = true;
            return;
        }
        let Some((symbols, inventory)) = self.stores.take() else {
            // Should not happen: `stores` is `None` only while `_task` is
            // `Some`, and that was just ruled out above. Recorded rather than
            // silently dropped, and retried once whatever is holding them
            // finishes.
            log::error!("symbol index: no store was available to start a pass with");
            self.refresh_pending = true;
            return;
        };
        if matches!(pass, Pass::InitialBuild) {
            self.state = State::Building;
        }

        let root = self.root.clone();
        let cores = self.cores;
        let task = cx.spawn(async move |this, cx| {
            let (pass, symbols, inventory, result) = cx
                .background_spawn(async move {
                    let result = run_pass(&pass, &root, cores, &symbols, &inventory);
                    // Handed back rather than copied beforehand: which pass this
                    // was decides what a failure means, and only one of them can
                    // own it.
                    (pass, symbols, inventory, result)
                })
                .await;

            this.update(cx, |this, cx| {
                this.stores = Some((symbols, inventory));
                this._task = None;
                match result {
                    Ok((files_parsed, catalogue)) => {
                        let symbols = catalogue.len();
                        this.catalogue = Some(catalogue);
                        this.state = State::Ready {
                            symbols,
                            files_parsed_last_pass: files_parsed,
                        };
                    }
                    Err(error) => match &pass {
                        // Only the very first build failing leaves the index
                        // with nothing to answer from at all; a later
                        // refresh failing keeps whatever the index already
                        // had, which is still a usable answer.
                        Pass::InitialBuild => {
                            this.state = State::Failed {
                                reason: format!("{error:#}").into(),
                            };
                        }
                        Pass::OneFile(path) => {
                            log::error!("symbol index: refreshing {path:?} failed -- {error:#}");
                        }
                        Pass::WholeProject => {
                            log::error!(
                                "symbol index: a whole-project refresh failed -- {error:#}"
                            );
                        }
                    },
                }
                if this.refresh_pending {
                    this.refresh_pending = false;
                    this.start_pass(Pass::WholeProject, cx);
                }
                cx.notify();
            })
            .log_err();
        });
        self._task = Some(task);
    }
}

/// The directory every project's symbol index file lives under in
/// production: alongside `db.sqlite`, the editor's own shared database, and
/// for the same reason -- both are sqlite-backed state this process owns,
/// not a project's own file. `paths::database_dir()` already keeps the
/// editor's other per-machine databases (see `crates/db/src/db.rs`), and this
/// is one more of those, not a new kind of directory the editor needs to
/// remember to clean up.
fn index_directory() -> PathBuf {
    paths::database_dir().join("symbol_index")
}

/// Where one project's index lives inside `index_dir`: one file, named by a
/// hash of the project's own root path, so two projects can never collide
/// and the same project always finds its own file again. Not a
/// cryptographic hash -- nothing here needs to resist being forged, only to
/// be stable and to not collide in practice -- so the standard library's own
/// hasher is enough, the same one `dev_container` already uses for the same
/// kind of naming problem.
fn index_file_path(index_dir: &Path, root: &Path) -> PathBuf {
    let mut hasher = DefaultHasher::new();
    root.hash(&mut hasher);
    index_dir.join(format!("{:016x}.db", hasher.finish()))
}

/// Opens the symbol store for `root` inside `index_dir`, creating the
/// directory first: `Symbols::open` (through `sqlez::Connection::open_file`)
/// falls back to an in-memory database silently if its parent directory does
/// not exist, which would make the index look persistent while quietly not
/// being -- creating the directory first is what keeps it honestly on disk.
///
/// The inventory that tracks what changed is kept in memory only: it exists
/// to compare "before this pass" to "after it" within one run of the editor,
/// not to remember what a *previous* run looked like, so persisting it would
/// buy nothing without also persisting enough to prove the previous run's
/// idea of the disk is still accurate today.
fn open_stores(index_dir: &Path, root: &Path) -> Result<(Symbols, Inventory)> {
    std::fs::create_dir_all(index_dir).context("creating the symbol index's directory")?;
    let symbols =
        Symbols::open(&index_file_path(index_dir, root)).context("opening the symbol index")?;
    let inventory = Inventory::open_in_memory().context("opening the file inventory")?;
    Ok((symbols, inventory))
}

/// Runs one pass and reloads the catalogue from what it left behind,
/// entirely off the foreground thread. Returns how many files the pass
/// itself parsed, and the catalogue built from everything the store now
/// holds -- not only what this pass touched, since a save only ever changes
/// one file's worth of rows but the catalogue answers for the whole project.
fn run_pass(
    pass: &Pass,
    root: &Path,
    cores: usize,
    symbols: &Symbols,
    inventory: &Inventory,
) -> Result<(usize, Catalogue)> {
    let files_parsed = match pass {
        Pass::InitialBuild => {
            let built = semantic_index::symbols::build(root, cores, symbols)
                .context("building the symbol index")?;
            inventory
                .take_stock(root, cores)
                .context("taking the first stock of the project, after the build")?;
            built.files
        }
        Pass::OneFile(path) => {
            refresh::refresh_one_file(root, path, symbols)
                .context("refreshing one saved file")?
                .files_parsed
        }
        Pass::WholeProject => {
            refresh::refresh(root, cores, inventory, symbols)
                .context("refreshing the whole project")?
                .files_parsed
        }
    };
    let catalogue = Catalogue::read_from(symbols).context("reading the catalogue back")?;
    Ok((files_parsed, catalogue))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use serde_json::json;
    use settings::SettingsStore;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
    }

    /// A project that exists twice over, at one and the same path.
    ///
    /// The editor's own side of it is the deterministic in-memory filesystem
    /// every other test in this repository uses -- a real one spawns threads
    /// and file watchers of its own, which the test scheduler rejects outright
    /// as non-deterministic. The index's side of it is a real directory,
    /// because the index walks and reads the disk with the standard library
    /// rather than through the editor's filesystem abstraction; that is
    /// deliberate, since it has to read a whole project as fast as the
    /// editor's own scanner does.
    ///
    /// Giving both the same absolute path is what lets one test cover the
    /// whole chain: the project reports what changed, and the index reads the
    /// file that really changed.
    async fn a_project(
        files: &[(&str, &str)],
        cx: &mut TestAppContext,
    ) -> (tempfile::TempDir, Entity<Project>) {
        let held = tempfile::tempdir().expect("a directory to put a project in");
        let mut tree = serde_json::Map::new();
        for (name, contents) in files {
            std::fs::write(held.path().join(name), contents).expect("a project file on disk");
            tree.insert(
                (*name).to_string(),
                serde_json::Value::String((*contents).to_string()),
            );
        }
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(held.path(), serde_json::Value::Object(tree))
            .await;
        let project = Project::test(fs, [held.path()], cx).await;
        (held, project)
    }

    /// A fresh, empty directory to point the index's own files at, so tests
    /// never touch the real, shared `paths::database_dir()` on the machine
    /// running them.
    fn a_scratch_index_dir() -> (tempfile::TempDir, PathBuf) {
        let held = tempfile::tempdir().expect("a directory for the index's own files");
        let dir = held.path().join("symbol_index");
        (held, dir)
    }

    fn open_index(
        project: Entity<Project>,
        index_dir: PathBuf,
        cx: &mut TestAppContext,
    ) -> Entity<SymbolIndex> {
        cx.new(|cx| SymbolIndex::new_with_index_dir(project, index_dir, cx))
    }

    #[gpui::test]
    async fn a_freshly_opened_project_builds_and_answers_a_query(cx: &mut TestAppContext) {
        init_test(cx);
        let (_held, project) = a_project(
            &[
                ("one.rs", "pub fn take_stock() {}\n"),
                ("two.rs", "pub fn read_one() {}\n"),
            ],
            cx,
        )
        .await;
        let (_held, index_dir) = a_scratch_index_dir();

        let index = open_index(project, index_dir, cx);
        cx.run_until_parked();

        let state = index.read_with(cx, |index, _| index.state().clone());
        match state {
            State::Ready { symbols, .. } => {
                assert_eq!(symbols, 2, "one function defined in each of two files")
            }
            other => panic!("expected the index to be ready, found {other:?}"),
        }

        let found = index.read_with(cx, |index, _| index.candidates("takestock", 10));
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].name, "take_stock");
    }

    #[gpui::test]
    async fn saving_a_file_finds_its_new_definition_without_reparsing_the_rest(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let (held, project) = a_project(
            &[
                ("one.rs", "pub fn first() {}\n"),
                ("two.rs", "pub fn second() {}\n"),
            ],
            cx,
        )
        .await;
        let (_held, index_dir) = a_scratch_index_dir();
        let index = open_index(project.clone(), index_dir, cx);
        cx.run_until_parked();

        let buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer(held.path().join("one.rs"), cx)
            })
            .await
            .unwrap();
        buffer.update(cx, |buffer, cx| {
            let end = buffer.len();
            buffer.edit([(end..end, "\npub fn added_later() {}\n")], None, cx);
        });
        // The real file has to carry the new text too: saving writes to the
        // project's own filesystem, and the index reads the disk. Written
        // before the save, so the change is already there when the event the
        // save emits sends the index to look.
        std::fs::write(
            held.path().join("one.rs"),
            "pub fn first() {}\n\npub fn added_later() {}\n",
        )
        .expect("the file on disk");
        project
            .update(cx, |project, cx| project.save_buffer(buffer.clone(), cx))
            .await
            .unwrap();
        cx.run_until_parked();

        let state = index.read_with(cx, |index, _| index.state().clone());
        let files_parsed = match state {
            State::Ready {
                files_parsed_last_pass,
                ..
            } => files_parsed_last_pass,
            other => panic!("expected the index to be ready, found {other:?}"),
        };
        assert_eq!(
            files_parsed, 1,
            "only the saved file should have been reparsed"
        );

        let found = index.read_with(cx, |index, _| index.candidates("addedlater", 10));
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].name, "added_later");
    }

    #[gpui::test]
    async fn a_query_before_the_build_finishes_reports_building_not_empty(cx: &mut TestAppContext) {
        init_test(cx);
        let (_held, project) = a_project(&[("one.rs", "pub fn first() {}\n")], cx).await;
        let (_held, index_dir) = a_scratch_index_dir();

        // Deliberately not `run_until_parked`: the point of the test is what
        // a query answers before the background pass has had a chance to run
        // at all.
        let index = open_index(project, index_dir, cx);

        let state = index.read_with(cx, |index, _| index.state().clone());
        assert!(
            matches!(state, State::Building),
            "expected Building, found {state:?}"
        );
        let found = index.read_with(cx, |index, _| index.candidates("first", 10));
        assert!(
            found.is_empty(),
            "an index still building has nothing to answer from yet"
        );
    }

    #[gpui::test]
    async fn two_projects_keep_separate_indexes(cx: &mut TestAppContext) {
        init_test(cx);
        let (_held_a, project_a) = a_project(&[("a.rs", "pub fn only_in_a() {}\n")], cx).await;
        let (_held_b, project_b) = a_project(&[("b.rs", "pub fn only_in_b() {}\n")], cx).await;
        // The same index directory for both: this is exactly the case that
        // would collide if the two projects were keyed the same way.
        let (_held, index_dir) = a_scratch_index_dir();

        let index_a = open_index(project_a, index_dir.clone(), cx);
        let index_b = open_index(project_b, index_dir, cx);
        cx.run_until_parked();

        let leaked_into_a = index_a.read_with(cx, |index, _| index.candidates("onlyinb", 10));
        assert!(
            leaked_into_a.is_empty(),
            "b's symbol must not appear in a's index"
        );
        let leaked_into_b = index_b.read_with(cx, |index, _| index.candidates("onlyina", 10));
        assert!(
            leaked_into_b.is_empty(),
            "a's symbol must not appear in b's index"
        );

        let found_a = index_a.read_with(cx, |index, _| index.candidates("onlyina", 10));
        assert_eq!(found_a.len(), 1, "{found_a:?}");
        let found_b = index_b.read_with(cx, |index, _| index.candidates("onlyinb", 10));
        assert_eq!(found_b.len(), 1, "{found_b:?}");
    }

    #[gpui::test]
    async fn a_failed_open_carries_the_reason_and_later_queries_still_answer(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let (_project_at, project) = a_project(&[("one.rs", "pub fn first() {}\n")], cx).await;

        // A file sitting exactly where the index would need to create its
        // own directory: `create_dir_all` cannot succeed over an existing
        // file, so opening the store fails before any pass is even started.
        let held = tempfile::tempdir().expect("a directory for the test");
        let index_dir = held.path().join("symbol_index");
        std::fs::write(&index_dir, b"in the way").expect("the file blocking the directory");

        let index = open_index(project, index_dir, cx);

        let state = index.read_with(cx, |index, _| index.state().clone());
        match state {
            State::Failed { reason } => assert!(!reason.is_empty(), "a reason must be recorded"),
            other => panic!("expected Failed, found {other:?}"),
        }
        let found = index.read_with(cx, |index, _| index.candidates("anything", 10));
        assert!(
            found.is_empty(),
            "a failed index answers empty rather than panicking"
        );
    }
}
