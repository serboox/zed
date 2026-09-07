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

use crate::watching::GO_SERVER_ID;
use crate::{Fix, Reported};

/// The fixes the last run found, kept so that a reader whose cursor is on a
/// finding can be offered the one the analyzer already computed for it.
///
/// Replaced whole each time rather than merged: one run covers the whole
/// module, so a file the new report says nothing about has no fixes, and
/// keeping the old ones would offer a fix for a finding that is gone.
#[derive(Default)]
pub struct Fixes {
    by_file: HashMap<PathBuf, Vec<Anchored>>,
}

/// A fix and the place the finding it belongs to was reported at.
///
/// The two can be far apart -- an analyzer may ask for an import at the top
/// of the file to fix a call further down -- so what decides whether to offer
/// a fix is where the finding is, which is where the reader's cursor will be.
struct Anchored {
    at: lsp::Range,
    fix: Fix,
}

impl Fixes {
    pub fn remember(&mut self, reported: &[Reported]) {
        let mut by_file: HashMap<PathBuf, Vec<Anchored>> = HashMap::new();
        for one in reported {
            for fix in &one.fixes {
                by_file.entry(one.path.clone()).or_default().push(Anchored {
                    at: one.diagnostic.range,
                    fix: fix.clone(),
                });
            }
        }
        self.by_file = by_file;
    }
}

/// Offers the analyzers' own suggested fixes as quick fixes, with no language
/// server anywhere.
pub fn init(fixes: Arc<Mutex<Fixes>>, cx: &mut App) {
    register_in_process_code_actions(Arc::new(SuggestedByAnAnalyzer(fixes)), cx);
}

struct SuggestedByAnAnalyzer(Arc<Mutex<Fixes>>);

impl InProcessCodeActions for SuggestedByAnAnalyzer {
    /// The same id the diagnostics are filed under. A fix and the finding it
    /// fixes come from one run of one tool, and one id is what tells the
    /// editor they belong together.
    fn server_id(&self) -> lsp::LanguageServerId {
        GO_SERVER_ID
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

impl SuggestedByAnAnalyzer {
    fn offered_for(
        &self,
        buffer: &Entity<Buffer>,
        asked_about: Range<PointUtf16>,
        cx: &App,
    ) -> Option<Vec<CodeAction>> {
        let read = buffer.read(cx);
        // The offsets in the report were measured on the file as it was
        // saved, which is when vet ran. Unsaved edits have moved them, and a
        // replacement made against moved offsets overwrites text the analyzer
        // never looked at.
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
                    server_id: GO_SERVER_ID,
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
/// somewhere on the line the finding is on, not on the exact character the
/// analyzer pointed at.
fn on_the_same_lines(at: lsp::Range, asked_about: &Range<PointUtf16>) -> bool {
    at.start.line <= asked_about.end.row && at.end.line >= asked_about.start.row
}

fn in_the_buffer(range: lsp::Range, snapshot: &BufferSnapshot) -> Range<PointUtf16> {
    let range = range_from_lsp(range);
    snapshot.clip_point_utf16(range.start, Bias::Left)
        ..snapshot.clip_point_utf16(range.end, Bias::Left)
}

/// Whether the text the analyzer measured is still the text that is there.
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
    use crate::what_vet_reported;
    use gpui::TestAppContext;
    use project::Project;
    use serde_json::json;
    use settings::SettingsStore;
    use std::path::Path;
    use util::path;

    const REAL_OUTPUT: &str = include_str!("../test_data/go-vet-fixes.txt");
    const REAL_SOURCE: &str = include_str!("../test_data/go-vet-fixes.source");

    /// The store as a real run leaves it: vet's own output, parsed by the
    /// reader that runs in the editor, over the exact text the analyzers
    /// measured.
    fn what_a_run_would_have_left(output: &str, source: &str) -> Arc<Mutex<Fixes>> {
        let reported = what_vet_reported(output, Path::new(path!("/src")), |path| {
            (path == Path::new(path!("/src/main.go"))).then(|| source.to_string())
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
        fs.insert_tree(path!("/src"), json!({ "main.go": on_disk }))
            .await;
        let fs: Arc<dyn fs::Fs> = fs;
        let project = Project::test(fs, [Path::new(path!("/src"))], cx).await;
        let buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer(path!("/src/main.go"), cx)
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

    fn titles(offered: &[CodeAction]) -> Vec<String> {
        offered
            .iter()
            .map(|action| action.lsp_action.title().to_string())
            .collect()
    }

    /// The whole point, end to end: an analyzer's own suggestion reaches the
    /// reader as a quick fix under the analyzer's own name for it, and taking
    /// it writes exactly what the analyzer asked for -- on a line where a
    /// byte count, a character count and a UTF-16 count are three different
    /// numbers.
    #[gpui::test]
    async fn an_analyzers_fix_is_offered_and_writes_its_own_text(cx: &mut TestAppContext) {
        // Three counts of the same place, all different.
        let line = REAL_SOURCE
            .lines()
            .nth(9)
            .expect("the line the format string is on");
        let format = line.find("2006-02-01").expect("the format string");
        assert_eq!(line[..format].len(), 58, "bytes");
        assert_eq!(line[..format].chars().count(), 47, "characters");
        assert_eq!(line[..format].encode_utf16().count(), 48, "UTF-16 units");

        let (project, buffer) = a_project_holding(REAL_SOURCE, cx).await;
        let fixes = what_a_run_would_have_left(REAL_OUTPUT, REAL_SOURCE);
        cx.update(|cx| init(fixes, cx));

        let at = REAL_SOURCE
            .find("2006-02-01")
            .expect("the format string the finding is on");
        let offered = offered_at(&project, &buffer, at, cx).await;
        assert_eq!(
            titles(&offered),
            vec!["Replace 2006-02-01 with 2006-01-02".to_string()]
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
            REAL_SOURCE.replace("2006-02-01", "2006-01-02"),
            "the day and month swap on the emoji line and nothing else moves"
        );
    }

    /// An analyzer that offers two ways to fix one finding has both offered,
    /// and the one made of two edits writes both halves at once. Its `rune(`
    /// and closing `)` are one change, and either alone leaves the file
    /// broken.
    #[gpui::test]
    async fn a_fix_made_of_two_edits_writes_both_halves(cx: &mut TestAppContext) {
        let (project, buffer) = a_project_holding(REAL_SOURCE, cx).await;
        let fixes = what_a_run_would_have_left(REAL_OUTPUT, REAL_SOURCE);
        cx.update(|cx| init(fixes, cx));

        let at = REAL_SOURCE
            .find("string(code)")
            .expect("the conversion the finding is on");
        let offered = offered_at(&project, &buffer, at, cx).await;
        assert_eq!(
            titles(&offered),
            vec![
                "Format the number as a decimal".to_string(),
                "Convert a single rune to a string".to_string(),
            ],
            "the analyzer's alternatives, in the order it listed them"
        );
        let fix = offered
            .into_iter()
            .find(|action| action.lsp_action.title() == "Convert a single rune to a string")
            .expect("the two-edit alternative");

        project
            .update(cx, |project, cx| {
                project.apply_code_action(buffer.clone(), fix, true, cx)
            })
            .await
            .expect("applying the fix should not fail");

        assert_eq!(
            buffer.read_with(cx, |buffer, _| buffer.text()),
            REAL_SOURCE.replace("string(code)", "string(rune(code))")
        );
    }

    /// Nothing is offered on a line the reader is not on. A fix belongs to
    /// one finding, and every other finding's fix would be noise.
    #[gpui::test]
    async fn a_fix_is_offered_only_near_the_finding_it_belongs_to(cx: &mut TestAppContext) {
        let (project, buffer) = a_project_holding(REAL_SOURCE, cx).await;
        let fixes = what_a_run_would_have_left(REAL_OUTPUT, REAL_SOURCE);
        cx.update(|cx| init(fixes, cx));

        let elsewhere = REAL_SOURCE
            .find("func main")
            .expect("a line no finding is on");
        assert!(
            offered_at(&project, &buffer, elsewhere, cx)
                .await
                .is_empty()
        );
    }

    /// A fix whose edits break the contract every analyzer writes to -- they
    /// "must not overlap" -- is refused rather than offered. `go vet` grades
    /// nothing, so a malformed fix is the only kind this reader can tell
    /// apart, and half of one applied over the other half would corrupt the
    /// file.
    #[gpui::test]
    async fn a_fix_whose_edits_overlap_is_not_offered(cx: &mut TestAppContext) {
        // The `rune(` insertion stretched over the closing `)` its own fix
        // asks for, so the two edits of that one fix now cover the same text.
        let overlapping = REAL_OUTPUT.replace("\"end\": 198,", "\"end\": 205,");
        assert_ne!(overlapping, REAL_OUTPUT, "the fixture shape has moved");

        let (project, buffer) = a_project_holding(REAL_SOURCE, cx).await;
        let fixes = what_a_run_would_have_left(&overlapping, REAL_SOURCE);
        cx.update(|cx| init(fixes, cx));

        let at = REAL_SOURCE
            .find("string(code)")
            .expect("the conversion the finding is on");
        assert_eq!(
            titles(&offered_at(&project, &buffer, at, cx).await),
            vec!["Format the number as a decimal".to_string()],
            "the well-formed alternative stands; the overlapping one is gone"
        );
    }

    /// Unsaved edits have moved every offset the analyzer measured, so
    /// nothing is offered until the file is saved and vetted again.
    #[gpui::test]
    async fn a_buffer_with_unsaved_edits_is_offered_nothing(cx: &mut TestAppContext) {
        let (project, buffer) = a_project_holding(REAL_SOURCE, cx).await;
        let fixes = what_a_run_would_have_left(REAL_OUTPUT, REAL_SOURCE);
        cx.update(|cx| init(fixes, cx));

        buffer.update(cx, |buffer, cx| {
            buffer.edit([(0..0, "// one line more\n")], None, cx)
        });
        let at = buffer
            .read_with(cx, |buffer, _| buffer.text())
            .find("2006-02-01")
            .expect("the format string");
        assert!(offered_at(&project, &buffer, at, cx).await.is_empty());
    }

    /// A file that is saved but is not the file vet read -- changed on disk,
    /// or checked out from under the report -- is offered nothing either: the
    /// fix carries the text it expects to replace, and it is not there.
    #[gpui::test]
    async fn a_fix_measured_against_other_text_is_not_offered(cx: &mut TestAppContext) {
        let changed = REAL_SOURCE.replace("2006-02-01", "2006-03-04");
        let (project, buffer) = a_project_holding(&changed, cx).await;
        let fixes = what_a_run_would_have_left(REAL_OUTPUT, REAL_SOURCE);
        cx.update(|cx| init(fixes, cx));

        let at = changed.find("2006-03-04").expect("the format string");
        assert!(
            offered_at(&project, &buffer, at, cx).await.is_empty(),
            "the format string vet asked to replace is not the one that is there"
        );
    }
}
