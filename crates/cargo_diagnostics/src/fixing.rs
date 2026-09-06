use std::collections::HashMap;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use gpui::{App, Entity, Task};
use language::{Bias, Buffer, BufferSnapshot, PointUtf16, range_from_lsp};
use project::{
    CodeAction, InProcessCodeActionContext, InProcessCodeActions, LspAction,
    register_in_process_code_actions,
};

use crate::watching::CARGO_SERVER_ID;
use crate::{Fix, Reported};

/// The fixes the last check found, kept so that a reader whose cursor is on
/// an error can be offered the one the compiler already computed for it.
///
/// Replaced whole each time rather than merged: one check covers the whole
/// workspace, so a file the new report says nothing about has no fixes, and
/// keeping the old ones would offer a fix for an error that is gone.
#[derive(Default)]
pub struct Fixes {
    by_file: HashMap<PathBuf, Vec<Anchored>>,
}

/// A fix and the place the diagnostic it belongs to was reported at.
///
/// The two are often far apart -- an import goes to the top of the file while
/// the error is at the use -- so what decides whether to offer a fix is where
/// the error is, which is where the reader's cursor will be.
struct Anchored {
    at: lsp::Range,
    fix: Fix,
}

impl Fixes {
    pub fn remember(&mut self, reported: &[Reported]) {
        let mut by_file: HashMap<PathBuf, Vec<Anchored>> = HashMap::new();
        for one in reported {
            for fix in &one.fixes {
                by_file
                    .entry(one.path.clone())
                    .or_default()
                    .push(Anchored {
                        at: one.diagnostic.range,
                        fix: fix.clone(),
                    });
            }
        }
        self.by_file = by_file;
    }
}

/// Offers the compiler's own suggestions as quick fixes, with no language
/// server anywhere.
pub fn init(fixes: Arc<Mutex<Fixes>>, cx: &mut App) {
    register_in_process_code_actions(Arc::new(SuggestedByTheCompiler(fixes)), cx);
}

struct SuggestedByTheCompiler(Arc<Mutex<Fixes>>);

impl InProcessCodeActions for SuggestedByTheCompiler {
    /// The same id the diagnostics are filed under. A fix and the error it
    /// fixes come from one run of one tool, and one id is what tells the
    /// editor they belong together.
    fn server_id(&self) -> lsp::LanguageServerId {
        CARGO_SERVER_ID
    }

    fn code_actions(
        &self,
        _context: &InProcessCodeActionContext,
        buffer: &Entity<Buffer>,
        range: Range<PointUtf16>,
        cx: &mut App,
    ) -> Task<Vec<CodeAction>> {
        Task::ready(self.offered_for(buffer, range, cx).unwrap_or_default())
    }
}

impl SuggestedByTheCompiler {
    fn offered_for(
        &self,
        buffer: &Entity<Buffer>,
        asked_about: Range<PointUtf16>,
        cx: &App,
    ) -> Option<Vec<CodeAction>> {
        // No language check: the store holds only what the compiler reported
        // on, so a file cargo never mentioned misses the lookup below anyway.
        let read = buffer.read(cx);
        // The offsets in the report were measured on the file as it was
        // saved, which is when the check ran. Unsaved edits have moved them,
        // and a replacement made against moved offsets overwrites text the
        // compiler never looked at.
        if read.is_dirty() {
            return None;
        }
        let path = read.file()?.as_local()?.abs_path(cx);
        let snapshot = read.snapshot();
        let fixes = self.0.lock().ok()?;
        let offered = fixes
            .by_file
            .get(&path)?
            .iter()
            .filter(|anchored| on_the_same_lines(anchored.at, &asked_about))
            .filter(|anchored| still_says_what_it_said(&anchored.fix, &snapshot))
            .map(|anchored| {
                let at = in_the_buffer(anchored.at, &snapshot);
                CodeAction {
                    server_id: CARGO_SERVER_ID,
                    range: snapshot.anchor_before(at.start)..snapshot.anchor_after(at.end),
                    lsp_action: LspAction::Action(Box::new(lsp::CodeAction {
                        title: anchored.fix.title.clone(),
                        kind: Some(lsp::CodeActionKind::QUICKFIX),
                        edit: Some(as_a_workspace_edit(&anchored.fix)),
                        ..Default::default()
                    })),
                    // Nothing to resolve: there is no server to ask, and the
                    // whole edit is in the action already.
                    resolved: true,
                }
            })
            .collect();
        Some(offered)
    }
}

/// Whether a fix reported at `at` is one to offer for a cursor or selection
/// covering `asked_about`.
///
/// Compared by line rather than by column: the reader puts the cursor
/// somewhere on the line the error is on, not on the exact character the
/// compiler underlined.
fn on_the_same_lines(at: lsp::Range, asked_about: &Range<PointUtf16>) -> bool {
    at.start.line <= asked_about.end.row && at.end.line >= asked_about.start.row
}

fn in_the_buffer(range: lsp::Range, snapshot: &BufferSnapshot) -> Range<PointUtf16> {
    let range = range_from_lsp(range);
    snapshot.clip_point_utf16(range.start, Bias::Left)
        ..snapshot.clip_point_utf16(range.end, Bias::Left)
}

/// Whether the text the compiler measured is still the text that is there.
///
/// A fix carries what it expects to replace, so a file that changed under the
/// report can be told from one that did not. An insertion replaces nothing
/// and cannot be checked this way; the unsaved-edit check above is what
/// covers it.
fn still_says_what_it_said(fix: &Fix, snapshot: &BufferSnapshot) -> bool {
    fix.replacements.iter().all(|replacement| {
        if replacement.replaced.is_empty() {
            return true;
        }
        let range = in_the_buffer(replacement.range, snapshot);
        snapshot.text_for_range(range).collect::<String>() == replacement.replaced
    })
}

fn as_a_workspace_edit(fix: &Fix) -> lsp::WorkspaceEdit {
    let mut changes: HashMap<lsp::Uri, Vec<lsp::TextEdit>> = HashMap::new();
    for replacement in &fix.replacements {
        let Ok(uri) = lsp::Uri::from_file_path(&replacement.path) else {
            continue;
        };
        changes.entry(uri).or_default().push(lsp::TextEdit {
            range: replacement.range,
            new_text: replacement.new_text.clone(),
        });
    }
    lsp::WorkspaceEdit {
        changes: Some(changes),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::what_the_compiler_reported;
    use gpui::TestAppContext;
    use project::Project;
    use serde_json::json;
    use settings::SettingsStore;
    use std::path::Path;
    use util::path;

    const MULTIBYTE: &str = include_str!("../test_data/cargo-check-multibyte.json");
    const MULTIBYTE_SOURCE: &str = include_str!("../test_data/cargo-check-multibyte.source");
    const SUGGESTED: &str = include_str!("../test_data/cargo-check-suggestions.json");
    const SUGGESTED_SOURCE: &str = include_str!("../test_data/cargo-check-suggestions.source");

    /// The store as a real check leaves it: the compiler's own output, parsed
    /// by the reader that runs in the editor, over the exact text the compiler
    /// measured.
    fn what_a_check_would_have_left(output: &str, source: &str) -> Arc<Mutex<Fixes>> {
        let reported = what_the_compiler_reported(output, Path::new(path!("/project")), |path| {
            (path == Path::new(path!("/project/src/lib.rs"))).then(|| source.to_string())
        });
        assert!(
            reported.iter().any(|one| !one.fixes.is_empty()),
            "the fixture is only meaningful while it still carries a fix"
        );
        let mut fixes = Fixes::default();
        fixes.remember(&reported);
        Arc::new(Mutex::new(fixes))
    }

    /// A project with no language server anywhere, which is the case this
    /// source exists for.
    async fn a_project_holding(
        on_disk: &str,
        cx: &mut TestAppContext,
    ) -> (Entity<Project>, Entity<Buffer>) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({ "src": { "lib.rs": on_disk } }))
            .await;
        let fs: Arc<dyn fs::Fs> = fs;
        let project = Project::test(fs, [Path::new(path!("/project"))], cx).await;
        let buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer(path!("/project/src/lib.rs"), cx)
            })
            .await
            .expect("the file should open");
        cx.executor().run_until_parked();
        (project, buffer)
    }

    async fn offered_at(
        project: &Entity<Project>,
        buffer: &Entity<Buffer>,
        offset: usize,
        cx: &mut TestAppContext,
    ) -> Vec<CodeAction> {
        project
            .update(cx, |project, cx| {
                project.code_actions(buffer, offset..offset, None, cx)
            })
            .await
            .expect("asking for code actions should not fail")
            .unwrap_or_default()
    }

    /// The whole point, end to end: the compiler's own suggestion reaches the
    /// reader as a quick fix, and taking it writes exactly what the compiler
    /// asked for -- on a line where a byte count, a character count and a
    /// UTF-16 count are three different numbers.
    #[gpui::test]
    async fn the_compilers_fix_is_offered_and_writes_its_own_text(cx: &mut TestAppContext) {
        let (project, buffer) = a_project_holding(MULTIBYTE_SOURCE, cx).await;
        let fixes = what_a_check_would_have_left(MULTIBYTE, MULTIBYTE_SOURCE);
        cx.update(|cx| init(fixes, cx));

        // Where the error is, which is a line below where the fix lands.
        let at = MULTIBYTE_SOURCE
            .find("счётчик += 1")
            .expect("the assignment the error is on");
        let offered = offered_at(&project, &buffer, at, cx).await;

        assert_eq!(
            offered
                .iter()
                .map(|action| action.lsp_action.title().to_string())
                .collect::<Vec<_>>(),
            vec!["consider making this binding mutable".to_string()]
        );
        let fix = offered.first().expect("the fix").clone();

        project
            .update(cx, |project, cx| {
                project.apply_code_action(buffer.clone(), fix, true, cx)
            })
            .await
            .expect("applying the fix should not fail");

        assert_eq!(
            buffer.read_with(cx, |buffer, _| buffer.text()),
            MULTIBYTE_SOURCE.replace("let счётчик", "let mut счётчик"),
            "the `mut` goes before the binding on the emoji line and nothing else moves"
        );
    }

    /// A deletion the compiler asked for is applied as one.
    #[gpui::test]
    async fn a_fix_that_deletes_removes_exactly_what_it_named(cx: &mut TestAppContext) {
        let (project, buffer) = a_project_holding(SUGGESTED_SOURCE, cx).await;
        let fixes = what_a_check_would_have_left(SUGGESTED, SUGGESTED_SOURCE);
        cx.update(|cx| init(fixes, cx));

        let at = SUGGESTED_SOURCE
            .find("use std::fmt::Debug;")
            .expect("the unused import");
        let offered = offered_at(&project, &buffer, at, cx).await;
        let fix = offered
            .iter()
            .find(|action| action.lsp_action.title() == "remove the whole `use` item")
            .expect("the removal")
            .clone();

        project
            .update(cx, |project, cx| {
                project.apply_code_action(buffer.clone(), fix, true, cx)
            })
            .await
            .expect("applying the fix should not fail");

        assert_eq!(
            buffer.read_with(cx, |buffer, _| buffer.text()),
            SUGGESTED_SOURCE.replace("use std::fmt::Debug;\n", "")
        );
    }

    /// Nothing is offered on a line the reader is not on. A fix belongs to
    /// one error, and every other error's fix would be noise.
    #[gpui::test]
    async fn a_fix_is_offered_only_near_the_error_it_belongs_to(cx: &mut TestAppContext) {
        let (project, buffer) = a_project_holding(MULTIBYTE_SOURCE, cx).await;
        let fixes = what_a_check_would_have_left(MULTIBYTE, MULTIBYTE_SOURCE);
        cx.update(|cx| init(fixes, cx));

        let elsewhere = MULTIBYTE_SOURCE
            .find("pub fn")
            .expect("the first line, which no error is on");
        assert!(
            offered_at(&project, &buffer, elsewhere, cx).await.is_empty(),
            "the error is three lines down"
        );
    }

    /// Unsaved edits have moved every offset the compiler measured, so
    /// nothing is offered until the file is saved and checked again.
    #[gpui::test]
    async fn a_buffer_with_unsaved_edits_is_offered_nothing(cx: &mut TestAppContext) {
        let (project, buffer) = a_project_holding(MULTIBYTE_SOURCE, cx).await;
        let fixes = what_a_check_would_have_left(MULTIBYTE, MULTIBYTE_SOURCE);
        cx.update(|cx| init(fixes, cx));

        buffer.update(cx, |buffer, cx| {
            buffer.edit([(0..0, "// one line more\n")], None, cx)
        });
        let at = buffer
            .read_with(cx, |buffer, _| buffer.text())
            .find("счётчик += 1")
            .expect("the assignment");
        assert!(offered_at(&project, &buffer, at, cx).await.is_empty());
    }

    /// A file that is saved but is not the file the compiler read -- changed
    /// on disk, or checked out from under the report -- is offered nothing
    /// either: the fix carries the text it expects to replace, and it is not
    /// there.
    #[gpui::test]
    async fn a_fix_measured_against_other_text_is_not_offered(cx: &mut TestAppContext) {
        let changed = SUGGESTED_SOURCE.replace("use std::fmt::Debug;", "use std::fmt::Display;");
        let (project, buffer) = a_project_holding(&changed, cx).await;
        let fixes = what_a_check_would_have_left(SUGGESTED, SUGGESTED_SOURCE);
        cx.update(|cx| init(fixes, cx));

        let at = changed.find("use std::fmt::Display;").expect("the import");
        let offered = offered_at(&project, &buffer, at, cx).await;
        assert!(
            !offered
                .iter()
                .any(|action| action.lsp_action.title() == "remove the whole `use` item"),
            "the import the compiler asked to remove is not the one that is there"
        );
    }
}
