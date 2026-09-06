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

use crate::{check, import_roots, what_to_tell};

actions!(
    proto_diagnostics,
    [
        /// Compiles the open Protocol Buffers files with the protobuf
        /// compiler and shows what it found, without a language server.
        Check
    ]
);

/// The id these diagnostics are filed under. There is no server behind it, in
/// the way `cargo_diagnostics` has none: the editor's diagnostics are keyed
/// by server, so a source that is not a server still needs an id. Chosen far
/// above any a running server would be assigned, and one apart from every
/// other source that is not a server: the SQL validator, the Rust compiler,
/// the Go one, ruff, the JSON schemas, the C front end and oxlint.
const PROTOX_SERVER_ID: language::LanguageServerId = language::LanguageServerId(usize::MAX - 1013);

/// Idle time after a save before the file is compiled. Long enough to
/// collapse the burst a single save arrives as -- format-on-save writes, then
/// the buffer's own -- and no longer. A `.proto` file compiles in well under
/// a millisecond, so there is nothing here worth waiting to protect.
const SETTLE: Duration = Duration::from_millis(50);

#[derive(Default)]
struct Checking {
    /// The compile in flight for each buffer. Dropped, and so cancelled, when
    /// the next save starts another. One per buffer rather than one in total:
    /// two open files must not cancel each other.
    compiling: HashMap<EntityId, Task<()>>,
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut workspace::Workspace, _, cx| {
        let project = workspace.project().clone();
        let checking = Rc::new(RefCell::new(Checking::default()));

        // Every buffer already open, and every one opened later. Collected
        // first: reading the store borrows the context that watching one
        // needs mutably.
        let already_open: Vec<Entity<Buffer>> =
            project.read(cx).buffer_store().read(cx).buffers().collect();
        for buffer in already_open {
            watch_one(&project, &buffer, &checking, cx);
        }
        let buffer_store = project.read(cx).buffer_store().clone();
        cx.subscribe(&buffer_store, {
            let project = project.clone();
            let checking = checking.clone();
            move |_: &mut workspace::Workspace, _, event, cx| {
                if let BufferStoreEvent::BufferAdded(buffer) = event {
                    watch_one(&project, buffer, &checking, cx);
                }
            }
        })
        .detach();

        workspace.register_action({
            let project = project.clone();
            let checking = checking.clone();
            move |_, _: &Check, _, cx| {
                let open: Vec<Entity<Buffer>> =
                    project.read(cx).buffer_store().read(cx).buffers().collect();
                for buffer in open {
                    if is_proto(&buffer, cx) {
                        compile_soon(&project, &buffer, &checking, cx);
                    }
                }
            }
        });
    })
    .detach();
}

/// Watches one buffer, and compiles it after each save -- but only where no
/// language server is doing it already.
///
/// That condition is the whole design. A project with a Protocol Buffers
/// server running already has these diagnostics, and producing them here as
/// well would show the reader every problem twice. A project without one has
/// nothing, and this is what it gets. So the feature turns itself on exactly
/// where it is needed, and needs no setting to say so.
fn watch_one(
    project: &Entity<Project>,
    buffer: &Entity<Buffer>,
    checking: &Rc<RefCell<Checking>>,
    cx: &mut gpui::Context<workspace::Workspace>,
) {
    cx.subscribe(buffer, {
        let project = project.clone();
        let checking = checking.clone();
        move |_: &mut workspace::Workspace, buffer, event, cx| {
            if !matches!(
                event,
                language::BufferEvent::Saved | language::BufferEvent::LanguageChanged(_)
            ) {
                return;
            }
            // Asked here rather than before subscribing: a buffer's language
            // is often settled after the store reports it added, so a check
            // made once at subscription time misses the first file of a
            // session entirely -- which is what the `LanguageChanged` arm
            // above is for.
            if !is_proto(&buffer, cx) {
                return;
            }
            compile_soon(&project, &buffer, &checking, cx);
        }
    })
    .detach();
    cx.observe_release(buffer, {
        let checking = checking.clone();
        let id = buffer.entity_id();
        move |_: &mut workspace::Workspace, _: &mut Buffer, _| {
            checking.borrow_mut().compiling.remove(&id);
        }
    })
    .detach();

    // A buffer that is already open has not been saved, and would otherwise
    // wait for the reader to save a file that is wrong now.
    if is_proto(buffer, cx) {
        compile_soon(project, buffer, checking, cx);
    }
}

fn is_proto(buffer: &Entity<Buffer>, cx: &App) -> bool {
    buffer
        .read(cx)
        .language()
        .is_some_and(|language| language.name().as_ref() == "Proto")
}

fn compile_soon(
    project: &Entity<Project>,
    buffer: &Entity<Buffer>,
    checking: &Rc<RefCell<Checking>>,
    cx: &mut gpui::Context<workspace::Workspace>,
) {
    let id = buffer.entity_id();
    let project = project.downgrade();
    let buffer = buffer.downgrade();
    let task = cx.spawn(async move |_, cx| {
        cx.background_executor().timer(SETTLE).await;
        if let Err(trouble) = compile(&project, &buffer, cx).await {
            // None of this is the reader's problem: a buffer closed mid
            // compile, a file that has moved. None of it is a reason to
            // interrupt them, and none of it clears what they were shown.
            log::debug!("compiling a Proto buffer: {trouble:#}");
        }
    });
    checking.borrow_mut().compiling.insert(id, task);
}

/// Where the file is and where its worktree begins, gathered in one pass over
/// the application so the compile itself can leave it alone.
struct About {
    path: PathBuf,
    root: PathBuf,
}

/// Everything needed to compile the buffer, or nothing where it must not be
/// compiled: a buffer with no file on disk, or one a language server already
/// serves.
fn about(project: &Entity<Project>, buffer: &Entity<Buffer>, cx: &mut App) -> Option<About> {
    // Taken owned, and the borrow of the buffer let go, before anything is
    // asked of the application mutably below.
    let (path, worktree_id) = {
        let read = buffer.read(cx);
        let file = read.file()?;
        (file.as_local()?.abs_path(cx), file.worktree_id(cx))
    };
    let root = project
        .read(cx)
        .worktree_for_id(worktree_id, cx)?
        .read(cx)
        .abs_path()
        .to_path_buf();

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
    Some(About { path, root })
}

async fn compile(
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

    // The file is read here rather than taken from the buffer, and the same
    // text is what the places are converted against: the compiler reads from
    // disk, so its offsets are offsets into what is on disk. A buffer with
    // unsaved edits would put them on the wrong columns.
    let diagnostics = {
        let path = about.path.clone();
        let root = about.root.clone();
        cx.background_spawn(async move {
            let text = std::fs::read_to_string(&path).ok()?;
            let roots = import_roots(&root, &path);
            Some(what_to_tell(check(&path, &roots, &text)))
        })
        .await
    };
    let Some(diagnostics) = diagnostics else {
        return Ok(());
    };

    show(project, about.path, diagnostics, cx)
}

/// Hands the editor what the compiler said, replacing whatever this source
/// said last. An empty list is not silence and is meant: a file whose mistake
/// the reader has just fixed has to be told it is clean, or the editor keeps
/// showing the mistake.
fn show(
    project: &WeakEntity<Project>,
    path: PathBuf,
    diagnostics: Vec<lsp::Diagnostic>,
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
                        diagnostics,
                        version: None,
                    },
                    result_id: None,
                    registration_id: None,
                    server_id: PROTOX_SERVER_ID,
                    disk_based_sources: std::borrow::Cow::Borrowed(&[]),
                }],
                |_, _, _| false,
                cx,
            );
            if let Err(trouble) = merged {
                log::warn!(
                    "showing what the protobuf compiler said about {}: {trouble:#}",
                    path.display()
                );
            }
        })
    })
}
