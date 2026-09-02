use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use collections::{HashMap, HashSet};
use gpui::{App, AsyncApp, Entity, Task, WeakEntity, actions};
use language::{Buffer, DiagnosticSourceKind};
use project::Project;
use project::buffer_store::BufferStoreEvent;

use crate::what_the_compiler_reported;

actions!(
    cargo_diagnostics,
    [
        /// Asks the compiler what is wrong with this project and shows the
        /// answer, without a language server.
        Check
    ]
);

/// The id these diagnostics are filed under. There is no server behind it, in
/// the way `db_client_ui`'s SQL validator has none: the editor's diagnostics
/// are keyed by server, so a source that is not a server still needs an id.
/// Chosen far above any a running server would be assigned, and one apart
/// from the SQL validator's.
const CARGO_SERVER_ID: language::LanguageServerId = language::LanguageServerId(usize::MAX - 1001);

/// Idle time after a save before the compiler is asked. A save often comes in
/// a burst -- format-on-save writes, then a multi-file rename -- and each one
/// would otherwise start a check that the next one makes stale. `cargo check`
/// on a large project costs minutes, so starting one to throw it away is the
/// most expensive mistake this file could make.
const SETTLE: Duration = Duration::from_millis(400);

/// The source of a project's compiler diagnostics: what it last reported, and
/// what it is running now.
#[derive(Default)]
struct Watching {
    /// Files that had diagnostics last time. A file that has been fixed has
    /// to be told so -- the editor keeps what it was last given until it is
    /// given something else, so a cleared file needs an empty report rather
    /// than silence.
    reported_in: HashSet<PathBuf>,
    /// The check in flight. Dropped, and so cancelled, when another starts.
    running: Option<Task<()>>,
}

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
                ask_the_compiler(&project, &watching, cx);
            }
        });
    })
    .detach();
}

/// Watches one buffer for saves, and asks the compiler after each -- but only
/// where no language server is doing it already.
///
/// That condition is the whole design. A project with rust-analyzer running
/// already has its diagnostics, and asking the compiler as well would be
/// paying twice for one answer. A project without one has nothing, and this
/// is what it gets. So the feature turns itself on exactly where it is
/// needed, and needs no setting to say so.
fn watch_one(
    project: &Entity<Project>,
    buffer: &Entity<Buffer>,
    watching: &Rc<RefCell<Watching>>,
    cx: &mut gpui::Context<workspace::Workspace>,
) {
    if !is_rust(buffer, cx) {
        return;
    }
    cx.subscribe(buffer, {
        let project = project.clone();
        let watching = watching.clone();
        move |_: &mut workspace::Workspace, buffer, event, cx| {
            if !matches!(event, language::BufferEvent::Saved) {
                return;
            }
            let lsp_store = project.read(cx).lsp_store();
            // Nested this way round because asking needs the buffer and the
            // application both, and the buffer's own update is what hands
            // over one without holding the other -- the same shape
            // `edit_prediction_cli` uses for the same call.
            let served = buffer.update(cx, |buffer, cx| {
                lsp_store.update(cx, |lsp_store, cx| {
                    !lsp_store
                        .language_servers_for_local_buffer(buffer, cx)
                        .is_empty()
                })
            });
            if served {
                return;
            }
            ask_the_compiler(&project, &watching, cx);
        }
    })
    .detach();
}

fn is_rust(buffer: &Entity<Buffer>, cx: &App) -> bool {
    buffer
        .read(cx)
        .language()
        .is_some_and(|language| language.name().as_ref() == "Rust")
}

/// Starts a check, cancelling whichever one was running. The previous answer
/// is about a file that has since changed, and finishing it would show the
/// reader a diagnostic they have already fixed.
fn ask_the_compiler(
    project: &Entity<Project>,
    watching: &Rc<RefCell<Watching>>,
    cx: &mut gpui::Context<workspace::Workspace>,
) {
    let Some(root) = a_cargo_project_root(project, cx) else {
        return;
    };
    let project = project.downgrade();
    let held = watching.clone();
    let task = cx.spawn(async move |_, cx| {
        cx.background_executor().timer(SETTLE).await;
        let asked = run_cargo_check(&root).await;
        let output = match asked {
            Ok(output) => output,
            Err(error) => {
                // A missing cargo, or a manifest that will not parse, is not
                // something to keep quiet about -- but it is also not a
                // reason to clear what the reader was already shown.
                log::warn!("asking the compiler about {}: {error:#}", root.display());
                return;
            }
        };
        if let Err(error) = show_what_it_said(&project, &root, &output, &held, cx).await {
            log::warn!("showing what the compiler said: {error:#}");
        }
    });
    watching.borrow_mut().running = Some(task);
}

/// The root of the cargo project this editor has open, or nothing where it has
/// not opened one. The first visible worktree with a `Cargo.toml` at its top:
/// cargo itself is run there, and every path in its report is relative to it.
fn a_cargo_project_root(project: &Entity<Project>, cx: &App) -> Option<PathBuf> {
    project
        .read(cx)
        .visible_worktrees(cx)
        .filter_map(|worktree| {
            let worktree = worktree.read(cx);
            let root = worktree.abs_path().to_path_buf();
            root.join("Cargo.toml").is_file().then_some(root)
        })
        .next()
}

async fn run_cargo_check(root: &Path) -> Result<String> {
    let cargo = which::which("cargo").context("cargo is not on PATH")?;
    let asked = smol::process::Command::new(&cargo)
        .current_dir(root)
        .args([
            "check",
            "--workspace",
            "--all-targets",
            "--message-format=json",
        ])
        // Kept off the terminal: the reader asked for diagnostics, not for a
        // build log, and cargo's progress goes to stderr.
        .stderr(std::process::Stdio::null())
        .output()
        .await
        .with_context(|| format!("running {} in {}", cargo.display(), root.display()))?;
    String::from_utf8(asked.stdout).context("cargo's report is not text")
}

/// What to tell the editor, and what to remember having told it.
///
/// Two halves, and the second is the one that is easy to forget: what the
/// editor was last given it keeps until it is given something else. A file
/// whose error the reader has just fixed reports nothing, and reporting
/// nothing about it leaves the old error on screen -- so it needs an empty
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

/// Files the compiler complained about, and files it stopped complaining
/// about, both told to the editor. The second half is the one that is easy to
/// forget: what the editor was last given it keeps.
async fn show_what_it_said(
    project: &WeakEntity<Project>,
    root: &Path,
    output: &str,
    watching: &Rc<RefCell<Watching>>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let read = |path: &Path| std::fs::read_to_string(path).ok();
    let reported = what_the_compiler_reported(output, root, read);
    let (telling, now_reported_in) = what_to_tell(reported, &watching.borrow().reported_in);
    watching.borrow_mut().reported_in = now_reported_in;

    project.update(cx, |project, cx| {
        project.lsp_store().update(cx, |lsp_store, cx| {
            for (path, diagnostics) in telling {
                let Ok(uri) = lsp::Uri::from_file_path(&path) else {
                    continue;
                };
                // `merge_lsp_diagnostics` and not `update_diagnostics`: the
                // latter is a test-only helper, and reaching for it would
                // have made this crate need `project/test-support` to
                // compile at all.
                let merged = lsp_store.merge_lsp_diagnostics(
                    // The compiler is a build, not a live analysis, which is
                    // what this kind means -- and it is how the editor knows
                    // to keep them until the next build rather than expect
                    // them refreshed as the reader types.
                    DiagnosticSourceKind::Other,
                    vec![project::lsp_store::DocumentDiagnosticsUpdate {
                        diagnostics: lsp::PublishDiagnosticsParams {
                            uri,
                            diagnostics,
                            version: None,
                        },
                        result_id: None,
                        registration_id: None,
                        server_id: CARGO_SERVER_ID,
                        disk_based_sources: std::borrow::Cow::Borrowed(&[]),
                    }],
                    |_, _, _| false,
                    cx,
                );
                if let Err(error) = merged {
                    log::warn!(
                        "showing the compiler's report for {}: {error:#}",
                        path.display()
                    );
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
            diagnostic: lsp::Diagnostic {
                message: "mismatched types".to_string(),
                ..Default::default()
            },
        }
    }

    /// A file that reported last time and does not now needs an empty report
    /// rather than silence: the editor keeps what it was last given, so
    /// saying nothing leaves an error the reader has already fixed on the
    /// screen.
    #[test]
    fn a_file_that_stopped_complaining_is_told_so_rather_than_left_alone() {
        let last_time: HashSet<PathBuf> = ["a.rs", "b.rs"].into_iter().map(PathBuf::from).collect();
        let (telling, now) = what_to_tell(vec![one_at("a.rs")], &last_time);

        let mut telling: Vec<(String, usize)> = telling
            .into_iter()
            .map(|(path, diagnostics)| (path.display().to_string(), diagnostics.len()))
            .collect();
        telling.sort();
        assert_eq!(
            telling,
            vec![("a.rs".to_string(), 1), ("b.rs".to_string(), 0)],
            "`b.rs` is told it is clean; saying nothing would leave its error up"
        );
        assert_eq!(
            now,
            ["a.rs"]
                .into_iter()
                .map(PathBuf::from)
                .collect::<HashSet<_>>(),
            "and only the file that still reports is remembered"
        );
    }

    /// Several diagnostics in one file arrive as one report for that file:
    /// the editor replaces a file's whole set each time, so sending them one
    /// at a time would leave only the last.
    #[test]
    fn every_diagnostic_in_one_file_is_told_at_once() {
        let (telling, _) = what_to_tell(
            vec![one_at("a.rs"), one_at("a.rs"), one_at("b.rs")],
            &HashSet::default(),
        );
        let mut counted: Vec<(String, usize)> = telling
            .into_iter()
            .map(|(path, diagnostics)| (path.display().to_string(), diagnostics.len()))
            .collect();
        counted.sort();
        assert_eq!(
            counted,
            vec![("a.rs".to_string(), 2), ("b.rs".to_string(), 1)]
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
}
