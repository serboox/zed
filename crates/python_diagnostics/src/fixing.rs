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

use crate::watching::RUFF_SERVER_ID;
use crate::{Fix, Reported};

/// The fixes the last run found, kept so that a reader whose cursor is on a
/// finding can be offered the one ruff already computed for it.
///
/// Replaced whole each time rather than merged: one run covers the whole
/// project, so a file the new report says nothing about has no fixes, and
/// keeping the old ones would offer a fix for a finding that is gone.
#[derive(Default)]
pub struct Fixes {
    by_file: HashMap<PathBuf, Vec<Anchored>>,
}

/// A fix and the place the finding it belongs to was reported at.
///
/// The two can be far apart -- ruff's fix for an unused import deletes the
/// whole statement while the finding sits on the name -- so what decides
/// whether to offer a fix is where the finding is, which is where the
/// reader's cursor will be.
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

/// Offers ruff's own fixes as quick fixes, with no language server anywhere.
pub fn init(fixes: Arc<Mutex<Fixes>>, cx: &mut App) {
    register_in_process_code_actions(Arc::new(SuggestedByRuff(fixes)), cx);
}

struct SuggestedByRuff(Arc<Mutex<Fixes>>);

impl InProcessCodeActions for SuggestedByRuff {
    /// The same id the diagnostics are filed under. A fix and the finding it
    /// fixes come from one run of one tool, and one id is what tells the
    /// editor they belong together.
    fn server_id(&self) -> lsp::LanguageServerId {
        RUFF_SERVER_ID
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

impl SuggestedByRuff {
    fn offered_for(
        &self,
        buffer: &Entity<Buffer>,
        asked_about: Range<PointUtf16>,
        cx: &App,
    ) -> Option<Vec<CodeAction>> {
        let read = buffer.read(cx);
        // The columns in the report were measured on the file as it was
        // saved, which is when ruff ran. Unsaved edits have moved them, and a
        // replacement made against moved offsets overwrites text ruff never
        // looked at.
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
                    server_id: RUFF_SERVER_ID,
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
/// somewhere on the line the finding is on, not on the exact character ruff
/// underlined.
fn on_the_same_lines(at: lsp::Range, asked_about: &Range<PointUtf16>) -> bool {
    at.start.line <= asked_about.end.row && at.end.line >= asked_about.start.row
}

fn in_the_buffer(range: lsp::Range, snapshot: &BufferSnapshot) -> Range<PointUtf16> {
    let range = range_from_lsp(range);
    snapshot.clip_point_utf16(range.start, Bias::Left)
        ..snapshot.clip_point_utf16(range.end, Bias::Left)
}

/// Whether the text ruff measured is still the text that is there.
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
    use crate::what_ruff_reported;
    use gpui::TestAppContext;
    use language::{Language, LanguageConfig, LanguageMatcher};
    use project::Project;
    use serde_json::json;
    use settings::SettingsStore;
    use smol::stream::StreamExt as _;
    use std::path::Path;
    use util::path;

    const REAL_OUTPUT: &str = include_str!("../test_data/ruff-check-fixes.json");
    const REAL_SOURCE: &str = include_str!("../test_data/ruff-check-fixes.source");

    /// The store as a real run leaves it: ruff's own output, parsed by the
    /// reader that runs in the editor, over the exact text ruff measured.
    fn what_a_run_would_have_left(output: &str, source: &str) -> Arc<Mutex<Fixes>> {
        let reported = what_ruff_reported(output, Path::new(path!("/project")), |path| {
            (path == Path::new(path!("/project/lint_me.py"))).then(|| source.to_string())
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
        let project = a_project_over(on_disk, cx).await;
        let buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer(path!("/project/lint_me.py"), cx)
            })
            .await
            .expect("the file should open");
        cx.executor().run_until_parked();
        (project, buffer)
    }

    async fn a_project_over(on_disk: &str, cx: &mut TestAppContext) -> Entity<Project> {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({ "lint_me.py": on_disk }))
            .await;
        let fs: Arc<dyn fs::Fs> = fs;
        Project::test(fs, [Path::new(path!("/project"))], cx).await
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

    /// The whole point, end to end: a fix ruff called safe reaches the reader
    /// as a quick fix under ruff's own name for it, and taking it writes
    /// exactly what ruff asked for -- on a line where a byte count, a
    /// character count and a UTF-16 count are three different numbers.
    #[gpui::test]
    async fn a_safe_fix_is_offered_and_writes_its_own_text(cx: &mut TestAppContext) {
        // Three counts of the same place, all different.
        let line = REAL_SOURCE
            .lines()
            .nth(5)
            .expect("the line the semicolon is on");
        let semicolon = line.find(';').expect("the semicolon");
        assert_eq!(line[..semicolon].len(), 33, "bytes");
        assert_eq!(line[..semicolon].chars().count(), 19, "characters");
        assert_eq!(line[..semicolon].encode_utf16().count(), 21, "UTF-16 units");

        let (project, buffer) = a_project_holding(REAL_SOURCE, cx).await;
        let fixes = what_a_run_would_have_left(REAL_OUTPUT, REAL_SOURCE);
        cx.update(|cx| init(fixes, cx));

        let at = REAL_SOURCE
            .find(';')
            .expect("the semicolon the finding is on");
        let offered = offered_at(&project, &buffer, at, cx).await;
        assert_eq!(
            titles(&offered),
            vec!["Remove unnecessary semicolon".to_string()]
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
            REAL_SOURCE.replace("\u{1f525}\";", "\u{1f525}\""),
            "the semicolon after the emoji goes and nothing else moves"
        );
    }

    /// A fix ruff will not vouch for is not offered. `E711`'s rewrite of
    /// `== None` to `is None` is graded unsafe because it changes what the
    /// comparison means, and ruff itself will not apply it unasked.
    #[gpui::test]
    async fn an_unsafe_fix_is_not_offered(cx: &mut TestAppContext) {
        let (project, buffer) = a_project_holding(REAL_SOURCE, cx).await;
        let fixes = what_a_run_would_have_left(REAL_OUTPUT, REAL_SOURCE);
        cx.update(|cx| init(fixes, cx));

        assert!(
            REAL_OUTPUT.contains("\"applicability\": \"unsafe\""),
            "the fixture is only meaningful while it still carries an unsafe fix"
        );
        let at = REAL_SOURCE
            .find("== None")
            .expect("the comparison the unsafe fix is for");
        assert!(
            offered_at(&project, &buffer, at, cx).await.is_empty(),
            "ruff graded that fix unsafe"
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
            .find("return total")
            .expect("a line no finding is on");
        assert!(
            offered_at(&project, &buffer, elsewhere, cx)
                .await
                .is_empty()
        );
    }

    /// Unsaved edits have moved every column ruff measured, so nothing is
    /// offered until the file is saved and linted again.
    #[gpui::test]
    async fn a_buffer_with_unsaved_edits_is_offered_nothing(cx: &mut TestAppContext) {
        let (project, buffer) = a_project_holding(REAL_SOURCE, cx).await;
        let fixes = what_a_run_would_have_left(REAL_OUTPUT, REAL_SOURCE);
        cx.update(|cx| init(fixes, cx));

        buffer.update(cx, |buffer, cx| {
            buffer.edit([(0..0, "# one line more\n")], None, cx)
        });
        let at = buffer
            .read_with(cx, |buffer, _| buffer.text())
            .find(';')
            .expect("the semicolon");
        assert!(offered_at(&project, &buffer, at, cx).await.is_empty());
    }

    /// A file that is saved but is not the file ruff read -- changed on disk,
    /// or checked out from under the report -- is offered nothing either: the
    /// fix carries the text it expects to replace, and it is not there.
    #[gpui::test]
    async fn a_fix_measured_against_other_text_is_not_offered(cx: &mut TestAppContext) {
        let changed = REAL_SOURCE.replace("import os", "import sys");
        let (project, buffer) = a_project_holding(&changed, cx).await;
        let fixes = what_a_run_would_have_left(REAL_OUTPUT, REAL_SOURCE);
        cx.update(|cx| init(fixes, cx));

        let at = changed.find("import sys").expect("the import");
        assert!(
            !titles(&offered_at(&project, &buffer, at, cx).await)
                .iter()
                .any(|title| title.contains("Remove unused import")),
            "the import ruff asked to remove is not the one that is there"
        );
    }

    /// A language server's answer is taken whole. Where one is running and
    /// answers, this source stays silent rather than adding a stale fix
    /// beside a live one.
    #[gpui::test]
    async fn a_language_servers_answer_leaves_this_source_silent(cx: &mut TestAppContext) {
        let project = a_project_over(REAL_SOURCE, cx).await;
        let languages = project.read_with(cx, |project, _| project.languages().clone());
        languages.add(Arc::new(Language::new(
            LanguageConfig {
                name: "Python".into(),
                matcher: LanguageMatcher {
                    path_suffixes: vec!["py".to_string()],
                    ..Default::default()
                },
                ..Default::default()
            },
            None,
        )));
        let mut servers = languages.register_fake_lsp(
            "Python",
            language::FakeLspAdapter {
                capabilities: lsp::ServerCapabilities {
                    code_action_provider: Some(lsp::CodeActionProviderCapability::Simple(true)),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let (buffer, _held) = project
            .update(cx, |project, cx| {
                project.open_local_buffer_with_lsp(path!("/project/lint_me.py"), cx)
            })
            .await
            .expect("the file should open");
        cx.executor().run_until_parked();
        let server = servers.next().await.expect("the fake server should start");
        let mut asked = server.set_request_handler::<lsp::request::CodeActionRequest, _, _>(
            |_, _| async move {
                Ok(Some(vec![lsp::CodeActionOrCommand::CodeAction(
                    lsp::CodeAction {
                        title: "the server's own fix".to_string(),
                        ..Default::default()
                    },
                )]))
            },
        );

        let fixes = what_a_run_would_have_left(REAL_OUTPUT, REAL_SOURCE);
        cx.update(|cx| init(fixes, cx));
        let at = REAL_SOURCE.find(';').expect("the semicolon");
        let offered = offered_at(&project, &buffer, at, cx).await;

        asked.next().await.expect("the server should be asked");
        assert_eq!(
            titles(&offered),
            vec!["the server's own fix".to_string()],
            "the server answered, so ruff's fix is not added beside it"
        );
    }
}
