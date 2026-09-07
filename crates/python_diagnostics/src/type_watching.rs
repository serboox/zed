use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use anyhow::Result;
use collections::HashMap;
use gpui::{App, AppContext as _, AsyncApp, Entity, Task, WeakEntity, actions};
use language::{Buffer, DiagnosticSourceKind};
use project::Project;
use project::buffer_store::BufferStoreEvent;

use crate::watching::{is_python, served_by_a_language_server};

actions!(
    python_diagnostics,
    [
        /// Asks `ty` what is wrong with the types in the Python files that are
        /// open, and shows the answer, without a language server.
        CheckTypes
    ]
);

/// The id these diagnostics are filed under. There is no server behind it: the
/// editor's diagnostics are keyed by server, so a source that is not a server
/// still needs an id. One apart from the linter's, so that a type error and a
/// lint finding in the same file replace only their own kind.
const TY_SERVER_ID: language::LanguageServerId = language::LanguageServerId(usize::MAX - 1014);

/// Idle time after a save before `ty` is asked. Long enough to collapse the
/// burst a single save arrives as -- format-on-save writes, then the buffer's
/// own -- and no longer.
const SETTLE: Duration = Duration::from_millis(50);

/// The checks in flight, one per file. A second check of the same file drops
/// the first, and so cancels it: its answer is about text that has since
/// changed. Checks of different files do not cancel each other, because each
/// one only ever reports about its own file.
#[derive(Default)]
struct Checking {
    running: HashMap<PathBuf, Task<()>>,
}

pub(crate) fn init(cx: &mut App) {
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
            move |_, _: &CheckTypes, _, cx| {
                let open: Vec<Entity<Buffer>> =
                    project.read(cx).buffer_store().read(cx).buffers().collect();
                for buffer in open {
                    ask_ty(&project, &buffer, &checking, cx);
                }
            }
        });
    })
    .detach();
}

/// Watches one buffer for saves, and asks `ty` about it after each.
///
/// A save rather than an idle pause while typing, on the measured cost. The
/// first check of a session takes around 700 ms, because it is the one that
/// builds the project's file index and infers the standard library, and it
/// holds the single lock hover and completion also read the database through.
/// Every check after that costs a fraction of a millisecond and adds no
/// memory at all, so the trigger is not about the steady state -- it is about
/// not putting that first check in the middle of a keystroke. A save is also
/// when the linter runs, so the reader gets both reports at once rather than
/// in two waves. `CheckTypes` is there for the reader who wants one now.
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
            if !matches!(event, language::BufferEvent::Saved) {
                return;
            }
            ask_ty(&project, &buffer, &checking, cx);
        }
    })
    .detach();
}

/// Whether this source answers for a buffer at all.
///
/// A project with a Python language server running already has its type
/// errors, and asking `ty` as well would show the reader the same problem
/// twice. A project without one has nothing, and this is what it gets. So the
/// feature turns itself on exactly where it is needed, and needs no setting to
/// say so.
///
/// The language is asked here rather than once at subscription time: a
/// buffer's language is often settled after the store reports it added, so a
/// check made then misses the first file of a session entirely.
fn answers_for(project: &Entity<Project>, buffer: &Entity<Buffer>, cx: &mut App) -> bool {
    is_python(buffer, cx) && !served_by_a_language_server(project, buffer, cx)
}

/// Asks `ty` about one file, and shows what it said.
fn ask_ty(
    project: &Entity<Project>,
    buffer: &Entity<Buffer>,
    checking: &Rc<RefCell<Checking>>,
    cx: &mut gpui::Context<workspace::Workspace>,
) {
    if !answers_for(project, buffer, cx) {
        return;
    }
    let Some(checker) = python_types::type_checker(cx) else {
        return;
    };
    let Some(path) = buffer
        .read(cx)
        .file()
        .and_then(|file| file.as_local())
        .map(|file| file.abs_path(cx))
    else {
        return;
    };
    let Some(root) = root_for(project, &path, cx) else {
        return;
    };
    let text = buffer.read(cx).snapshot().text();
    let weak_project = project.downgrade();
    let task = cx.spawn({
        let path = path.clone();
        async move |_, cx| {
            cx.background_executor().timer(SETTLE).await;
            let found = cx
                .background_spawn({
                    let path = path.clone();
                    async move { checker.check(&root, &path, &text) }
                })
                .await;
            if let Err(error) = show_what_it_said(&weak_project, &path, found, cx).await {
                log::warn!("showing what ty said about {}: {error:#}", path.display());
            }
        }
    });
    checking.borrow_mut().running.insert(path, task);
}

/// The project root `ty` should read this file in: the visible worktree that
/// holds it, and the file's own directory where none does -- which is what an
/// editor opened on a single file has.
fn root_for(project: &Entity<Project>, path: &Path, cx: &App) -> Option<PathBuf> {
    project
        .read(cx)
        .visible_worktrees(cx)
        .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
        .find(|root| path.starts_with(root))
        .or_else(|| path.parent().map(Path::to_path_buf))
}

/// Tells the editor what `ty` found in one file, including when it found
/// nothing.
///
/// The empty case is the one that is easy to forget: what the editor was last
/// given it keeps until it is given something else, so a file whose error the
/// reader has just fixed needs an empty report rather than silence.
async fn show_what_it_said(
    project: &WeakEntity<Project>,
    path: &Path,
    found: Vec<lsp::Diagnostic>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let Ok(uri) = lsp::Uri::from_file_path(path) else {
        return Ok(());
    };
    project.update(cx, |project, cx| {
        project.lsp_store().update(cx, |lsp_store, cx| {
            let merged = lsp_store.merge_lsp_diagnostics(
                // `ty` reads the text of the open buffer rather than the file
                // on disk, which is what this kind means.
                DiagnosticSourceKind::Pushed,
                vec![project::lsp_store::DocumentDiagnosticsUpdate {
                    diagnostics: lsp::PublishDiagnosticsParams {
                        uri,
                        diagnostics: found,
                        version: None,
                    },
                    result_id: None,
                    registration_id: None,
                    server_id: TY_SERVER_ID,
                    disk_based_sources: std::borrow::Cow::Borrowed(&[]),
                }],
                |_, _, _| false,
                cx,
            );
            if let Err(error) = merged {
                log::warn!("showing ty's report for {}: {error:#}", path.display());
            }
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use futures::StreamExt as _;
    use gpui::TestAppContext;
    use language::{FakeLspAdapter, Language, LanguageConfig, LanguageMatcher};
    use settings::SettingsStore;
    use std::sync::Arc;
    use workspace::MultiWorkspace;

    /// A file with a genuine type error in it: a function that takes an `int`,
    /// handed a string.
    const WRONG: &str = "def double(value: int) -> int:\n    return value * 2\n\n\nanswer = double(\"twenty-one\")\n";

    /// The same file with the mistake taken out.
    const RIGHT: &str =
        "def double(value: int) -> int:\n    return value * 2\n\n\nanswer = double(21)\n";

    fn python_lang() -> Arc<Language> {
        Arc::new(Language::new(
            LanguageConfig {
                name: "Python".into(),
                matcher: LanguageMatcher {
                    path_suffixes: vec!["py".to_string()],
                    ..Default::default()
                },
                ..Default::default()
            },
            None,
        ))
    }

    /// A project that exists twice at one path: the editor's own in-memory
    /// filesystem, which the buffer and the worktree are read through, and a
    /// real directory, because `ty` walks the disk to find a project at all
    /// and reads the standard library from it.
    struct Opened {
        project: Entity<Project>,
        buffer: Entity<Buffer>,
        /// Every fake language server that starts, where one was registered.
        /// Held rather than dropped: the adapter hands its server over this
        /// channel, and a dropped receiver is a server that never arrives.
        servers: Option<futures::channel::mpsc::UnboundedReceiver<lsp::FakeLanguageServer>>,
        // Held so that the directory on disk, the buffer's registration with
        // the language server store, and the window whose workspace this
        // source subscribed to all outlive the test.
        _at: tempfile::TempDir,
        _lsp_handle: project::lsp_store::OpenLspBufferHandle,
        _window: gpui::WindowHandle<MultiWorkspace>,
    }

    /// Opens one Python file, with a language server registered for Python or
    /// without one. The server is registered before the buffer is opened: a
    /// buffer only picks up a server that is already registered by the time it
    /// is opened, so this order matters.
    async fn open_a_python_file(
        source: &str,
        with_a_language_server: bool,
        cx: &mut TestAppContext,
    ) -> Opened {
        let at = tempfile::tempdir().expect("a directory to put a project in");
        std::fs::write(at.path().join("checked.py"), source).expect("a file on disk");
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            at.path(),
            serde_json::json!({ "checked.py": source.to_string() }),
        )
        .await;
        let project = Project::test(fs, [at.path()], cx).await;

        let language_registry = project.read_with(cx, |project, _| project.languages().clone());
        language_registry.add(python_lang());
        let servers = with_a_language_server
            .then(|| language_registry.register_fake_lsp("Python", FakeLspAdapter::default()));

        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let (buffer, lsp_handle) = project
            .update(cx, |project, cx| {
                project.open_local_buffer_with_lsp(at.path().join("checked.py"), cx)
            })
            .await
            .expect("the file opens");
        cx.run_until_parked();

        Opened {
            project,
            buffer,
            servers,
            _at: at,
            _lsp_handle: lsp_handle,
            _window: window,
        }
    }

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            editor::init(cx);
            python_types::init(cx);
            super::init(cx);
        });
    }

    /// What `ty` put on the buffer. Filtered by source, because the linter
    /// watches the same saves and may be installed on the machine the test
    /// runs on.
    fn what_ty_put_on(opened: &Opened, cx: &mut TestAppContext) -> Vec<language::Diagnostic> {
        opened.buffer.read_with(cx, |buffer, _| {
            let snapshot = buffer.snapshot();
            let whole_file = 0..snapshot.len();
            snapshot
                .diagnostics_in_range::<usize, usize>(whole_file, false)
                .filter(|entry| entry.diagnostic.source.as_deref() == Some("ty"))
                .map(|entry| entry.diagnostic.clone())
                .collect()
        })
    }

    async fn save(opened: &Opened, cx: &mut TestAppContext) {
        opened
            .project
            .update(cx, |project, cx| {
                project.save_buffer(opened.buffer.clone(), cx)
            })
            .await
            .expect("the buffer saves");
        cx.executor().advance_clock(SETTLE * 4);
        cx.run_until_parked();
    }

    /// The whole of what a reader sees: they save a file with a type error in
    /// it, and the error appears on the line it is about, with no language
    /// server anywhere.
    #[gpui::test]
    async fn saving_a_file_with_a_type_error_shows_it(cx: &mut TestAppContext) {
        init_test(cx);
        let opened = open_a_python_file(WRONG, false, cx).await;
        save(&opened, cx).await;

        let found = what_ty_put_on(&opened, cx);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(
            found[0].severity,
            lsp::DiagnosticSeverity::ERROR,
            "{found:?}"
        );
        assert_eq!(
            found[0].code,
            Some(lsp::NumberOrString::String(
                "invalid-argument-type".to_string()
            )),
            "{found:?}"
        );
    }

    /// A file the reader has just fixed loses its underlining, rather than
    /// keeping what it said before: the editor holds the last report it was
    /// given until it is given another one.
    #[gpui::test]
    async fn fixing_the_error_takes_the_report_off_again(cx: &mut TestAppContext) {
        init_test(cx);
        let opened = open_a_python_file(WRONG, false, cx).await;
        save(&opened, cx).await;
        assert_eq!(what_ty_put_on(&opened, cx).len(), 1);

        opened.buffer.update(cx, |buffer, cx| {
            let whole_file = 0..buffer.len();
            buffer.edit([(whole_file, RIGHT)], None, cx);
        });
        save(&opened, cx).await;

        let found = what_ty_put_on(&opened, cx);
        assert!(found.is_empty(), "{found:?}");
    }

    /// A buffer a language server serves gets nothing from this source. The
    /// server already reports the file's type errors, and a second source
    /// reporting them would show the reader the same problem twice.
    #[gpui::test]
    async fn a_buffer_a_language_server_serves_gets_nothing(cx: &mut TestAppContext) {
        init_test(cx);
        let mut opened = open_a_python_file(WRONG, true, cx).await;
        // Started by hand, because that is the only way a server runs in this
        // editor: opening a Python file brings nothing up on its own.
        let serve_this = vec![opened.buffer.clone()];
        opened.project.update(cx, |project, cx| {
            project.restart_language_servers_for_buffers(
                serve_this,
                collections::HashSet::default(),
                true,
                cx,
            );
        });
        opened
            .servers
            .as_mut()
            .expect("a language server was registered")
            .next()
            .await
            .expect("the language server starts");
        cx.run_until_parked();
        // Asserted rather than assumed: with no server actually serving the
        // buffer, the rest of this test would pass for the wrong reason.
        let serving =
            cx.update(|cx| served_by_a_language_server(&opened.project, &opened.buffer, cx));
        assert!(serving, "the language server should be serving this buffer");

        save(&opened, cx).await;

        let found = what_ty_put_on(&opened, cx);
        assert!(
            found.is_empty(),
            "the server serves this buffer, so ty must stay out of it: {found:?}"
        );
    }
}
