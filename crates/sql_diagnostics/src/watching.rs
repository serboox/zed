use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use collections::HashMap;
use gpui::{App, AppContext as _, AsyncApp, Entity, EntityId, Task, WeakEntity, actions};
use language::{Buffer, DiagnosticSourceKind};
use project::Project;
use project::buffer_store::BufferStoreEvent;

use crate::{Configured, configuration_covering};

actions!(
    sql_diagnostics,
    [
        /// Lints the open SQL buffers against the project's own sqruff
        /// configuration and shows what it found, without a language server.
        Lint
    ]
);

/// The id these diagnostics are filed under. There is no server behind it, in
/// the way `cargo_diagnostics` has none: the editor's diagnostics are keyed by
/// server, so a source that is not a server still needs an id. Chosen far above
/// any a running server would be assigned, and one apart from every other
/// source that is not a server -- among them the SQL console's own validator,
/// which reports on the same files and must not be overwritten here.
const SQRUFF_SERVER_ID: language::LanguageServerId = language::LanguageServerId(usize::MAX - 1011);

/// Idle time after a save before the buffer is linted. Long enough to collapse
/// the burst a single save arrives as -- format-on-save writes, then the
/// buffer's own -- and no longer: sqruff over one statement costs a fraction of
/// a millisecond, so there is nothing here worth waiting to protect.
const SETTLE: Duration = Duration::from_millis(50);

#[derive(Default)]
struct Watching {
    /// The lint in flight for each buffer. Dropped, and so cancelled, when the
    /// next save starts another. One per buffer rather than one in total: two
    /// open files must not cancel each other.
    linting: HashMap<EntityId, Task<()>>,
}

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

        workspace.register_action(move |_, _: &Lint, _, cx| {
            let open: Vec<Entity<Buffer>> =
                project.read(cx).buffer_store().read(cx).buffers().collect();
            for buffer in open {
                if is_sql(&buffer, cx) {
                    lint_soon(&project, &buffer, &watching, cx);
                }
            }
        });
    })
    .detach();
}

/// Watches one buffer, and lints it after each save -- but only where no
/// language server is doing it already.
///
/// That condition is the whole design. A project with a SQL server running
/// already has its diagnostics, and producing them here as well would show the
/// reader the same finding twice. A project without one has nothing, and this
/// is what it gets. So the feature turns itself on exactly where it is needed,
/// and needs no setting to say so.
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
                language::BufferEvent::Saved | language::BufferEvent::LanguageChanged(_)
            ) {
                return;
            }
            // Asked here rather than before subscribing: a buffer's language is
            // often settled after the store reports it added, so a check made
            // once at subscription time misses the first file of a session
            // entirely -- which is exactly what the `LanguageChanged` arm above
            // is for.
            if !is_sql(&buffer, cx) {
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

    // A buffer that is already open has not been saved, and would otherwise
    // wait for the reader to save a file that is wrong now.
    if is_sql(buffer, cx) {
        lint_soon(project, buffer, watching, cx);
    }
}

fn is_sql(buffer: &Entity<Buffer>, cx: &App) -> bool {
    buffer
        .read(cx)
        .language()
        .is_some_and(|language| language.name().as_ref() == "SQL")
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
    let task = cx.spawn(async move |_, cx| {
        cx.background_executor().timer(SETTLE).await;
        if let Err(error) = lint(&project, &buffer, cx).await {
            // None of this is the reader's problem: a file with no path yet, a
            // buffer closed mid-lint, a project that is being torn down.
            log::debug!("linting a SQL buffer: {error:#}");
        }
    });
    watching.borrow_mut().linting.insert(id, task);
}

/// What has to be known before anything can be linted, gathered in one pass
/// over the application so the rest of the work can leave it alone.
struct About {
    path: PathBuf,
    root: PathBuf,
    text: String,
}

fn about(project: &Entity<Project>, buffer: &Entity<Buffer>, cx: &mut App) -> Option<About> {
    // Taken owned, and the borrow of the buffer let go, before anything is
    // asked of the application mutably below.
    let (path, worktree_id, text) = {
        let read = buffer.read(cx);
        let file = read.file()?;
        (
            file.as_local()?.abs_path(cx),
            file.worktree_id(cx),
            read.text(),
        )
    };
    let root = project
        .read(cx)
        .worktree_for_id(worktree_id, cx)?
        .read(cx)
        .abs_path()
        .to_path_buf();

    let lsp_store = project.read(cx).lsp_store();
    // Nested this way round because asking needs the buffer and the application
    // both, and the buffer's own update is what hands over one without holding
    // the other.
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
    Some(About { path, root, text })
}

async fn lint(
    project: &WeakEntity<Project>,
    buffer: &WeakEntity<Buffer>,
    cx: &mut AsyncApp,
) -> Result<()> {
    // A buffer a language server already covers ends here: nothing is
    // published, so nothing any other source put on that file is disturbed.
    let about = cx.update(|cx| {
        let project = project.upgrade()?;
        let buffer = buffer.upgrade()?;
        about(&project, &buffer, cx)
    });
    let Some(about) = about else {
        return Ok(());
    };

    let path = about.path.clone();
    let findings = cx
        .background_spawn(async move {
            let covering =
                configuration_covering(&about.path, &about.root, |candidate| candidate.is_file());
            // A project that has said nothing about its SQL is reported on as
            // clean rather than skipped, so that deleting the configuration
            // also takes down what it had already been shown.
            Configured::from_files(&covering)
                .map(|configured| configured.findings_in(&about.text))
                .unwrap_or_default()
        })
        .await;

    show(project, path, findings, cx)
}

/// Hands the editor what the linter said, replacing whatever this source said
/// last. An empty list is not silence and is meant: a file whose finding the
/// reader has just fixed has to be told it is clean, or the editor keeps
/// showing the finding.
fn show(
    project: &WeakEntity<Project>,
    path: PathBuf,
    findings: Vec<lsp::Diagnostic>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let uri = lsp::Uri::from_file_path(&path)
        .map_err(|_| anyhow!("{} is not a path with a URI", path.display()))?;
    project.update(cx, |project, cx| {
        project.lsp_store().update(cx, |lsp_store, cx| {
            let merged = lsp_store.merge_lsp_diagnostics(
                DiagnosticSourceKind::Pushed,
                vec![project::lsp_store::DocumentDiagnosticsUpdate {
                    diagnostics: lsp::PublishDiagnosticsParams {
                        uri,
                        diagnostics: findings,
                        version: None,
                    },
                    result_id: None,
                    registration_id: None,
                    server_id: SQRUFF_SERVER_ID,
                    disk_based_sources: std::borrow::Cow::Borrowed(&[]),
                }],
                |_, _, _| false,
                cx,
            );
            if let Err(error) = merged {
                log::warn!(
                    "showing what sqruff said about {}: {error:#}",
                    path.display()
                );
            }
        })
    })
}
