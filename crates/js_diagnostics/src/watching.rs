use std::cell::RefCell;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use collections::{HashMap, HashSet};
use gpui::{App, AsyncApp, Entity, Task, WeakEntity, actions};
use language::{Buffer, DiagnosticSourceKind};
use project::Project;
use project::buffer_store::BufferStoreEvent;

use crate::what_oxlint_reported;

actions!(
    js_diagnostics,
    [
        /// Asks oxlint what is wrong with this project's JavaScript and
        /// TypeScript and shows the answer, without a language server.
        Lint
    ]
);

/// The id these diagnostics are filed under. There is no server behind it, in
/// the way `cargo_diagnostics` has none: the editor's diagnostics are keyed
/// by server, so a source that is not a server still needs an id. Chosen far
/// above any a running server would be assigned, and one apart from every
/// other source that is not a server.
const OXLINT_SERVER_ID: language::LanguageServerId = language::LanguageServerId(usize::MAX - 1007);

/// Idle time after a save before oxlint is asked. Long enough to collapse the
/// burst a single save arrives as -- format-on-save writes, then the buffer's
/// own -- and no longer. oxlint over a whole project costs tens of
/// milliseconds, so unlike a compiler check there is nothing here worth
/// waiting to protect.
const SETTLE: Duration = Duration::from_millis(50);

/// The languages this asks about. oxlint reads all three, and Zed names them
/// separately.
const LINTED_LANGUAGES: [&str; 3] = ["JavaScript", "TypeScript", "TSX"];

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
            move |_, _: &Lint, _, cx| {
                ask_oxlint(&project, &watching, cx);
            }
        });
    })
    .detach();
}

/// Watches one buffer for saves, and asks oxlint after each -- but only where
/// no language server is doing it already.
///
/// That condition is the whole design. A project with a JavaScript server
/// running already has its diagnostics, and asking oxlint as well would be
/// paying twice for one answer. A project without one has nothing, and this
/// is what it gets. So the feature turns itself on exactly where it is
/// needed, and needs no setting to say so.
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
            if !matches!(event, language::BufferEvent::Saved) {
                return;
            }
            // Asked here rather than before subscribing: a buffer's language
            // is often settled after the store reports it added, so a check
            // made once at subscription time misses the first file of a
            // session entirely.
            if !is_linted(&buffer, cx) {
                return;
            }
            let lsp_store = project.read(cx).lsp_store();
            // Nested this way round because asking needs the buffer and the
            // application both, and the buffer's own update is what hands
            // over one without holding the other.
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
            ask_oxlint(&project, &watching, cx);
        }
    })
    .detach();
}

fn is_linted(buffer: &Entity<Buffer>, cx: &App) -> bool {
    buffer
        .read(cx)
        .language()
        .is_some_and(|language| LINTED_LANGUAGES.contains(&language.name().as_ref()))
}

/// Starts a lint, cancelling whichever one was running. The previous answer
/// is about a file that has since changed, and finishing it would show the
/// reader a finding they have already fixed.
fn ask_oxlint(
    project: &Entity<Project>,
    watching: &Rc<RefCell<Watching>>,
    cx: &mut gpui::Context<workspace::Workspace>,
) {
    let Some(root) = a_project_root(project, cx) else {
        return;
    };
    let project = project.downgrade();
    let held = watching.clone();
    let task = cx.spawn(async move |_, cx| {
        cx.background_executor().timer(SETTLE).await;
        let output = match run_oxlint(&root).await {
            Ok(output) => output,
            Err(error) => {
                // A project with no oxlint anywhere is the ordinary case, not
                // a fault, so this stays out of the reader's way -- and
                // either way it is no reason to clear what they were already
                // shown.
                log::debug!("asking oxlint about {}: {error:#}", root.display());
                return;
            }
        };
        if let Err(error) = show_what_it_said(&project, &root, &output, &held, cx).await {
            log::warn!("showing what oxlint said: {error:#}");
        }
    });
    watching.borrow_mut().running = Some(task);
}

/// The root of the project this editor has open, or nothing where it has not
/// opened one. The first visible worktree: oxlint is run there, and the
/// report is about what is under it.
fn a_project_root(project: &Entity<Project>, cx: &App) -> Option<PathBuf> {
    project
        .read(cx)
        .visible_worktrees(cx)
        .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
        .next()
}

/// Where oxlint is, or nothing at all.
///
/// The project's own `node_modules/.bin` comes first, because a project that
/// pins an oxlint version means that one and its rule set, and a newer one on
/// PATH would report rules the project has not adopted. PATH is next, which
/// is where a single system-wide install lives.
///
/// Nothing found is the ordinary case for a machine with no oxlint, and the
/// caller's job is to be quiet about it.
fn where_oxlint_is(
    root: &Path,
    exists: impl Fn(&Path) -> bool,
    on_path: impl Fn() -> Option<PathBuf>,
) -> Option<PathBuf> {
    let installed = root.join("node_modules").join(".bin").join(oxlint_binary());
    if exists(&installed) {
        return Some(installed);
    }
    on_path()
}

/// npm writes a `.cmd` shim into `node_modules/.bin` on Windows; everywhere
/// else the name is plain.
fn oxlint_binary() -> OsString {
    if cfg!(target_os = "windows") {
        OsString::from("oxlint.cmd")
    } else {
        OsString::from("oxlint")
    }
}

async fn run_oxlint(root: &Path) -> Result<String> {
    let oxlint = where_oxlint_is(root, |path| path.is_file(), || which::which("oxlint").ok())
        .context("oxlint is not in this project's node_modules or on PATH")?;
    let asked = smol::process::Command::new(&oxlint)
        .current_dir(root)
        .args([
            "--format",
            "json",
            // A worktree whose JavaScript is all ignored is a clean project,
            // not a failed run.
            "--no-error-on-unmatched-pattern",
            ".",
        ])
        // Kept off the terminal: the reader asked for diagnostics, not for a
        // lint log.
        .stderr(std::process::Stdio::null())
        .output()
        .await
        .with_context(|| format!("running {} in {}", oxlint.display(), root.display()))?;
    let output = String::from_utf8(asked.stdout).context("oxlint's report is not text")?;
    // oxlint exits 1 both for a file with an error in it and for a run it
    // could not make at all, so the exit code says nothing on its own. What
    // separates them is the report: a failed run writes its reason as plain
    // text on the same stream.
    if output.trim().is_empty() {
        return Err(anyhow!("oxlint said nothing in {}", root.display()));
    }
    Ok(output)
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

/// Files oxlint complained about, and files it stopped complaining about,
/// both told to the editor. The second half is the one that is easy to
/// forget: what the editor was last given it keeps.
async fn show_what_it_said(
    project: &WeakEntity<Project>,
    root: &Path,
    output: &str,
    watching: &Rc<RefCell<Watching>>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let read = |path: &Path| std::fs::read_to_string(path).ok();
    let reported = what_oxlint_reported(output, root, read)
        .context("oxlint wrote something other than a report")?;
    let (telling, now_reported_in) = what_to_tell(reported, &watching.borrow().reported_in);
    watching.borrow_mut().reported_in = now_reported_in;

    project.update(cx, |project, cx| {
        project.lsp_store().update(cx, |lsp_store, cx| {
            for (path, diagnostics) in telling {
                let Ok(uri) = lsp::Uri::from_file_path(&path) else {
                    continue;
                };
                let merged = lsp_store.merge_lsp_diagnostics(
                    // oxlint is a run over the files on disk, not a live
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
                        server_id: OXLINT_SERVER_ID,
                        disk_based_sources: std::borrow::Cow::Borrowed(&[]),
                    }],
                    |_, _, _| false,
                    cx,
                );
                if let Err(error) = merged {
                    log::warn!("showing oxlint's report for {}: {error:#}", path.display());
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
                message: "`debugger` statement is not allowed".to_string(),
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
        let last_time: HashSet<PathBuf> = ["a.ts", "b.ts"].into_iter().map(PathBuf::from).collect();
        let (telling, now) = what_to_tell(vec![one_at("a.ts")], &last_time);

        let mut telling: Vec<(String, usize)> = telling
            .into_iter()
            .map(|(path, diagnostics)| (path.display().to_string(), diagnostics.len()))
            .collect();
        telling.sort();
        assert_eq!(
            telling,
            vec![("a.ts".to_string(), 1), ("b.ts".to_string(), 0)],
            "`b.ts` is told it is clean; saying nothing would leave its finding up"
        );
        assert_eq!(
            now,
            ["a.ts"]
                .into_iter()
                .map(PathBuf::from)
                .collect::<HashSet<_>>(),
            "and only the file that still reports is remembered"
        );
    }

    /// The whole of a fix, end to end: real output over a file with findings,
    /// then real output over a project with none. The second run has to hand
    /// the editor an empty list for the file that used to complain, because
    /// that is the only thing that takes its underlining down.
    #[test]
    fn a_file_that_is_fixed_ends_with_an_empty_report_and_not_its_old_one() {
        const FOUND: &str = include_str!("../test_data/oxlint-check.json");
        const CLEAN: &str = include_str!("../test_data/oxlint-clean.json");
        let linted = include_str!("../test_data/lint_me.ts").to_string();
        let read = |_: &Path| Some(linted.clone());

        let while_broken = what_oxlint_reported(FOUND, Path::new("/project"), read)
            .expect("the captured output is a report");
        let (telling, remembered) = what_to_tell(while_broken, &HashSet::default());
        assert_eq!(
            telling
                .iter()
                .map(|(path, diagnostics)| (path.display().to_string(), diagnostics.len()))
                .collect::<Vec<_>>(),
            vec![("/project/lint_me.ts".to_string(), 3)]
        );

        let once_fixed = what_oxlint_reported(CLEAN, Path::new("/project"), read)
            .expect("the captured output is a report");
        let (telling, remembered) = what_to_tell(once_fixed, &remembered);
        assert_eq!(
            telling
                .iter()
                .map(|(path, diagnostics)| (path.display().to_string(), diagnostics.len()))
                .collect::<Vec<_>>(),
            vec![("/project/lint_me.ts".to_string(), 0)],
            "an empty report, not silence -- silence would leave the three findings up"
        );
        assert!(
            remembered.is_empty(),
            "and the file is forgotten, so it is not cleared again forever"
        );
    }

    /// Several findings in one file arrive as one report for that file: the
    /// editor replaces a file's whole set each time, so sending them one at a
    /// time would leave only the last.
    #[test]
    fn every_finding_in_one_file_is_told_at_once() {
        let (telling, _) = what_to_tell(
            vec![one_at("a.ts"), one_at("a.ts"), one_at("b.ts")],
            &HashSet::default(),
        );
        let mut counted: Vec<(String, usize)> = telling
            .into_iter()
            .map(|(path, diagnostics)| (path.display().to_string(), diagnostics.len()))
            .collect();
        counted.sort();
        assert_eq!(
            counted,
            vec![("a.ts".to_string(), 2), ("b.ts".to_string(), 1)]
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

    /// A machine with no oxlint anywhere is the ordinary case, and it has to
    /// be harmless: nothing found, so nothing run and nothing reported.
    #[test]
    fn no_oxlint_anywhere_is_nothing_found_rather_than_a_fault() {
        let found = where_oxlint_is(Path::new("/project"), |_| false, || None);
        assert_eq!(found, None);
    }

    /// The project's own install wins over PATH: a project that pins an
    /// oxlint version means that one, and a newer one on PATH would report
    /// rules the project has not adopted.
    #[test]
    fn the_projects_own_install_is_preferred_to_path() {
        let installed = Path::new("/project")
            .join("node_modules")
            .join(".bin")
            .join(oxlint_binary());
        let found = where_oxlint_is(
            Path::new("/project"),
            |path| path == installed,
            || Some(PathBuf::from("/usr/bin/oxlint")),
        );
        assert_eq!(found, Some(installed));
    }

    /// A single system-wide install is what most machines have, and it is
    /// found once the project holds none.
    #[test]
    fn an_oxlint_on_path_is_used_where_the_project_holds_none() {
        let found = where_oxlint_is(
            Path::new("/project"),
            |_| false,
            || Some(PathBuf::from("/usr/bin/oxlint")),
        );
        assert_eq!(found, Some(PathBuf::from("/usr/bin/oxlint")));
    }
}
