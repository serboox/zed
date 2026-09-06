use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use collections::HashMap;
use gpui::{App, AppContext as _, AsyncApp, Entity, EntityId, Task, WeakEntity, actions};
use language::{Buffer, DiagnosticSourceKind};
use project::Project;
use project::buffer_store::BufferStoreEvent;

use crate::{Configured, configuration_for, diagnostics_for, rules_for};

actions!(
    markdown_diagnostics,
    [
        /// Reads this project's Markdown configuration again and relints every
        /// open Markdown file, without a language server.
        Lint
    ]
);

/// The id these diagnostics are filed under. There is no server behind it, in
/// the way `cargo_diagnostics` has none: the editor's diagnostics are keyed by
/// server, so a source that is not a server still needs an id. Chosen far
/// above any a running server would be assigned, and one apart from every
/// other source that is not a server.
const MARKDOWN_SERVER_ID: language::LanguageServerId =
    language::LanguageServerId(usize::MAX - 1012);

/// Idle time after an edit before the document is linted. The linter runs in
/// this process and costs a few milliseconds over a document of a few thousand
/// lines, so there is nothing here worth waiting for a save to collect; what is
/// left is the cost of running it on every keystroke, which this avoids.
const SETTLE: Duration = Duration::from_millis(150);

#[derive(Default)]
struct Watching {
    /// The configuration covering each file, by absolute path, read once.
    ///
    /// Reading it walks the file's directory and every directory above it
    /// looking for four rumdl file names and eleven `markdownlint` ones. Once
    /// per file is affordable; once per keystroke, which is what this runs at,
    /// would not be. What it costs is freshness: a config file edited
    /// mid-session reaches files opened after it and not files already open.
    /// The `Lint` action forgets all of this, and is how a reader picks up a
    /// config they have just changed.
    configurations: HashMap<PathBuf, Arc<Configured>>,
    /// The lint in flight for each buffer. Dropped, and so cancelled, when the
    /// next edit to that buffer starts another. One per buffer rather than one
    /// in total: two open files must not cancel each other.
    linting: HashMap<EntityId, Task<()>>,
}

/// Lints every Markdown buffer in one workspace after an edit -- but only
/// where no language server is doing it already.
///
/// That condition is the whole design. A project with a Markdown server running
/// already has its diagnostics, and producing them here as well would show the
/// reader every finding twice. A project without one has nothing, and this is
/// what it gets. So the feature turns itself on exactly where it is needed, and
/// needs no setting to say so.
pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut workspace::Workspace, _, cx| {
        let project = workspace.project().clone();
        let watching = Rc::new(RefCell::new(Watching::default()));

        // Every buffer already open, and every one opened later. Collected
        // first: reading the store borrows the context that watching one needs
        // mutably.
        let already_open: Vec<Entity<Buffer>> =
            project.read(cx).buffer_store().read(cx).buffers().collect();
        for buffer in already_open {
            watch_one(&project, &buffer, &watching, cx);
        }
        let buffer_store = project.read(cx).buffer_store().clone();
        cx.subscribe(&buffer_store, {
            let project = project.clone();
            let watching = watching.clone();
            move |_: &mut workspace::Workspace, _, event, cx| {
                if let BufferStoreEvent::BufferAdded(buffer) = event {
                    watch_one(&project, buffer, &watching, cx);
                }
            }
        })
        .detach();

        workspace.register_action({
            let project = project.clone();
            let watching = watching.clone();
            move |_, _: &Lint, _, cx| {
                relint_everything(&project, &watching, cx);
            }
        });
    })
    .detach();
}

fn relint_everything(
    project: &Entity<Project>,
    watching: &Rc<RefCell<Watching>>,
    cx: &mut gpui::Context<workspace::Workspace>,
) {
    watching.borrow_mut().configurations.clear();
    let open: Vec<Entity<Buffer>> = project.read(cx).buffer_store().read(cx).buffers().collect();
    for buffer in open {
        if is_markdown(&buffer, cx) {
            lint_soon(project, &buffer, watching, cx);
        }
    }
}

fn watch_one(
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
            // Asked here rather than before subscribing: a buffer's language is
            // often settled after the store reports it added, so a check made
            // once at subscription time misses the first file of a session
            // entirely -- which is exactly what the `LanguageChanged` arm above
            // is for.
            if !is_markdown(&buffer, cx) {
                return;
            }
            lint_soon(&project, &buffer, &watching, cx);
        }
    })
    .detach();
    cx.observe_release(buffer, {
        let watching = watching.clone();
        let id = buffer.entity_id();
        move |_: &mut workspace::Workspace, _: &mut Buffer, _| {
            watching.borrow_mut().linting.remove(&id);
        }
    })
    .detach();

    // A buffer that is already open has not been edited, and would otherwise
    // wait for its first keystroke to say anything about a document that is
    // wrong now. One that is not Markdown yet is caught by `LanguageChanged`.
    if is_markdown(buffer, cx) {
        lint_soon(project, buffer, watching, cx);
    }
}

fn is_markdown(buffer: &Entity<Buffer>, cx: &App) -> bool {
    buffer
        .read(cx)
        .language()
        .is_some_and(|language| language.name().as_ref() == "Markdown")
}

fn lint_soon(
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
        if let Err(error) = lint(&project, &buffer, &held, cx).await {
            // A buffer closed mid-lint, a workspace going away: none of it is
            // the reader's problem, and none of it clears what they were shown.
            log::debug!("linting a Markdown buffer: {error:#}");
        }
    });
    watching.borrow_mut().linting.insert(id, task);
}

/// What has to be known before anything can be linted, gathered in one pass
/// over the application so the rest of the work can leave it alone.
struct About {
    path: PathBuf,
    project_root: PathBuf,
    text: String,
}

fn about(project: &Entity<Project>, buffer: &Entity<Buffer>, cx: &mut App) -> Option<About> {
    // Taken owned, and the borrow of the buffer let go, before anything is
    // asked of the application mutably below.
    let (path, worktree_id, text) = {
        let read = buffer.read(cx);
        let file = read.file()?;
        let local = file.as_local()?;
        (local.abs_path(cx), file.worktree_id(cx), read.text())
    };
    let lsp_store = project.read(cx).lsp_store();
    let served_by_a_language_server = buffer.update(cx, |buffer, cx| {
        lsp_store.update(cx, |lsp_store, cx| {
            !lsp_store
                .language_servers_for_local_buffer(buffer, cx)
                .is_empty()
        })
    });
    if served_by_a_language_server {
        return None;
    }
    let project_root = project
        .read(cx)
        .worktree_for_id(worktree_id, cx)?
        .read(cx)
        .abs_path()
        .to_path_buf();
    Some(About {
        path,
        project_root,
        text,
    })
}

async fn lint(
    project: &WeakEntity<Project>,
    buffer: &WeakEntity<Buffer>,
    watching: &Rc<RefCell<Watching>>,
    cx: &mut AsyncApp,
) -> Result<()> {
    // A buffer a language server already covers, and one with no file behind
    // it, both end here: nothing is published, so nothing any other source put
    // on that file is disturbed either.
    let about = cx.update(|cx| {
        let project = project.upgrade()?;
        let buffer = buffer.upgrade()?;
        about(&project, &buffer, cx)
    });
    let Some(about) = about else {
        return Ok(());
    };
    let configured = configuration(&about, watching, cx).await;

    let text = about.text;
    let path = about.path.clone();
    let diagnostics = cx
        .background_spawn(async move {
            let rules = rules_for(&configured, Some(&path));
            diagnostics_for(&text, &rules, &configured.config, Some(&path))
        })
        .await;
    show(project, about.path, diagnostics, cx)
}

async fn configuration(
    about: &About,
    watching: &Rc<RefCell<Watching>>,
    cx: &mut AsyncApp,
) -> Arc<Configured> {
    if let Some(known) = watching.borrow().configurations.get(&about.path) {
        return known.clone();
    }
    let path = about.path.clone();
    let project_root = about.project_root.clone();
    let configured = cx
        .background_spawn(async move { Arc::new(configuration_for(&path, &project_root)) })
        .await;
    watching
        .borrow_mut()
        .configurations
        .insert(about.path.clone(), configured.clone());
    configured
}

/// Hands the editor what the linter said, replacing whatever this source said
/// last. An empty list is not silence and is meant: a document whose fault the
/// reader has just fixed has to be told it is clean now, or the editor keeps
/// showing the fault.
fn show(
    project: &WeakEntity<Project>,
    path: PathBuf,
    diagnostics: Vec<lsp::Diagnostic>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let uri = lsp::Uri::from_file_path(&path)
        .map_err(|_| anyhow::anyhow!("{} is not a path with a URI", path.display()))?;
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
                    server_id: MARKDOWN_SERVER_ID,
                    disk_based_sources: std::borrow::Cow::Borrowed(&[]),
                }],
                |_, _, _| false,
                cx,
            );
            if let Err(error) = merged {
                log::warn!(
                    "showing the linter's report for {}: {error:#}",
                    path.display()
                );
            }
        })
    })
}
