use anyhow::Result;
use gpui::{App, AppContext as _, Entity, Task};
use language::{Buffer, BufferSnapshot, CodeLabel};
use lsp::CompletionContext;
use project::lsp_store::CompletionDocumentation;
use project::{Completion, CompletionSource, InProcessCompletions, InProcessProject};
use ruff_db::files::{File, system_path_to_file};
use text::{PointUtf16, ToOffset as _};
use ty_ide::{CompletionCapabilities, CompletionInsertTextFormat, CompletionSettings};
use ty_project::SemanticDb as _;
use ty_python_semantic::ProgramEnvironment;

use crate::{Asked, PythonProject, TypesFromTy, what_was_asked};

/// One suggestion, already turned into the plain strings an editor shows, so
/// that nothing borrowed from the database escapes the lock it was read under.
pub(crate) struct Offered {
    pub(crate) name: String,
    pub(crate) new_text: String,
    /// The inferred type of what is being offered, where `ty` knows one.
    pub(crate) type_name: Option<String>,
    pub(crate) documentation: Option<String>,
}

impl InProcessCompletions for TypesFromTy {
    fn completions(
        &self,
        project: &InProcessProject,
        buffer: &Entity<Buffer>,
        position: PointUtf16,
        _context: &CompletionContext,
        cx: &mut App,
    ) -> Task<Result<Vec<Completion>>> {
        let nothing = || Task::ready(Ok(Vec::new()));

        let snapshot = buffer.read(cx).snapshot();
        let offset = position.to_offset(&snapshot);
        let Some(start) = member_being_typed(&snapshot, offset) else {
            return nothing();
        };
        let Some(asked) = what_was_asked(&project.worktree_roots, buffer, position, cx) else {
            return nothing();
        };
        let Some(project) = self.project_for(&asked.root) else {
            return nothing();
        };

        cx.background_spawn(async move {
            // What has been typed is replaced rather than appended to, so
            // accepting a suggestion leaves the member's name once.
            let replace_range = snapshot.anchor_before(start)..snapshot.anchor_after(offset);
            Ok(offer(&project, &asked)
                .into_iter()
                .map(|offered| {
                    let name_len = offered.name.len();
                    let label = match &offered.type_name {
                        Some(type_name) => format!("{}  {type_name}", offered.name),
                        None => offered.name.clone(),
                    };
                    Completion {
                        replace_range: replace_range.clone(),
                        new_text: offered.new_text,
                        label: CodeLabel::filtered(label, name_len, None, Vec::new()),
                        documentation: offered
                            .documentation
                            .map(|text| CompletionDocumentation::MultiLineMarkdown(text.into())),
                        source: CompletionSource::Custom,
                        icon_path: None,
                        icon_color: None,
                        match_start: Some(replace_range.start),
                        snippet_deduplication_key: None,
                        insert_text_mode: None,
                        confirm: None,
                        group: None,
                    }
                })
                .collect())
        })
    }
}

/// Where the member being typed starts, when the cursor is in one.
///
/// Only member access is answered here. Everything else a reader types is a
/// name the symbol index already offers, and two sources answering the same
/// name would put it in the menu twice; what the index cannot know, and `ty`
/// can, is which members the expression before the `.` actually has.
fn member_being_typed(snapshot: &BufferSnapshot, offset: usize) -> Option<usize> {
    let mut start = offset;
    let mut characters = snapshot.reversed_chars_at(offset);
    let before = loop {
        match characters.next() {
            Some(character) if is_name_character(character) => start -= character.len_utf8(),
            Some(character) => break character,
            None => return None,
        }
    };
    if before != '.' {
        return None;
    }
    // `1.` and `12.` are floats rather than member access, and what tells them
    // apart from `value1.` is the first character of the token before the dot.
    let mut token: Vec<char> = Vec::new();
    for character in characters {
        if !is_name_character(character) {
            break;
        }
        token.push(character);
    }
    if token.last().is_some_and(char::is_ascii_digit) {
        return None;
    }
    Some(start)
}

fn is_name_character(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

/// Asks `ty` what the expression before the cursor's `.` has on it.
///
/// Returns nothing rather than propagating a panic, for the same reason the
/// hover side does: a type-inference query that falls over is a menu that
/// misses some entries, and must not be the end of the editor being typed into.
pub(crate) fn offer(project: &PythonProject, asked: &Asked) -> Vec<Offered> {
    let Ok(mut database) = project.database.lock() else {
        return Vec::new();
    };
    if project.open.record(&asked.path, asked.text.clone()) {
        File::sync_path(&mut *database, &asked.path);
    }
    let database = &*database;

    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let Ok(file) = system_path_to_file(database, &asked.path) else {
            return Vec::new();
        };
        let program_file = database.program_file(file);
        let environment = ProgramEnvironment::from_file(program_file);
        // Names that are not imported yet are left to the symbol index: taking
        // them here would mean offering a name whose `import` line this source
        // has no way to insert.
        let settings = CompletionSettings {
            auto_import: false,
            complete_function_parentheses: false,
        };
        ty_ide::completion(
            database,
            &settings,
            CompletionCapabilities::default(),
            program_file,
            asked.offset,
        )
        .into_iter()
        .filter(|completion| {
            completion.import.is_none()
                && completion.insert_text_format == CompletionInsertTextFormat::PlainText
        })
        .map(|completion| {
            let name = completion.label().to_string();
            let new_text = completion
                .insert
                .as_deref()
                .unwrap_or(name.as_str())
                .to_string();
            let documentation = completion
                .documentation
                .as_ref()
                .map(|docstring| docstring.render_markdown())
                .filter(|text| !text.trim().is_empty());
            let type_name = completion
                .ty
                .map(|inferred| inferred.display(database, &environment).to_string());
            Offered {
                name,
                new_text,
                type_name,
                documentation,
            }
        })
        .collect()
    }))
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruff_db::system::SystemPathBuf;
    use ruff_text_size::TextSize;

    const SOURCE: &str = "class Circle:\n    radius: int\n\n    def area(self) -> float:\n        return 3.14 * self.radius * self.radius\n\n\nshape = Circle()\nshape.\n";

    fn offered_at(source: &str, offset: usize) -> Vec<Offered> {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join("asked.py");
        std::fs::write(&path, source).expect("a file to ask about");
        let Some(project) = PythonProject::discover(directory.path()) else {
            return Vec::new();
        };
        offer(
            &project,
            &Asked {
                root: directory.path().to_path_buf(),
                path: SystemPathBuf::from_path_buf(path).expect("a UTF-8 path"),
                text: source.to_string(),
                offset: TextSize::try_from(offset).expect("an offset that fits"),
            },
        )
    }

    #[test]
    fn a_member_is_offered_with_the_type_it_has() {
        let at = SOURCE.rfind("shape.").expect("the member access") + "shape.".len();
        let offered = offered_at(SOURCE, at);
        let names: Vec<&str> = offered.iter().map(|one| one.name.as_str()).collect();
        assert!(
            names.contains(&"radius"),
            "`Circle` has a `radius`, ty offered {names:?}"
        );
        assert!(
            names.contains(&"area"),
            "`Circle` has an `area`, ty offered {names:?}"
        );
        let radius = offered
            .iter()
            .find(|one| one.name == "radius")
            .expect("radius");
        assert_eq!(
            radius.type_name.as_deref(),
            Some("int"),
            "the type of the member is what makes this worth showing"
        );
    }

    /// A source that offered every name in the project would offer these too;
    /// the point of asking `ty` is that a type answers for its own members.
    #[test]
    fn a_name_the_type_does_not_have_is_not_offered() {
        let at = SOURCE.rfind("shape.").expect("the member access") + "shape.".len();
        let names: Vec<String> = offered_at(SOURCE, at)
            .into_iter()
            .map(|one| one.name)
            .collect();
        assert!(
            !names.iter().any(|name| name == "Circle"),
            "`Circle` is not a member of a `Circle`, ty offered {names:?}"
        );
        assert!(
            !names.iter().any(|name| name == "shape"),
            "`shape` is not a member of a `Circle`, ty offered {names:?}"
        );
    }
}

#[cfg(test)]
mod cursor_tests {
    use super::member_being_typed;
    use gpui::{AppContext as _, TestAppContext};
    use language::Buffer;

    fn member_start(cx: &mut TestAppContext, text_with_cursor: &str) -> Option<usize> {
        let offset = text_with_cursor
            .find('|')
            .expect("the test text must mark the cursor");
        let text = text_with_cursor.replace('|', "");
        let buffer = cx.new(|cx| Buffer::local(text, cx));
        cx.update(|cx| member_being_typed(&buffer.read(cx).snapshot(), offset))
    }

    #[gpui::test]
    fn only_a_member_access_is_answered(cx: &mut TestAppContext) {
        assert_eq!(member_start(cx, "shape.|"), Some(6));
        assert_eq!(member_start(cx, "shape.rad|"), Some(6));
        // A bare name is the symbol index's ground, not this source's.
        assert_eq!(member_start(cx, "sha|"), None);
        assert_eq!(member_start(cx, "shape = |"), None);
        // A float literal is not a member access.
        assert_eq!(member_start(cx, "x = 3.|"), None);
        assert_eq!(member_start(cx, "x = value1.|"), Some(11));
    }
}

#[cfg(test)]
mod buffer_tests {
    use gpui::TestAppContext;
    use project::{DEFAULT_COMPLETION_CONTEXT, Project};
    use serde_json::json;
    use settings::SettingsStore;
    use std::sync::Arc;

    const SOURCE: &str = "class Circle:\n    radius: int\n\n    def area(self) -> float:\n        return 3.14 * self.radius * self.radius\n\n\nshape = Circle()\nshape.\n";

    /// Drives the real path: a Python buffer in a project with no language
    /// server, asked through `Project::completions`.
    async fn completions_after_the_dot(cx: &mut TestAppContext) -> Vec<String> {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            crate::init(cx);
        });

        // The file is written twice over: once on the real disk, which is
        // where `ty` reads a project from, and once into the editor's own fake
        // filesystem at the same absolute path, because a real filesystem in a
        // gpui test brings a watcher thread the test scheduler rejects.
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join("shapes.py");
        std::fs::write(&path, SOURCE).expect("a file to open");

        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree(directory.path(), json!({ "shapes.py": SOURCE }))
            .await;
        let fs: Arc<dyn fs::Fs> = fs;
        let project = Project::test(fs, [directory.path()], cx).await;
        let buffer = project
            .update(cx, |project, cx| project.open_local_buffer(&path, cx))
            .await
            .expect("the file should open");
        cx.executor().run_until_parked();

        let offset = SOURCE.rfind("shape.").expect("the member access") + "shape.".len();
        let responses = project
            .update(cx, |project, cx| {
                project.completions(&buffer, offset, DEFAULT_COMPLETION_CONTEXT, cx)
            })
            .await
            .expect("completions should not fail");

        responses
            .iter()
            .flat_map(|response| &response.completions)
            .map(|completion| completion.new_text.clone())
            .collect()
    }

    #[gpui::test]
    async fn offers_the_members_of_the_type_with_no_language_server(cx: &mut TestAppContext) {
        let completions = completions_after_the_dot(cx).await;
        assert!(
            completions.iter().any(|name| name == "radius"),
            "`Circle` has a `radius`, got {completions:?}"
        );
        assert!(
            completions.iter().any(|name| name == "area"),
            "`Circle` has an `area`, got {completions:?}"
        );
        assert!(
            !completions.iter().any(|name| name == "Circle"),
            "`Circle` is not a member of a `Circle`, got {completions:?}"
        );
    }
}
