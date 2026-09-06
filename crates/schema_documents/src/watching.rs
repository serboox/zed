use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use collections::HashMap;
use gpui::{App, AppContext as _, AsyncApp, Entity, EntityId, Task, WeakEntity};
use jsonschema::Validator;
use language::{Buffer, DiagnosticSourceKind, LanguageRegistry};
use project::buffer_store::BufferStoreEvent;
use project::{LspStore, Project};
use settings::SettingsLocation;

use crate::{Association, schema_covering, validator_for};

/// Idle time after an edit before the buffer is checked.
///
/// A compiler check costs minutes, so waiting for a save is worth it. This
/// costs a parse and a walk of a document that is measured in kilobytes --
/// tens of microseconds -- so waiting for a save would only mean the reader
/// stares at a mistake they have already made. What is left is the cost of
/// running it once per keystroke, which this avoids: 150ms is longer than the
/// gap between two keys of anyone typing, and short enough that a reader who
/// has stopped to look at what they wrote sees the answer already there.
const SETTLE: Duration = Duration::from_millis(150);

/// Everything one language's schema checking needs that is about that
/// language rather than about schemas. The rest of this file is the same for
/// all of them.
pub struct Reads {
    /// Which languages this covers, by the name the registry gives them.
    pub languages: &'static [&'static str],
    /// The id these diagnostics are filed under. There is no server behind
    /// it, in the way `cargo_diagnostics` has none: the editor's diagnostics
    /// are keyed by server, so a source that is not a server still needs an
    /// id. Chosen far above any a running server would be assigned, and one
    /// apart from every other source that is not a server.
    pub server_id: language::LanguageServerId,
    /// Everything the schema has to say about a text of this language.
    pub diagnostics: fn(&str, &Validator) -> Vec<lsp::Diagnostic>,
    /// Everything wrong with a text of this language that needs no schema to
    /// see -- a fault the grammar itself finds. A language whose whole
    /// report comes from a schema leaves this empty, and its files are then
    /// looked at only where a schema covers them, which is what JSON and
    /// YAML do.
    pub faults: Option<fn(&str) -> Vec<lsp::Diagnostic>>,
    /// The rules saying which schema covers which files.
    pub associations: for<'registry, 'at, 'app> fn(
        &'registry Arc<LanguageRegistry>,
        Option<SettingsLocation<'at>>,
        &'app mut App,
    ) -> serde_json::Value,
    /// How a schema is fetched, given the URI a rule named.
    pub fetch: fn(Entity<LspStore>, String, &mut AsyncApp) -> Task<Result<String>>,
}

/// A schema, kept as both what it arrived as and what it was built into.
///
/// The text is kept in order to notice a schema that has changed. Several of
/// them are made from what the editor currently holds -- the installed
/// themes, fonts and actions -- and are rebuilt behind this crate's back when
/// an extension is installed. Comparing the text is what makes a validator
/// that has gone stale get replaced instead of outliving the schema it came
/// from.
struct Built {
    came_as: String,
    validator: Arc<Validator>,
}

#[derive(Default)]
pub struct Watching {
    /// By schema URI. A `None` is a schema that would not build, remembered
    /// so it is not built again on every keystroke; it means silence, and
    /// silence is what a reader whose schema is broken should get.
    schemas: HashMap<String, Option<Built>>,
    /// The schema covering each file, by absolute path, worked out once. A
    /// `None` is a file no schema covers, which is most of them.
    covering: HashMap<PathBuf, Option<String>>,
    /// The check in flight for each buffer. Dropped, and so cancelled, when
    /// the next edit to that buffer starts another. One per buffer rather
    /// than one in total: two open files must not cancel each other.
    checking: HashMap<EntityId, Task<()>>,
}

/// Watches every buffer of these languages in one workspace, and checks each
/// after an edit -- but only where no language server is doing it already.
///
/// That condition is the whole design. A project with the language's server
/// running already has these diagnostics, and producing them here as well
/// would show the reader every problem twice. A project without one has
/// nothing, and this is what it gets. So the feature turns itself on exactly
/// where it is needed, and needs no setting to say so.
pub fn watch(
    reads: &'static Reads,
    workspace: &mut workspace::Workspace,
    cx: &mut gpui::Context<workspace::Workspace>,
) -> Rc<RefCell<Watching>> {
    let project = workspace.project().clone();
    let watching = Rc::new(RefCell::new(Watching::default()));

    // Every buffer already open, and every one opened later. Collected
    // first: reading the store borrows the context that watching one needs
    // mutably.
    let already_open: Vec<Entity<Buffer>> =
        project.read(cx).buffer_store().read(cx).buffers().collect();
    for buffer in already_open {
        watch_one(reads, &project, &buffer, &watching, cx);
    }
    let buffer_store = project.read(cx).buffer_store().clone();
    cx.subscribe(&buffer_store, {
        let project = project.clone();
        let watching = watching.clone();
        move |_: &mut workspace::Workspace, _, event, cx| {
            if let BufferStoreEvent::BufferAdded(buffer) = event {
                watch_one(reads, &project, buffer, &watching, cx);
            }
        }
    })
    .detach();

    watching
}

/// Forgets everything worked out once and checks every open buffer again.
///
/// This is the way to pick up a schema or a rule that changed after the files
/// it covers were opened.
pub fn recheck_everything(
    reads: &'static Reads,
    watching: &Rc<RefCell<Watching>>,
    project: &Entity<Project>,
    cx: &mut gpui::Context<workspace::Workspace>,
) {
    {
        let mut watching = watching.borrow_mut();
        watching.schemas.clear();
        watching.covering.clear();
    }
    let open: Vec<Entity<Buffer>> = project.read(cx).buffer_store().read(cx).buffers().collect();
    for buffer in open {
        if is_covered(reads, &buffer, cx) {
            check_soon(reads, project, &buffer, watching, cx);
        }
    }
}

fn watch_one(
    reads: &'static Reads,
    project: &Entity<Project>,
    buffer: &Entity<Buffer>,
    watching: &Rc<RefCell<Watching>>,
    cx: &mut gpui::Context<workspace::Workspace>,
) {
    cx.subscribe(buffer, {
        let project = project.clone();
        let watching = watching.clone();
        move |_: &mut workspace::Workspace, buffer, event, cx| {
            if !matches!(
                event,
                language::BufferEvent::Edited { .. } | language::BufferEvent::LanguageChanged(_)
            ) {
                return;
            }
            // Asked here rather than before subscribing: a buffer's language
            // is often settled after the store reports it added, so a check
            // made once at subscription time misses the first file of a
            // session entirely -- which is exactly what the
            // `LanguageChanged` arm above is for.
            if !is_covered(reads, &buffer, cx) {
                return;
            }
            check_soon(reads, &project, &buffer, &watching, cx);
        }
    })
    .detach();
    cx.observe_release(buffer, {
        let watching = watching.clone();
        let id = buffer.entity_id();
        move |_: &mut workspace::Workspace, _: &mut Buffer, _| {
            watching.borrow_mut().checking.remove(&id);
        }
    })
    .detach();

    // A buffer that is already open has not been edited, and would otherwise
    // wait for its first keystroke to say anything about a file that is
    // wrong now. One that is not of this language yet is caught by
    // `LanguageChanged`.
    if is_covered(reads, buffer, cx) {
        check_soon(reads, project, buffer, watching, cx);
    }
}

fn is_covered(reads: &Reads, buffer: &Entity<Buffer>, cx: &App) -> bool {
    buffer.read(cx).language().is_some_and(|language| {
        let name = language.name();
        reads
            .languages
            .iter()
            .any(|covered| *covered == name.as_ref())
    })
}

fn check_soon(
    reads: &'static Reads,
    project: &Entity<Project>,
    buffer: &Entity<Buffer>,
    watching: &Rc<RefCell<Watching>>,
    cx: &mut gpui::Context<workspace::Workspace>,
) {
    let id = buffer.entity_id();
    let project = project.downgrade();
    let buffer = buffer.downgrade();
    let held = watching.clone();
    let task = cx.spawn(async move |_, cx| {
        cx.background_executor().timer(SETTLE).await;
        if let Err(error) = check(reads, &project, &buffer, &held, cx).await {
            // Nothing here is the reader's problem: a schema that will not
            // build, a buffer closed mid-check. None of it is a reason to
            // interrupt them, and none of it clears what they were shown.
            log::debug!("checking a buffer against its schema: {error:#}");
        }
    });
    watching.borrow_mut().checking.insert(id, task);
}

/// What has to be known before anything can be checked, gathered in one pass
/// over the application so the rest of the work can leave it alone.
struct About {
    path: PathBuf,
    text: String,
    /// The schema covering this file, already resolved to a URI, or nothing
    /// where none covers it -- which is most files, and every TOML one.
    covered_by: Option<String>,
}

fn about(
    reads: &Reads,
    project: &Entity<Project>,
    buffer: &Entity<Buffer>,
    watching: &Rc<RefCell<Watching>>,
    cx: &mut App,
) -> Option<About> {
    // Taken owned, and the borrow of the buffer let go, before anything is
    // asked of the application mutably below.
    let (path, worktree_id, in_worktree, text) = {
        let read = buffer.read(cx);
        let file = read.file()?;
        let local = file.as_local()?;
        (
            local.abs_path(cx),
            file.worktree_id(cx),
            file.path().clone(),
            read.text(),
        )
    };
    if served_by_a_language_server(project, buffer, cx) {
        return None;
    }
    let covered_by = covering(
        reads,
        &path,
        SettingsLocation {
            worktree_id,
            path: in_worktree.as_ref(),
        },
        project,
        watching,
        cx,
    );
    if covered_by.is_none() && reads.faults.is_none() {
        return None;
    }
    Some(About {
        path,
        text,
        covered_by,
    })
}

/// Whether a language server already answers for this buffer.
///
/// This is the whole of the condition that keeps an in-process source quiet:
/// a project with the language's server running already has these
/// diagnostics, and saying them again would show the reader every problem
/// twice.
pub fn served_by_a_language_server(
    project: &Entity<Project>,
    buffer: &Entity<Buffer>,
    cx: &mut App,
) -> bool {
    let lsp_store = project.read(cx).lsp_store();
    buffer.update(cx, |buffer, cx| {
        lsp_store.update(cx, |lsp_store, cx| {
            !lsp_store
                .language_servers_for_local_buffer(buffer, cx)
                .is_empty()
        })
    })
}

/// The schema covering a file, worked out once per file and kept.
///
/// Working it out means building the whole list of rules, and that list names
/// every action the editor knows, so it costs a few thousand allocations. Once
/// per file is affordable; once per keystroke, which is what this runs at,
/// would not be. What it costs is freshness: a rule added mid-session by an
/// extension, or by a `file_types` line in the settings, reaches files opened
/// after it and not files already open. [`recheck_everything`] forgets all of
/// this, and is how a reader gets the new rule without reopening anything.
fn covering(
    reads: &Reads,
    path: &Path,
    at: SettingsLocation<'_>,
    project: &Entity<Project>,
    watching: &Rc<RefCell<Watching>>,
    cx: &mut App,
) -> Option<String> {
    if let Some(known) = watching.borrow().covering.get(path) {
        return known.clone();
    }
    let languages = project.read(cx).languages().clone();
    let rules = (reads.associations)(&languages, Some(at), cx);
    let found = serde_json::from_value::<Vec<Association>>(rules)
        .ok()
        .and_then(|rules| schema_covering(&rules, path));
    watching
        .borrow_mut()
        .covering
        .insert(path.to_path_buf(), found.clone());
    found
}

async fn check(
    reads: &'static Reads,
    project: &WeakEntity<Project>,
    buffer: &WeakEntity<Buffer>,
    watching: &Rc<RefCell<Watching>>,
    cx: &mut AsyncApp,
) -> Result<()> {
    // A buffer a language server already covers ends here, and so does a
    // file that neither a schema nor a fault of its own has anything to say
    // about: nothing is published, so nothing any other source put on that
    // file is disturbed either.
    let about = cx.update(|cx| {
        let project = project.upgrade()?;
        let buffer = buffer.upgrade()?;
        about(reads, &project, &buffer, watching, cx)
    });
    let Some(about) = about else {
        return Ok(());
    };

    let mut diagnostics = Vec::new();
    if let Some(faults) = reads.faults {
        let text = about.text.clone();
        diagnostics.extend(cx.background_spawn(async move { faults(&text) }).await);
    }
    if let Some(uri) = about.covered_by {
        match validator(reads, &uri, project, watching, cx).await {
            Some(validator) => {
                let text = about.text;
                let say = reads.diagnostics;
                diagnostics.extend(
                    cx.background_spawn(async move { say(&text, &validator) })
                        .await,
                );
            }
            // A schema that would not resolve or would not build means
            // silence for a language that has nothing else to say, so that
            // what was shown last is left where it is rather than cleared.
            None if reads.faults.is_none() => return Ok(()),
            None => {}
        }
    }
    show(reads, project, about.path, diagnostics, cx)
}

/// The validator for a schema, built once and kept, or nothing at all where
/// the schema will not resolve or will not build. Nothing means silence: a
/// reader whose schema is broken is no worse off than one who has no schema,
/// and is certainly not helped by an error about a file they did not write.
async fn validator(
    reads: &Reads,
    uri: &str,
    project: &WeakEntity<Project>,
    watching: &Rc<RefCell<Watching>>,
    cx: &mut AsyncApp,
) -> Option<Arc<Validator>> {
    let lsp_store = project
        .read_with(cx, |project, _| project.lsp_store())
        .ok()?;
    let came_as = (reads.fetch)(lsp_store, uri.to_string(), cx).await.ok()?;

    let known = match watching.borrow().schemas.get(uri) {
        Some(Some(built)) if built.came_as == came_as => Some(Some(built.validator.clone())),
        // A schema that would not build stays unbuilt until it changes,
        // rather than being rebuilt on every keystroke.
        Some(None) => Some(None),
        _ => None,
    };
    if let Some(known) = known {
        return known;
    }

    let built = cx
        .background_spawn(async move {
            let schema = serde_json::from_str(&came_as).context("this schema is not JSON")?;
            let validator = validator_for(&schema)?;
            anyhow::Ok(Built {
                came_as,
                validator: Arc::new(validator),
            })
        })
        .await;
    let built = match built {
        Ok(built) => built,
        Err(error) => {
            log::debug!("building the validator for {uri}: {error:#}");
            watching.borrow_mut().schemas.insert(uri.to_string(), None);
            return None;
        }
    };
    let validator = built.validator.clone();
    watching
        .borrow_mut()
        .schemas
        .insert(uri.to_string(), Some(built));
    Some(validator)
}

/// Hands the editor what the schema said, replacing whatever this source said
/// last. An empty list is not silence and is meant: a file whose mistake the
/// reader has just fixed has to be told it is clean, or the editor keeps
/// showing the mistake.
fn show(
    reads: &Reads,
    project: &WeakEntity<Project>,
    path: PathBuf,
    diagnostics: Vec<lsp::Diagnostic>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let uri = lsp::Uri::from_file_path(&path)
        .map_err(|_| anyhow::anyhow!("{} is not a path with a URI", path.display()))?;
    let server_id = reads.server_id;
    project.update(cx, |project, cx| {
        project.lsp_store().update(cx, |lsp_store, cx| {
            let merged = lsp_store.merge_lsp_diagnostics(
                DiagnosticSourceKind::Pushed,
                vec![project::lsp_store::DocumentDiagnosticsUpdate {
                    diagnostics: lsp::PublishDiagnosticsParams {
                        uri,
                        diagnostics,
                        version: None,
                    },
                    result_id: None,
                    registration_id: None,
                    server_id,
                    disk_based_sources: std::borrow::Cow::Borrowed(&[]),
                }],
                |_, _, _| false,
                cx,
            );
            if let Err(error) = merged {
                log::warn!(
                    "showing the schema's report for {}: {error:#}",
                    path.display()
                );
            }
        })
    })
}
