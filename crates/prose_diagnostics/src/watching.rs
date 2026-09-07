use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use collections::{HashMap, HashSet};
use gpui::{App, AppContext as _, Entity, Task, actions};
use language::{Buffer, DiagnosticSourceKind};
use project::Project;
use project::buffer_store::BufferStoreEvent;
use settings::Settings as _;

use crate::{ProseDiagnosticsSettings, prose_of, what_harper_said};

actions!(
    prose_diagnostics,
    [
        /// Reads the prose of every open document and comment for grammar and
        /// spelling, and shows what is worth reconsidering, without a language
        /// server.
        Check
    ]
);

/// The id this advice is filed under. There is no server behind it, in the way
/// `js_diagnostics` has none: the editor's diagnostics are keyed by server, so a
/// source that is not a server still needs an id. Chosen far above any a
/// running server would be assigned, and one apart from every other source that
/// is not a server.
const PROSE_SERVER_ID: language::LanguageServerId = language::LanguageServerId(usize::MAX - 1009);

/// Idle time after a save before the prose is read. Long enough to collapse the
/// burst a single save arrives as -- a format-on-save write, then the buffer's
/// own -- and no longer.
const SETTLE: Duration = Duration::from_millis(100);

/// The source of a project's prose advice: what it last said, and what it is
/// working on now.
#[derive(Default)]
struct Checking {
    /// Files that had advice last time. A file whose prose has been fixed has to
    /// be told so -- the editor keeps what it was last given until it is given
    /// something else, so a cleared file needs an empty report rather than
    /// silence.
    advised_on: HashSet<PathBuf>,
    /// The reading in flight for each file. Replaced, and so cancelled, when
    /// another starts on the same file: the old answer is about text the reader
    /// has since changed.
    running: HashMap<PathBuf, Task<()>>,
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut workspace::Workspace, _, cx| {
        let project = workspace.project().clone();
        let checking = Rc::new(RefCell::new(Checking::default()));

        // Every buffer already open, and every one opened later. Collected
        // first: reading the store borrows the context that watching one needs
        // mutably.
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
                    read_the_prose(&project, &buffer, &checking, cx);
                }
            }
        });
    })
    .detach();
}

/// Watches one buffer and reads its prose whenever there is new prose to read:
/// when it is saved, and when its language is settled, which for a file being
/// opened happens after the store reports it added. A check made only at
/// subscription time would miss the first file of a session entirely.
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
            read_the_prose(&project, &buffer, &checking, cx);
        }
    })
    .detach();
    read_the_prose(project, buffer, checking, cx);
}

fn read_the_prose(
    project: &Entity<Project>,
    buffer: &Entity<Buffer>,
    checking: &Rc<RefCell<Checking>>,
    cx: &mut gpui::Context<workspace::Workspace>,
) {
    let settings = ProseDiagnosticsSettings::get_global(cx);
    let read = buffer.read(cx);
    let language = read.language().cloned();
    let line_comments: Vec<String> = language
        .as_ref()
        .map(|language| {
            language
                .config()
                .line_comments
                .iter()
                .map(|marker| marker.to_string())
                .collect()
        })
        .unwrap_or_default();
    let name = language.as_ref().map(|language| language.name());
    let Some(prose) = prose_of(
        name.as_ref().map(|name| name.as_ref()),
        &line_comments,
        settings,
    ) else {
        return;
    };
    let Some(path) = read
        .file()
        .and_then(|file| file.as_local())
        .map(|file| file.abs_path(cx))
    else {
        return;
    };
    let text = read.text();

    let project = project.downgrade();
    let held = checking.clone();
    let task = cx.spawn({
        let path = path.clone();
        async move |_, cx| {
            cx.background_executor().timer(SETTLE).await;
            let said = cx
                .background_spawn(async move { what_harper_said(&text, &prose) })
                .await;

            // Nothing to say about a file nothing was said about before is
            // nothing to tell the editor. Nothing to say about a file that had
            // advice is an empty report, which is what takes the old advice
            // down -- and a different thing from silence.
            let advised_on_before = held.borrow().advised_on.contains(&path);
            if said.is_empty() && !advised_on_before {
                return;
            }
            if said.is_empty() {
                held.borrow_mut().advised_on.remove(&path);
            } else {
                held.borrow_mut().advised_on.insert(path.clone());
            }

            let Ok(uri) = lsp::Uri::from_file_path(&path) else {
                return;
            };
            let shown = project.update(cx, |project, cx| {
                project.lsp_store().update(cx, |lsp_store, cx| {
                    lsp_store.merge_lsp_diagnostics(
                        // Read from the buffer on a save rather than pushed by
                        // a server as the reader types, which is what this kind
                        // means -- and is how the editor knows to keep the
                        // advice until the next reading.
                        DiagnosticSourceKind::Other,
                        vec![project::lsp_store::DocumentDiagnosticsUpdate {
                            diagnostics: lsp::PublishDiagnosticsParams {
                                uri,
                                diagnostics: said,
                                version: None,
                            },
                            result_id: None,
                            registration_id: None,
                            server_id: PROSE_SERVER_ID,
                            disk_based_sources: std::borrow::Cow::Borrowed(&[]),
                        }],
                        |_, _, _| false,
                        cx,
                    )
                })
            });
            match shown {
                Ok(Ok(())) => {}
                Ok(Err(error)) => log::warn!(
                    "showing what harper said about {}: {error:#}",
                    path.display()
                ),
                // The project closed while the reading was in flight, which is
                // an ordinary way for a reading to end.
                Err(error) => log::debug!(
                    "the project went away before harper's advice about {}: {error:#}",
                    path.display()
                ),
            }
        }
    });
    checking.borrow_mut().running.insert(path, task);
}
