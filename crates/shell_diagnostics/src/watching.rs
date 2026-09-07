use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow};
use gpui::{App, AppContext as _, AsyncApp, Entity, WeakEntity, actions};
use language::{Buffer, DiagnosticSourceKind};
use project::Project;
use project::buffer_store::BufferStoreEvent;

use crate::what_shellcheck_reported;

actions!(
    shell_diagnostics,
    [
        /// Asks shellcheck what is wrong with every open shell script and
        /// shows the answer, without a language server.
        Check
    ]
);

/// The id these diagnostics are filed under. There is no server behind it, in
/// the way `cargo_diagnostics` has none: the editor's diagnostics are keyed
/// by server, so a source that is not a server still needs an id. Chosen far
/// above any a running server would be assigned, and one apart from every
/// other source that is not a server.
const SHELLCHECK_SERVER_ID: language::LanguageServerId =
    language::LanguageServerId(usize::MAX - 1015);

/// Zed's one name for every shell it reads, `.sh` and `.zshrc` alike.
const SHELL_LANGUAGE: &str = "Shell Script";

/// shellcheck's exit codes for a run it made: nothing found, and something
/// found. Anything else -- a file it could not open, an option it does not
/// know, a `.shellcheckrc` it could not read -- is a run that did not happen,
/// which is not the same as a clean script and must not be read as one.
const NOTHING_FOUND: i32 = 0;
const SOMETHING_FOUND: i32 = 1;

/// Reports what shellcheck says about every open shell script in one
/// workspace, after each save -- but only where no language server is doing
/// it already.
///
/// That condition is the whole design. A project with a shell server running
/// already has its diagnostics, and asking shellcheck as well would show the
/// reader every finding twice. A project without one has nothing, and this is
/// what it gets. So the feature turns itself on exactly where it is needed,
/// and needs no setting to say so.
pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut workspace::Workspace, _, cx| {
        let project = workspace.project().clone();

        // Every buffer already open, and every one opened later. Collected
        // first: reading the store borrows the context that watching one
        // needs mutably.
        let already_open: Vec<Entity<Buffer>> =
            project.read(cx).buffer_store().read(cx).buffers().collect();
        for buffer in already_open {
            watch_one(&project, &buffer, cx);
            check_one(&project, &buffer, cx);
        }

        let buffer_store = project.read(cx).buffer_store().clone();
        cx.subscribe(&buffer_store, {
            let project = project.clone();
            move |_: &mut workspace::Workspace, _, event, cx| {
                if let BufferStoreEvent::BufferAdded(buffer) = event {
                    watch_one(&project, buffer, cx);
                    check_one(&project, buffer, cx);
                }
            }
        })
        .detach();

        workspace.register_action({
            let project = project.clone();
            move |_, _: &Check, _, cx| {
                let open: Vec<Entity<Buffer>> =
                    project.read(cx).buffer_store().read(cx).buffers().collect();
                for buffer in open {
                    check_one(&project, &buffer, cx);
                }
            }
        });
    })
    .detach();
}

/// Watches one buffer for the two moments its findings are worth reading:
/// every save, and the moment the editor settles what language the file is
/// in.
///
/// The second is not optional. A buffer's language is usually decided after
/// the store reports it added, so a file checked only when it appears is
/// checked before there is anything to check it as, and the reader would see
/// nothing until their first save.
fn watch_one(
    project: &Entity<Project>,
    buffer: &Entity<Buffer>,
    cx: &mut gpui::Context<workspace::Workspace>,
) {
    cx.subscribe(buffer, {
        let project = project.clone();
        move |_: &mut workspace::Workspace, buffer, event, cx| {
            if matches!(
                event,
                language::BufferEvent::Saved | language::BufferEvent::LanguageChanged(_)
            ) {
                check_one(&project, &buffer, cx);
            }
        }
    })
    .detach();
}

fn check_one(
    project: &Entity<Project>,
    buffer: &Entity<Buffer>,
    cx: &mut gpui::Context<workspace::Workspace>,
) {
    // Asked here rather than before subscribing: a buffer's language is often
    // settled after the store reports it added, so a check made once at
    // subscription time misses the first file of a session entirely -- which
    // is what the `LanguageChanged` arm above is for.
    if !is_a_shell_script(buffer, cx) {
        return;
    }
    let Some((path, project_root)) = local_path_and_root(project, buffer, cx) else {
        return;
    };
    let lsp_store = project.read(cx).lsp_store();
    // Nested this way round because the check needs the buffer and the
    // application both, and the buffer's own update is what hands over one
    // without holding the other.
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
    let project = project.downgrade();
    cx.spawn(async move |_, cx| {
        let found = match cx
            .background_spawn({
                let path = path.clone();
                async move { ask_shellcheck(&path, &project_root).await }
            })
            .await
        {
            Ok(found) => found,
            Err(error) => {
                // A machine with no shellcheck anywhere is the ordinary case
                // rather than a fault, so this stays out of the reader's way
                // -- and either way it is no reason to clear what they were
                // already shown.
                log::debug!("asking shellcheck about {}: {error:#}", path.display());
                return;
            }
        };
        if let Err(error) = show(&project, path, found, cx) {
            log::warn!("showing what shellcheck said: {error:#}");
        }
    })
    .detach();
}

fn is_a_shell_script(buffer: &Entity<Buffer>, cx: &App) -> bool {
    buffer
        .read(cx)
        .language()
        .is_some_and(|language| language.name().as_ref() == SHELL_LANGUAGE)
}

/// Where the script is, and the root of the project holding it -- which is
/// where a project's own shellcheck install would be, and not where the
/// script itself happens to sit.
fn local_path_and_root(
    project: &Entity<Project>,
    buffer: &Entity<Buffer>,
    cx: &App,
) -> Option<(PathBuf, PathBuf)> {
    let read = buffer.read(cx);
    let file = read.file()?;
    let path = file.as_local()?.abs_path(cx);
    let root = project
        .read(cx)
        .worktree_for_id(file.worktree_id(cx), cx)?
        .read(cx)
        .abs_path()
        .to_path_buf();
    Some((path, root))
}

/// npm's `shellcheck` package and pip's `shellcheck-py` both drop a binary
/// into a project, and the shell scripts of such a project are meant to pass
/// the version it pins: a newer one on PATH reports rules the project has not
/// adopted. PATH is last, which is where a single system-wide install lives.
///
/// Nothing found is the ordinary case for a machine with no shellcheck, and
/// the caller's job is to be quiet about it.
fn where_shellcheck_is(
    root: &Path,
    exists: impl Fn(&Path) -> bool,
    on_path: impl Fn() -> Option<PathBuf>,
) -> Option<PathBuf> {
    let installed = [
        root.join("node_modules").join(".bin"),
        root.join(".venv").join(BINARY_DIR),
        root.join("venv").join(BINARY_DIR),
    ];
    for directory in installed {
        let candidate = directory.join(shellcheck_binary());
        if exists(&candidate) {
            return Some(candidate);
        }
    }
    on_path()
}

/// The virtualenv layout: `bin` everywhere but Windows, which uses `Scripts`.
const BINARY_DIR: &str = if cfg!(target_os = "windows") {
    "Scripts"
} else {
    "bin"
};

fn shellcheck_binary() -> OsString {
    if cfg!(target_os = "windows") {
        OsString::from("shellcheck.exe")
    } else {
        OsString::from("shellcheck")
    }
}

/// Runs shellcheck over one script and reads its answer.
///
/// It is run in the script's own directory, and given the bare file name: the
/// name comes back on every finding, and matching it against the name that
/// was asked about is how a finding meant for a `source`d file is kept off
/// this one.
///
/// The file's text is read from disk rather than taken from the buffer, so
/// that the columns and the text they are converted against come from the
/// same bytes shellcheck read.
async fn ask_shellcheck(path: &Path, project_root: &Path) -> Result<Vec<lsp::Diagnostic>> {
    let directory = path
        .parent()
        .context("a script with no directory above it")?;
    let named = path
        .file_name()
        .context("a script with no file name")?
        .to_str()
        .context("a script whose name is not text")?;
    let shellcheck = where_shellcheck_is(
        project_root,
        |candidate| candidate.is_file(),
        || which::which("shellcheck").ok(),
    )
    .context("shellcheck is not in this project or on PATH")?;
    let asked = smol::process::Command::new(&shellcheck)
        .current_dir(directory)
        .args(["--format=json1", named])
        // Kept off the terminal: the reader asked for diagnostics, not for a
        // lint log, and shellcheck's own errors go to stderr.
        .stderr(std::process::Stdio::null())
        .output()
        .await
        .with_context(|| {
            format!(
                "running {} in {}",
                shellcheck.display(),
                directory.display()
            )
        })?;
    if !matches!(asked.status.code(), Some(NOTHING_FOUND | SOMETHING_FOUND)) {
        return Err(anyhow!("shellcheck could not read {}", path.display()));
    }
    let output = String::from_utf8(asked.stdout).context("shellcheck's report is not text")?;
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading {} back to place its columns", path.display()))?;
    Ok(what_shellcheck_reported(&output, named, &text))
}

/// Hands the editor what shellcheck said, replacing whatever this source said
/// last.
///
/// An empty list is not silence and is meant: a script whose fault the reader
/// has just fixed has to be told it is clean now, or the editor keeps showing
/// the fault.
fn show(
    project: &WeakEntity<Project>,
    path: PathBuf,
    found: Vec<lsp::Diagnostic>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let uri = lsp::Uri::from_file_path(&path)
        .map_err(|_| anyhow!("{} is not a path a uri can name", path.display()))?;
    project
        .update(cx, |project, cx| {
            project.lsp_store().update(cx, |lsp_store, cx| {
                lsp_store.merge_lsp_diagnostics(
                    // A run over the file on disk rather than a live analysis,
                    // which is what this kind means: the editor keeps what it
                    // was given until the next run instead of expecting it
                    // refreshed as the reader types.
                    DiagnosticSourceKind::Other,
                    vec![project::lsp_store::DocumentDiagnosticsUpdate {
                        diagnostics: lsp::PublishDiagnosticsParams {
                            uri,
                            diagnostics: found,
                            version: None,
                        },
                        result_id: None,
                        registration_id: None,
                        server_id: SHELLCHECK_SERVER_ID,
                        disk_based_sources: std::borrow::Cow::Borrowed(&[]),
                    }],
                    |_, _, _| false,
                    cx,
                )
            })
        })
        .context("reaching the project this script belongs to")?
        .context("merging what shellcheck said into the editor's diagnostics")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// A machine with no shellcheck anywhere is the ordinary case, and it has
    /// to be harmless: nothing found, so nothing run and nothing reported.
    #[test]
    fn no_shellcheck_anywhere_is_nothing_found_rather_than_a_fault() {
        let found = where_shellcheck_is(Path::new("/project"), |_| false, || None);
        assert_eq!(found, None);
    }

    /// The project's own install wins over PATH: a project that pins a
    /// shellcheck version means that one, and a newer one on PATH reports
    /// rules the project has not adopted.
    #[test]
    fn the_projects_own_install_is_preferred_to_path() {
        let inside = [
            Path::new("/project").join("node_modules").join(".bin"),
            Path::new("/project").join(".venv").join(BINARY_DIR),
        ];
        for directory in inside {
            let installed = directory.join(shellcheck_binary());
            let found = where_shellcheck_is(
                Path::new("/project"),
                |candidate| candidate == installed,
                || Some(PathBuf::from("/usr/bin/shellcheck")),
            );
            assert_eq!(found, Some(installed));
        }
    }

    /// A single system-wide install is what most machines have, and it is
    /// found once the project holds none.
    #[test]
    fn a_shellcheck_on_path_is_used_where_the_project_holds_none() {
        let found = where_shellcheck_is(
            Path::new("/project"),
            |_| false,
            || Some(PathBuf::from("/usr/bin/shellcheck")),
        );
        assert_eq!(found, Some(PathBuf::from("/usr/bin/shellcheck")));
    }
}
