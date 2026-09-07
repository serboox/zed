use std::cell::RefCell;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use collections::{HashMap, HashSet};
use gpui::{App, AsyncApp, Entity, Task, WeakEntity, actions};
use language::{Buffer, DiagnosticSourceKind};
use project::Project;
use project::buffer_store::BufferStoreEvent;

use crate::fixing::Fixes;
use crate::what_ruff_reported;

actions!(
    python_diagnostics,
    [
        /// Asks ruff what is wrong with this project's Python and shows the
        /// answer, without a language server.
        Lint
    ]
);

/// The id these diagnostics are filed under. There is no server behind it, in
/// the way `cargo_diagnostics` has none: the editor's diagnostics are keyed
/// by server, so a source that is not a server still needs an id. Chosen far
/// above any a running server would be assigned, and one apart from every
/// other source that is not a server: the SQL validator, the Rust compiler
/// and the Go one.
pub(crate) const RUFF_SERVER_ID: language::LanguageServerId =
    language::LanguageServerId(usize::MAX - 1003);

/// Idle time after a save before ruff is asked. Long enough to collapse the
/// burst a single save arrives as -- format-on-save writes, then the buffer's
/// own -- and no longer. ruff over a whole project costs tens of
/// milliseconds, so unlike a compiler check there is nothing here worth
/// waiting to protect: a wait the reader can feel would cost more than the
/// work it saves.
const SETTLE: Duration = Duration::from_millis(50);

/// ruff's exit code when it could not run at all -- an unparseable
/// `ruff.toml`, an unknown rule in a `--select`. It writes the reason to the
/// terminal and leaves stdout empty, which is not the same as a clean
/// project, and must not be read as one.
const RUFF_COULD_NOT_RUN: i32 = 2;

/// The source of a project's lint diagnostics: what it last reported, and
/// what it is running now.
#[derive(Default)]
struct Watching {
    /// Files that had diagnostics last time. A file that has been fixed has
    /// to be told so -- the editor keeps what it was last given until it is
    /// given something else, so a cleared file needs an empty report rather
    /// than silence.
    reported_in: HashSet<PathBuf>,
    /// The lint in flight. Dropped, and so cancelled, when another starts.
    running: Option<Task<()>>,
}

pub(crate) fn init(cx: &mut App) {
    // One store for the whole application rather than one per workspace: the
    // source that reads it is registered once, and a file belongs to
    // whichever run last reported on it.
    let fixes = Arc::new(Mutex::new(Fixes::default()));
    crate::fixing::init(fixes.clone(), cx);
    cx.observe_new(move |workspace: &mut workspace::Workspace, _, cx| {
        let project = workspace.project().clone();
        let watching = Rc::new(RefCell::new(Watching::default()));
        let fixes = fixes.clone();

        // Every buffer already open, and every one opened later. Collected
        // first: reading the store borrows the context that watching one
        // needs mutably.
        let already_open: Vec<Entity<Buffer>> =
            project.read(cx).buffer_store().read(cx).buffers().collect();
        for buffer in already_open {
            watch_one(&project, &buffer, &watching, &fixes, cx);
        }
        let buffer_store = project.read(cx).buffer_store().clone();
        cx.subscribe(&buffer_store, {
            let project = project.clone();
            let watching = watching.clone();
            let fixes = fixes.clone();
            move |_: &mut workspace::Workspace, _, event, cx| {
                if let BufferStoreEvent::BufferAdded(buffer) = event {
                    watch_one(&project, buffer, &watching, &fixes, cx);
                }
            }
        })
        .detach();

        workspace.register_action({
            let project = project.clone();
            let watching = watching.clone();
            let fixes = fixes.clone();
            move |_, _: &Lint, _, cx| {
                ask_ruff(&project, &watching, &fixes, cx);
            }
        });
    })
    .detach();
}

/// Watches one buffer for saves, and asks ruff after each -- but only where
/// no language server is doing it already.
///
/// That condition is the whole design. A project with a Python server running
/// already has its diagnostics, and asking ruff as well would be paying twice
/// for one answer. A project without one has nothing, and this is what it
/// gets. So the feature turns itself on exactly where it is needed, and needs
/// no setting to say so.
fn watch_one(
    project: &Entity<Project>,
    buffer: &Entity<Buffer>,
    watching: &Rc<RefCell<Watching>>,
    fixes: &Arc<Mutex<Fixes>>,
    cx: &mut gpui::Context<workspace::Workspace>,
) {
    cx.subscribe(buffer, {
        let project = project.clone();
        let watching = watching.clone();
        let fixes = fixes.clone();
        move |_: &mut workspace::Workspace, buffer, event, cx| {
            if !matches!(event, language::BufferEvent::Saved) {
                return;
            }
            // Asked here rather than before subscribing: a buffer's language
            // is often settled after the store reports it added, so a check
            // made once at subscription time misses the first file of a
            // session entirely.
            if !is_python(&buffer, cx) {
                return;
            }
            if served_by_a_language_server(&project, &buffer, cx) {
                return;
            }
            ask_ruff(&project, &watching, &fixes, cx);
        }
    })
    .detach();
}

/// Whether a language server is already answering for this buffer.
///
/// Nested this way round because asking needs the buffer and the application
/// both, and the buffer's own update is what hands over one without holding
/// the other.
pub(crate) fn served_by_a_language_server(
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

pub(crate) fn is_python(buffer: &Entity<Buffer>, cx: &App) -> bool {
    buffer
        .read(cx)
        .language()
        .is_some_and(|language| language.name().as_ref() == "Python")
}

/// Starts a lint, cancelling whichever one was running. The previous answer
/// is about a file that has since changed, and finishing it would show the
/// reader a finding they have already fixed.
fn ask_ruff(
    project: &Entity<Project>,
    watching: &Rc<RefCell<Watching>>,
    fixes: &Arc<Mutex<Fixes>>,
    cx: &mut gpui::Context<workspace::Workspace>,
) {
    let Some(root) = a_python_project_root(project, cx) else {
        return;
    };
    let project = project.downgrade();
    let held = watching.clone();
    let fixes = fixes.clone();
    let task = cx.spawn(async move |_, cx| {
        cx.background_executor().timer(SETTLE).await;
        let output = match run_ruff(&root).await {
            Ok(output) => output,
            Err(error) => {
                // A project with no ruff anywhere is the ordinary case, not a
                // fault, so this stays out of the reader's way -- and either
                // way it is no reason to clear what they were already shown.
                log::debug!("asking ruff about {}: {error:#}", root.display());
                return;
            }
        };
        if let Err(error) = show_what_it_said(&project, &root, &output, &held, &fixes, cx).await {
            log::warn!("showing what ruff said: {error:#}");
        }
    });
    watching.borrow_mut().running = Some(task);
}

/// The root of the Python project this editor has open, or nothing where it
/// has not opened one. The first visible worktree ruff is willing to run in:
/// ruff itself is run there, and the report is about what is under it.
fn a_python_project_root(project: &Entity<Project>, cx: &App) -> Option<PathBuf> {
    project
        .read(cx)
        .visible_worktrees(cx)
        .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
        .next()
}

/// The virtualenv layout: `bin` everywhere but Windows, which uses `Scripts`.
const BINARY_DIR: &str = if cfg!(target_os = "windows") {
    "Scripts"
} else {
    "bin"
};

/// Where ruff is, or nothing at all.
///
/// A project's own virtualenv comes first, because a project that pins a ruff
/// version means that one and its rule set, and a newer one on PATH would
/// report rules the project has not adopted. `VIRTUAL_ENV` is next -- an
/// activated environment the editor was launched from -- and PATH last, which
/// is where a single system-wide install lives.
///
/// Nothing found is the ordinary case for a machine with no ruff, and the
/// caller's job is to be quiet about it.
fn where_ruff_is(
    root: &Path,
    virtual_env: Option<PathBuf>,
    exists: impl Fn(&Path) -> bool,
    on_path: impl Fn() -> Option<PathBuf>,
) -> Option<PathBuf> {
    let in_venv = [root.join(".venv"), root.join("venv")]
        .into_iter()
        .chain(virtual_env)
        .map(|venv| venv.join(BINARY_DIR).join(ruff_binary()));
    for candidate in in_venv {
        if exists(&candidate) {
            return Some(candidate);
        }
    }
    on_path()
}

fn ruff_binary() -> OsString {
    if cfg!(target_os = "windows") {
        OsString::from("ruff.exe")
    } else {
        OsString::from("ruff")
    }
}

async fn run_ruff(root: &Path) -> Result<String> {
    let ruff = where_ruff_is(
        root,
        std::env::var_os("VIRTUAL_ENV").map(PathBuf::from),
        |path| path.is_file(),
        || which::which("ruff").ok(),
    )
    .context("ruff is not in this project's virtualenv or on PATH")?;
    let asked = smol::process::Command::new(&ruff)
        .current_dir(root)
        .args(["check", "--output-format", "json", "--quiet", "."])
        // Kept off the terminal: the reader asked for diagnostics, not for a
        // lint log, and ruff's own errors go to stderr.
        .stderr(std::process::Stdio::null())
        .output()
        .await
        .with_context(|| format!("running {} in {}", ruff.display(), root.display()))?;
    if asked.status.code() == Some(RUFF_COULD_NOT_RUN) {
        return Err(anyhow!("ruff could not run in {}", root.display()));
    }
    String::from_utf8(asked.stdout).context("ruff's report is not text")
}

/// What to tell the editor, and what to remember having told it.
///
/// Two halves, and the second is the one that is easy to forget: what the
/// editor was last given it keeps until it is given something else. A file
/// whose finding the reader has just fixed reports nothing, and reporting
/// nothing about it leaves the old finding on screen -- so it needs an empty
/// report, which is a different thing from silence.
fn what_to_tell(
    reported: Vec<crate::Reported>,
    reported_in_last_time: &HashSet<PathBuf>,
) -> (Vec<(PathBuf, Vec<lsp::Diagnostic>)>, HashSet<PathBuf>) {
    let mut by_file: HashMap<PathBuf, Vec<lsp::Diagnostic>> = HashMap::default();
    for one in reported {
        by_file.entry(one.path).or_default().push(one.diagnostic);
    }
    let cleared: Vec<PathBuf> = reported_in_last_time
        .iter()
        .filter(|path| !by_file.contains_key(*path))
        .cloned()
        .collect();
    let now: HashSet<PathBuf> = by_file.keys().cloned().collect();
    let telling = by_file
        .into_iter()
        .chain(cleared.into_iter().map(|path| (path, Vec::new())))
        .collect();
    (telling, now)
}

/// Files ruff complained about, and files it stopped complaining about, both
/// told to the editor. The second half is the one that is easy to forget:
/// what the editor was last given it keeps.
async fn show_what_it_said(
    project: &WeakEntity<Project>,
    root: &Path,
    output: &str,
    watching: &Rc<RefCell<Watching>>,
    fixes: &Arc<Mutex<Fixes>>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let read = |path: &Path| std::fs::read_to_string(path).ok();
    let reported = what_ruff_reported(output, root, read);
    match fixes.lock() {
        Ok(mut fixes) => fixes.remember(&reported),
        // A poisoned store means a panic while it was held. The diagnostics
        // are still worth showing; only the fixes are lost.
        Err(error) => log::warn!("keeping ruff's fixes: {error}"),
    }
    let (telling, now_reported_in) = what_to_tell(reported, &watching.borrow().reported_in);
    watching.borrow_mut().reported_in = now_reported_in;

    project.update(cx, |project, cx| {
        project.lsp_store().update(cx, |lsp_store, cx| {
            for (path, diagnostics) in telling {
                let Ok(uri) = lsp::Uri::from_file_path(&path) else {
                    continue;
                };
                let merged = lsp_store.merge_lsp_diagnostics(
                    // ruff is a run over the files on disk, not a live
                    // analysis, which is what this kind means -- and it is
                    // how the editor knows to keep them until the next run
                    // rather than expect them refreshed as the reader types.
                    DiagnosticSourceKind::Other,
                    vec![project::lsp_store::DocumentDiagnosticsUpdate {
                        diagnostics: lsp::PublishDiagnosticsParams {
                            uri,
                            diagnostics,
                            version: None,
                        },
                        result_id: None,
                        registration_id: None,
                        server_id: RUFF_SERVER_ID,
                        disk_based_sources: std::borrow::Cow::Borrowed(&[]),
                    }],
                    |_, _, _| false,
                    cx,
                );
                if let Err(error) = merged {
                    log::warn!("showing ruff's report for {}: {error:#}", path.display());
                }
            }
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Reported;

    fn one_at(path: &str) -> Reported {
        Reported {
            path: PathBuf::from(path),
            fixes: Vec::new(),
            diagnostic: lsp::Diagnostic {
                message: "`os` imported but unused".to_string(),
                ..Default::default()
            },
        }
    }

    /// A file that reported last time and does not now needs an empty report
    /// rather than silence: the editor keeps what it was last given, so
    /// saying nothing leaves a finding the reader has already fixed on the
    /// screen.
    #[test]
    fn a_file_that_stopped_complaining_is_told_so_rather_than_left_alone() {
        let last_time: HashSet<PathBuf> = ["a.py", "b.py"].into_iter().map(PathBuf::from).collect();
        let (telling, now) = what_to_tell(vec![one_at("a.py")], &last_time);

        let mut telling: Vec<(String, usize)> = telling
            .into_iter()
            .map(|(path, diagnostics)| (path.display().to_string(), diagnostics.len()))
            .collect();
        telling.sort();
        assert_eq!(
            telling,
            vec![("a.py".to_string(), 1), ("b.py".to_string(), 0)],
            "`b.py` is told it is clean; saying nothing would leave its finding up"
        );
        assert_eq!(
            now,
            ["a.py"]
                .into_iter()
                .map(PathBuf::from)
                .collect::<HashSet<_>>(),
            "and only the file that still reports is remembered"
        );
    }

    /// Several findings in one file arrive as one report for that file: the
    /// editor replaces a file's whole set each time, so sending them one at a
    /// time would leave only the last.
    #[test]
    fn every_finding_in_one_file_is_told_at_once() {
        let (telling, _) = what_to_tell(
            vec![one_at("a.py"), one_at("a.py"), one_at("b.py")],
            &HashSet::default(),
        );
        let mut counted: Vec<(String, usize)> = telling
            .into_iter()
            .map(|(path, diagnostics)| (path.display().to_string(), diagnostics.len()))
            .collect();
        counted.sort();
        assert_eq!(
            counted,
            vec![("a.py".to_string(), 2), ("b.py".to_string(), 1)]
        );
    }

    /// Nothing to say and nothing said last time is nothing to tell -- not an
    /// empty report for every file in the project.
    #[test]
    fn a_clean_project_that_was_always_clean_is_told_nothing() {
        let (telling, now) = what_to_tell(Vec::new(), &HashSet::default());
        assert!(telling.is_empty());
        assert!(now.is_empty());
    }

    /// A machine with no ruff anywhere is the ordinary case, and it has to be
    /// harmless: nothing found, so nothing run and nothing reported.
    #[test]
    fn no_ruff_anywhere_is_nothing_found_rather_than_a_fault() {
        let found = where_ruff_is(Path::new("/project"), None, |_| false, || None);
        assert_eq!(found, None);
    }

    /// The project's own virtualenv wins over PATH: a project that pins a
    /// ruff version means that one, and a newer one on PATH would report
    /// rules the project has not adopted.
    #[test]
    fn the_projects_own_virtualenv_is_preferred_to_path() {
        let in_venv = Path::new("/project")
            .join(".venv")
            .join(BINARY_DIR)
            .join(ruff_binary());
        let found = where_ruff_is(
            Path::new("/project"),
            Some(PathBuf::from("/elsewhere/env")),
            |path| path == in_venv,
            || Some(PathBuf::from("/usr/bin/ruff")),
        );
        assert_eq!(found, Some(in_venv));
    }

    /// An activated environment the editor was launched from is used where
    /// the project has no virtualenv of its own, and still before PATH.
    #[test]
    fn an_activated_environment_is_used_before_path() {
        let activated = Path::new("/elsewhere/env")
            .join(BINARY_DIR)
            .join(ruff_binary());
        let found = where_ruff_is(
            Path::new("/project"),
            Some(PathBuf::from("/elsewhere/env")),
            |path| path == activated,
            || Some(PathBuf::from("/usr/bin/ruff")),
        );
        assert_eq!(found, Some(activated));
    }

    /// A single system-wide install is what most machines have, and it is
    /// found once no virtualenv holds one.
    #[test]
    fn a_ruff_on_path_is_used_where_no_virtualenv_holds_one() {
        let found = where_ruff_is(
            Path::new("/project"),
            None,
            |_| false,
            || Some(PathBuf::from("/usr/bin/ruff")),
        );
        assert_eq!(found, Some(PathBuf::from("/usr/bin/ruff")));
    }
}
