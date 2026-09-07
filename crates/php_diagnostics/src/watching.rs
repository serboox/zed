use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow};
use gpui::{App, AppContext as _, AsyncApp, Entity, WeakEntity, actions};
use language::{Buffer, DiagnosticSourceKind};
use project::Project;
use project::buffer_store::BufferStoreEvent;

use crate::what_php_reported;

actions!(
    php_diagnostics,
    [
        /// Asks php -l what is wrong with every open PHP file and shows
        /// the answer, without a language server.
        Check
    ]
);

/// The id these diagnostics are filed under. There is no server behind it, in
/// the way `cargo_diagnostics` has none: the editor's diagnostics are keyed
/// by server, so a source that is not a server still needs an id. Chosen far
/// above any a running server would be assigned, and one apart from every
/// other source that is not a server.
const SERVER_ID: language::LanguageServerId = language::LanguageServerId(usize::MAX - 1017);

/// The editor's name for this language.
const LANGUAGE: &str = "PHP";

/// The two exit codes a run that happened can end with: nothing wrong, and
/// something wrong. Anything else is a run that did not happen -- a missing
/// interpreter, a file it could not open -- which is not the same as a clean
/// file and must not be read as one.
const NOTHING_WRONG: i32 = 0;
const SOMETHING_WRONG: i32 = 255;

/// Reports what php -l says about every open PHP file in one workspace,
/// after each save -- but only where no language server is doing it already.
///
/// That condition is the whole design. A project with a PHP server running
/// already has its diagnostics, and asking as well would show the reader every
/// fault twice. A project without one has nothing, and this is what it gets.
/// So the feature turns itself on exactly where it is needed, and needs no
/// setting to say so.
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

/// Watches one buffer for the two moments its faults are worth reading: every
/// save, and the moment the editor settles what language the file is in.
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
    if !is_the_language(buffer, cx) {
        return;
    }
    let Some(path) = local_path(buffer, cx) else {
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
                async move { ask(&path).await }
            })
            .await
        {
            Ok(found) => found,
            Err(error) => {
                // A machine with no php -l is the ordinary case rather than a
                // fault, so this stays out of the reader's way -- and either
                // way it is no reason to clear what they were already shown.
                log::debug!("asking about {}: {error:#}", path.display());
                return;
            }
        };
        if let Err(error) = show(&project, path, found, cx) {
            log::warn!("showing what php -l said: {error:#}");
        }
    })
    .detach();
}

fn is_the_language(buffer: &Entity<Buffer>, cx: &App) -> bool {
    buffer
        .read(cx)
        .language()
        .is_some_and(|language| language.name().as_ref() == LANGUAGE)
}

fn local_path(buffer: &Entity<Buffer>, cx: &App) -> Option<PathBuf> {
    Some(buffer.read(cx).file()?.as_local()?.abs_path(cx))
}

/// Runs the check over one file and reads its answer.
///
/// It runs in the file's own directory and is given the bare file name,
/// because the name comes back inside the message and matching it is how a
/// fault meant for an included file is kept off this one.
///
/// The file's text is read from disk rather than taken from the buffer, so the
/// line numbers and the text they are placed against come from the same bytes
/// the interpreter read.
async fn ask(path: &Path) -> Result<Vec<lsp::Diagnostic>> {
    let directory = path.parent().context("a file with no directory above it")?;
    let named = path
        .file_name()
        .context("a file with no file name")?
        .to_str()
        .context("a file whose name is not text")?;
    let tool = which::which("php").context("php is not on PATH")?;
    let asked = smol::process::Command::new(&tool)
        .current_dir(directory)
        .args(["-l", named])
        .output()
        .await
        .with_context(|| format!("running {} in {}", tool.display(), directory.display()))?;
    if !matches!(asked.status.code(), Some(NOTHING_WRONG | SOMETHING_WRONG)) {
        return Err(anyhow!("php could not read {}", path.display()));
    }
    // Read from both: php writes its parse error to stdout when it is a
    // command-line run and to stderr when `display_errors` sends it there, and
    // which one it uses is a setting of the machine rather than of this run.
    let mut output = String::from_utf8_lossy(&asked.stdout).into_owned();
    output.push_str(&String::from_utf8_lossy(&asked.stderr));
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading {} back to place its lines", path.display()))?;
    Ok(what_php_reported(&output, named, &text))
}

/// Hands the editor what was said, replacing whatever this source said last.
///
/// An empty list is not silence and is meant: a file whose fault the reader
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
                        server_id: SERVER_ID,
                        disk_based_sources: std::borrow::Cow::Borrowed(&[]),
                    }],
                    |_, _, _| false,
                    cx,
                )
            })
        })
        .context("reaching the project this file belongs to")?
        .context("merging what was said into the editor's diagnostics")
}
