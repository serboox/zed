use std::path::PathBuf;

use anyhow::{Context as _, Result};
use gpui::{App, AppContext as _, AsyncApp, Entity, WeakEntity, actions};
use language::{Buffer, DiagnosticSourceKind};
use project::Project;
use project::buffer_store::BufferStoreEvent;
use settings::Settings as _;

use crate::{Accepted, TypoSettings, accepted_near, typos_in};

actions!(
    typo_diagnostics,
    [
        /// Reports the misspellings in every open file -- in names and in
        /// comments alike -- without a language server.
        Check
    ]
);

/// The id these diagnostics are filed under. There is no server behind it,
/// in the way `cargo_diagnostics` has none: the editor's diagnostics are
/// keyed by server, so a source that is not a server still needs an id.
/// Chosen far above any a running server would be assigned, and one apart
/// from every other source that is not a server.
const TYPOS_SERVER_ID: language::LanguageServerId = language::LanguageServerId(usize::MAX - 1010);

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

/// Watches one buffer for the two moments its misspellings are worth
/// reading: every save, and the moment the editor settles what language the
/// file is in. The second is not optional -- a buffer's language is usually
/// decided after the store reports it added, so a file checked only when it
/// appears is checked before there is anything to check it as, and the
/// reader would see nothing until their first save.
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

/// Reads one buffer's misspellings and hands them to the editor.
///
/// Only a file the editor has recognised a language for is read. A log, a
/// dump or a pasted blob is prose the reader did not write and does not
/// intend to fix, and reporting every unusual word in it would bury the
/// findings that are about their own code.
fn check_one(
    project: &Entity<Project>,
    buffer: &Entity<Buffer>,
    cx: &mut gpui::Context<workspace::Workspace>,
) {
    let Some(path) = local_path(buffer, cx) else {
        return;
    };
    if buffer.read(cx).language().is_none() {
        return;
    }
    let text = buffer.read(cx).text();
    let enabled = TypoSettings::get_global(cx).enabled;
    let project = project.downgrade();
    cx.spawn(async move |_, cx| {
        let found = cx
            .background_spawn({
                let path = path.clone();
                async move {
                    let looked_in = path.parent().unwrap_or(&path).to_path_buf();
                    let accepted = accepted_near(&looked_in, |at| std::fs::read_to_string(at).ok());
                    what_to_report(enabled, &text, &accepted)
                }
            })
            .await;
        if let Err(error) = show(&project, path, found, cx) {
            log::warn!("showing what typos said: {error:#}");
        }
    })
    .detach();
}

/// What to tell the editor about one file's text.
///
/// An empty list is a real answer and not silence: it is what takes down the
/// underlining of a misspelling the reader has just fixed, and it is also
/// what a reader who has turned the check off gets, so that turning it off
/// clears what it had already reported rather than leaving it on the screen.
pub fn what_to_report(enabled: bool, text: &str, accepted: &Accepted) -> Vec<lsp::Diagnostic> {
    if !enabled {
        return Vec::new();
    }
    typos_in(text, accepted)
}

fn local_path(buffer: &Entity<Buffer>, cx: &App) -> Option<PathBuf> {
    Some(buffer.read(cx).file()?.as_local()?.abs_path(cx))
}

fn show(
    project: &WeakEntity<Project>,
    path: PathBuf,
    found: Vec<lsp::Diagnostic>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let uri = lsp::Uri::from_file_path(&path)
        .map_err(|_| anyhow::anyhow!("{} is not a path a uri can name", path.display()))?;
    project
        .update(cx, |project, cx| {
            project.lsp_store().update(cx, |lsp_store, cx| {
                lsp_store.merge_lsp_diagnostics(
                    // A pass over the text as it stands rather than a live
                    // analysis, which is what this kind means: the editor keeps
                    // what it was given until the next pass instead of expecting
                    // it refreshed as the reader types.
                    DiagnosticSourceKind::Other,
                    vec![project::lsp_store::DocumentDiagnosticsUpdate {
                        diagnostics: lsp::PublishDiagnosticsParams {
                            uri,
                            diagnostics: found,
                            version: None,
                        },
                        result_id: None,
                        registration_id: None,
                        server_id: TYPOS_SERVER_ID,
                        disk_based_sources: std::borrow::Cow::Borrowed(&[]),
                    }],
                    |_, _, _| false,
                    cx,
                )
            })
        })
        .context("reaching the project this file belongs to")?
        .context("merging what typos said into the editor's diagnostics")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    const MISSPELLED: &str = "fn my_functoin() {}\n";

    /// Turned off, the check says nothing about a file it would otherwise
    /// have two things to say about -- and says it as an empty report, which
    /// is what takes down what it had already put on the screen.
    #[test]
    fn the_setting_turns_the_check_off() {
        let accepted = Accepted::default();

        let on = what_to_report(true, MISSPELLED, &accepted);
        assert_eq!(
            on.iter()
                .map(|found| found.message.clone())
                .collect::<Vec<_>>(),
            vec!["`functoin` should be `function`".to_string()]
        );

        assert_eq!(what_to_report(false, MISSPELLED, &accepted), Vec::new());
    }
}
