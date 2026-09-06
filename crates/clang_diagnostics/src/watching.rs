use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use collections::{HashMap, HashSet};
use gpui::{App, AppContext as _, AsyncApp, Entity, EntityId, Task, WeakEntity, actions};
use language::{Buffer, DiagnosticSourceKind};
use project::Project;
use project::buffer_store::BufferStoreEvent;

use crate::compile_commands::{arguments_for, entries_in, where_the_database_is};
use crate::parsing::{Request, ask_libclang};
use crate::{Finding, as_diagnostics};

actions!(
    clang_diagnostics,
    [
        /// Parses the open C and C++ buffers with the compiler's own front end
        /// and shows what it found, without a language server.
        Diagnose
    ]
);

/// The id these diagnostics are filed under. There is no server behind it, in
/// the way `cargo_diagnostics` has none: the editor's diagnostics are keyed by
/// server, so a source that is not a server still needs an id. Chosen far above
/// any a running server would be assigned, and one apart from every other
/// source that is not a server: the SQL validator, the Rust compiler, the Go
/// one, ruff and the JSON schemas.
const CLANG_SERVER_ID: language::LanguageServerId = language::LanguageServerId(usize::MAX - 1006);

/// Idle time after a save before the file is parsed.
///
/// Long enough to collapse the burst a single save arrives as -- format-on-save
/// writes, then the buffer's own. Unlike the schema check, which runs per
/// keystroke, this is deliberately tied to a save: parsing a C++ translation
/// unit means expanding every header it reaches, which costs a fraction of a
/// second at best, and doing that while somebody is typing would spend the
/// machine on answers nobody waited for.
const SETTLE: Duration = Duration::from_millis(100);

#[derive(Default)]
struct Watching {
    /// The parse in flight for each buffer. Dropped, and so cancelled, when the
    /// next save starts another. One per buffer rather than one in total: two
    /// open files must not cancel each other.
    parsing: HashMap<EntityId, Task<()>>,
    /// Files a database was already looked for and not found, so the walk up
    /// the tree is not repeated on every save of a project that has none --
    /// which is most C projects there are.
    without_a_database: HashSet<PathBuf>,
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

        workspace.register_action({
            let project = project.clone();
            let watching = watching.clone();
            move |_, _: &Diagnose, _, cx| {
                // What was looked for and not found is forgotten first, so that
                // this is also how a reader picks up a compilation database
                // their build has just written.
                watching.borrow_mut().without_a_database.clear();
                let open: Vec<Entity<Buffer>> =
                    project.read(cx).buffer_store().read(cx).buffers().collect();
                for buffer in open {
                    if is_c_or_cpp(&buffer, cx) {
                        parse_soon(&project, &buffer, &watching, cx);
                    }
                }
            }
        });
    })
    .detach();
}

/// Watches one buffer, and parses it after each save -- but only where no
/// language server is doing it already.
///
/// That condition is the whole design. A project with `clangd` running already
/// has these diagnostics, and producing them here as well would show the reader
/// every problem twice. A project without one has nothing, and this is what it
/// gets. So the feature turns itself on exactly where it is needed, and needs
/// no setting to say so.
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
            if !is_c_or_cpp(&buffer, cx) {
                return;
            }
            parse_soon(&project, &buffer, &watching, cx);
        }
    })
    .detach();
    cx.observe_release(buffer, {
        let watching = watching.clone();
        let id = buffer.entity_id();
        move |_: &mut workspace::Workspace, _: &mut Buffer, _| {
            watching.borrow_mut().parsing.remove(&id);
        }
    })
    .detach();

    // A buffer that is already open has not been saved, and would otherwise
    // wait for the reader to save a file that is wrong now.
    if is_c_or_cpp(buffer, cx) {
        parse_soon(project, buffer, watching, cx);
    }
}

fn is_c_or_cpp(buffer: &Entity<Buffer>, cx: &App) -> bool {
    buffer
        .read(cx)
        .language()
        .is_some_and(|language| matches!(language.name().as_ref(), "C" | "C++"))
}

fn parse_soon(
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
        if let Err(error) = parse(&project, &buffer, &held, cx).await {
            // None of this is the reader's problem: a machine with no libclang,
            // a project with no compilation database, a buffer closed
            // mid-parse. None of it is a reason to interrupt them, and none of
            // it clears what they were shown.
            log::debug!("parsing a C or C++ buffer: {error:#}");
        }
    });
    watching.borrow_mut().parsing.insert(id, task);
}

/// What has to be known before anything can be parsed, gathered in one pass
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

async fn parse(
    project: &WeakEntity<Project>,
    buffer: &WeakEntity<Buffer>,
    watching: &Rc<RefCell<Watching>>,
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
    if watching.borrow().without_a_database.contains(&about.path) {
        return Ok(());
    }

    let arguments = {
        let path = about.path.clone();
        let root = about.root.clone();
        cx.background_spawn(async move { how_it_is_compiled(&path, &root) })
            .await
    };
    // No compilation database is the ordinary case, and it means silence. A
    // translation unit's include paths and defines are unknown without one, and
    // a parse without them reports a screenful of missing headers that says
    // nothing about the code the reader wrote.
    let Some(arguments) = arguments else {
        watching.borrow_mut().without_a_database.insert(about.path);
        return Ok(());
    };

    let parsed = ask_libclang(Request {
        file: about.path.clone(),
        text: about.text.clone(),
        arguments,
    })
    .await
    .with_context(|| format!("parsing {}", about.path.display()))?;
    log::debug!(
        "{}: {} findings, {} MB in the translation unit, {} held",
        about.path.display(),
        parsed.findings.len(),
        parsed.memory / 1_048_576,
        parsed.held,
    );

    show(project, about.path, &parsed.findings, &about.text, cx)
}

/// The arguments a file is compiled with, from the compilation database nearest
/// it, or nothing at all where there is none.
fn how_it_is_compiled(path: &Path, root: &Path) -> Option<Vec<String>> {
    let database = where_the_database_is(path, root, |candidate| candidate.is_file())?;
    let text = std::fs::read_to_string(&database).ok()?;
    arguments_for(&entries_in(&text), path)
}

/// Hands the editor what the front end said, replacing whatever this source
/// said last. An empty list is not silence and is meant: a file whose mistake
/// the reader has just fixed has to be told it is clean, or the editor keeps
/// showing the mistake.
fn show(
    project: &WeakEntity<Project>,
    path: PathBuf,
    findings: &[Finding],
    text: &str,
    cx: &mut AsyncApp,
) -> Result<()> {
    let uri = lsp::Uri::from_file_path(&path)
        .map_err(|_| anyhow::anyhow!("{} is not a path with a URI", path.display()))?;
    let diagnostics = as_diagnostics(findings, text);
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
                    server_id: CLANG_SERVER_ID,
                    disk_based_sources: std::borrow::Cow::Borrowed(&[]),
                }],
                |_, _, _| false,
                cx,
            );
            if let Err(error) = merged {
                log::warn!(
                    "showing what libclang said about {}: {error:#}",
                    path.display()
                );
            }
        })
    })
}
