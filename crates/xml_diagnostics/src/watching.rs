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

use crate::faults_in;

actions!(
    xml_diagnostics,
    [
        /// Reports what is wrong with every open XML document, without a
        /// language server.
        Check
    ]
);

/// The id these diagnostics are filed under. There is no server behind it, in
/// the way `cargo_diagnostics` has none: the editor's diagnostics are keyed
/// by server, so a source that is not a server still needs an id. Chosen far
/// above any a running server would be assigned, and one apart from every
/// other source that is not a server.
const XML_SERVER_ID: language::LanguageServerId = language::LanguageServerId(usize::MAX - 1016);

const XML_LANGUAGE: &str = "XML";

/// Idle time after an edit before the document is read. The parser runs in
/// this process and costs a fraction of a millisecond over a document of a
/// few thousand lines, so there is nothing here worth waiting for a save to
/// collect; what is left is the cost of running it on every keystroke, which
/// this avoids -- and a document mid-keystroke is half-written rather than
/// wrong, which is not worth underlining.
const SETTLE: Duration = Duration::from_millis(150);

#[derive(Default)]
struct Watching {
    /// The read in flight for each buffer. Dropped, and so cancelled, when
    /// the next edit to that buffer starts another. One per buffer rather
    /// than one in total: two open documents must not cancel each other.
    reading: HashMap<EntityId, Task<()>>,
}

/// Reports what is wrong with every XML document in one workspace after an
/// edit -- but only where no language server is doing it already.
///
/// That condition is the whole design. A project with an XML server running
/// already has its diagnostics, and producing them here as well would show
/// the reader every fault twice. A project without one has nothing, and this
/// is what it gets. So the feature turns itself on exactly where it is
/// needed, and needs no setting to say so.
pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut workspace::Workspace, _, cx| {
        let project = workspace.project().clone();
        let watching = Rc::new(RefCell::new(Watching::default()));

        // Every buffer already open, and every one opened later. Collected
        // first: reading the store borrows the context that watching one
        // needs mutably.
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
            move |_, _: &Check, _, cx| {
                let open: Vec<Entity<Buffer>> =
                    project.read(cx).buffer_store().read(cx).buffers().collect();
                for buffer in open {
                    read_soon(&project, &buffer, &watching, cx);
                }
            }
        });
    })
    .detach();
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
            if matches!(
                event,
                language::BufferEvent::Edited { .. } | language::BufferEvent::LanguageChanged(_)
            ) {
                read_soon(&project, &buffer, &watching, cx);
            }
        }
    })
    .detach();
    cx.observe_release(buffer, {
        let watching = watching.clone();
        let id = buffer.entity_id();
        move |_: &mut workspace::Workspace, _: &mut Buffer, _| {
            watching.borrow_mut().reading.remove(&id);
        }
    })
    .detach();

    // A buffer that is already open has not been edited, and would otherwise
    // wait for its first keystroke to say anything about a document that is
    // wrong now. One that is not XML yet is caught by `LanguageChanged`.
    read_soon(project, buffer, watching, cx);
}

fn read_soon(
    project: &Entity<Project>,
    buffer: &Entity<Buffer>,
    watching: &Rc<RefCell<Watching>>,
    cx: &mut gpui::Context<workspace::Workspace>,
) {
    // Asked here rather than before subscribing: a buffer's language is often
    // settled after the store reports it added, so a check made once at
    // subscription time misses the first document of a session entirely --
    // which is what the `LanguageChanged` arm above is for.
    if !is_xml(buffer, cx) {
        return;
    }
    let id = buffer.entity_id();
    let project = project.downgrade();
    let buffer = buffer.downgrade();
    let task = cx.spawn(async move |_, cx| {
        cx.background_executor().timer(SETTLE).await;
        if let Err(error) = read(&project, &buffer, cx).await {
            // A buffer closed mid-read, a workspace going away: none of it is
            // the reader's problem, and none of it clears what they were
            // shown.
            log::debug!("reading an XML buffer: {error:#}");
        }
    });
    watching.borrow_mut().reading.insert(id, task);
}

fn is_xml(buffer: &Entity<Buffer>, cx: &App) -> bool {
    buffer
        .read(cx)
        .language()
        .is_some_and(|language| language.name().as_ref() == XML_LANGUAGE)
}

/// What has to be known before anything can be read, gathered in one pass
/// over the application so the rest of the work can leave it alone.
struct About {
    path: PathBuf,
    text: String,
}

fn about(project: &Entity<Project>, buffer: &Entity<Buffer>, cx: &mut App) -> Option<About> {
    // Taken owned, and the borrow of the buffer let go, before anything is
    // asked of the application mutably below.
    let (path, text) = {
        let read = buffer.read(cx);
        (read.file()?.as_local()?.abs_path(cx), read.text())
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
    Some(About { path, text })
}

async fn read(
    project: &WeakEntity<Project>,
    buffer: &WeakEntity<Buffer>,
    cx: &mut AsyncApp,
) -> Result<()> {
    // A buffer a language server already covers, and one with no file behind
    // it, both end here: nothing is published, so nothing any other source
    // put on that file is disturbed either.
    let about = cx.update(|cx| {
        let project = project.upgrade()?;
        let buffer = buffer.upgrade()?;
        about(&project, &buffer, cx)
    });
    let Some(about) = about else {
        return Ok(());
    };
    let text = about.text;
    let found = cx.background_spawn(async move { faults_in(&text) }).await;
    show(project, about.path, found, cx)
}

/// Hands the editor what the parser said, replacing whatever this source said
/// last.
///
/// An empty list is not silence and is meant: a document whose fault the
/// reader has just fixed has to be told it is well-formed now, or the editor
/// keeps showing the fault.
fn show(
    project: &WeakEntity<Project>,
    path: PathBuf,
    found: Vec<lsp::Diagnostic>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let uri = lsp::Uri::from_file_path(&path)
        .map_err(|_| anyhow!("{} is not a path a uri can name", path.display()))?;
    project.update(cx, |project, cx| {
        project.lsp_store().update(cx, |lsp_store, cx| {
            let merged = lsp_store.merge_lsp_diagnostics(
                DiagnosticSourceKind::Pushed,
                vec![project::lsp_store::DocumentDiagnosticsUpdate {
                    diagnostics: lsp::PublishDiagnosticsParams {
                        uri,
                        diagnostics: found,
                        version: None,
                    },
                    result_id: None,
                    registration_id: None,
                    server_id: XML_SERVER_ID,
                    disk_based_sources: std::borrow::Cow::Borrowed(&[]),
                }],
                |_, _, _| false,
                cx,
            );
            if let Err(error) = merged {
                log::warn!(
                    "showing what the XML parser said about {}: {error:#}",
                    path.display()
                );
            }
        })
    })
}
