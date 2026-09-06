#[cfg(test)]
mod tests {
    use editor::SemanticsProvider as _;
    use fs::FakeFs;
    use gpui::{Entity, TestAppContext};
    use language::Buffer;
    use project::Project;
    use settings::SettingsStore;
    use text::ToOffset as _;

    use crate::index_semantics::IndexFirst;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
    }

    /// A project that exists twice over at one path, the way this crate's own
    /// tests build one: the editor reads the in-memory filesystem every test
    /// here uses, and the index reads the real directory, because it walks the
    /// disk with the standard library rather than through the editor.
    async fn a_project(
        files: &[(&str, &str)],
        cx: &mut TestAppContext,
    ) -> (tempfile::TempDir, Entity<Project>) {
        let held = tempfile::tempdir().expect("a directory to put a project in");
        let mut tree = serde_json::Map::new();
        for (name, contents) in files {
            std::fs::write(held.path().join(name), contents).expect("a project file on disk");
            tree.insert(
                (*name).to_string(),
                serde_json::Value::String((*contents).to_string()),
            );
        }
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(held.path(), serde_json::Value::Object(tree))
            .await;
        let project = Project::test(fs, [held.path()], cx).await;
        (held, project)
    }

    const DECLARES: &str = "pub fn take_stock() -> u32 {\n    1\n}\n";
    const CALLS: &str = "pub fn main() {\n    let held = take_stock();\n}\n";

    /// The project, its index, and the buffer of the file that does the
    /// calling -- with the provider built over both, exactly as `init` builds
    /// it for every editor.
    async fn a_project_that_calls_what_another_file_declares(
        cx: &mut TestAppContext,
    ) -> (
        tempfile::TempDir,
        tempfile::TempDir,
        Entity<Project>,
        Entity<Buffer>,
        IndexFirst,
    ) {
        init_test(cx);
        let (project_at, project) = a_project(&[("one.rs", DECLARES), ("two.rs", CALLS)], cx).await;

        let index_at = tempfile::tempdir().expect("a directory for the index's own files");
        let index = cx.update(|cx| {
            crate::ensure_index_at(project.clone(), index_at.path().join("symbol_index"), cx)
        });
        cx.run_until_parked();

        let calling = project
            .update(cx, |project, cx| {
                project.open_local_buffer(project_at.path().join("two.rs"), cx)
            })
            .await
            .expect("the calling file opens");
        let over = IndexFirst::over(&project, &index);
        (project_at, index_at, project, calling, over)
    }

    /// Where `name` is written in the buffer, as an anchor the provider takes.
    fn at_the_name(buffer: &Entity<Buffer>, name: &str, cx: &mut TestAppContext) -> text::Anchor {
        buffer.read_with(cx, |buffer, _| {
            let snapshot = buffer.snapshot();
            let text = snapshot.text();
            let at = text.find(name).expect("the name is in this buffer");
            snapshot.anchor_before(at + 1)
        })
    }

    /// What a location points at, as the reader would see it: which file, and
    /// which words.
    fn what_it_points_at(
        location: &language::Location,
        cx: &mut TestAppContext,
    ) -> (String, String) {
        location.buffer.read_with(cx, |buffer, _| {
            let snapshot = buffer.snapshot();
            let path = buffer
                .file()
                .expect("a location points into a file of the project")
                .path()
                .to_string();
            let range =
                location.range.start.to_offset(&snapshot)..location.range.end.to_offset(&snapshot);
            (path, snapshot.text_for_range(range).collect())
        })
    }

    #[gpui::test]
    async fn go_to_definition_lands_on_the_declaration_with_no_server(cx: &mut TestAppContext) {
        let (_project_at, _index_at, _project, calling, over) =
            a_project_that_calls_what_another_file_declares(cx).await;
        let position = at_the_name(&calling, "take_stock", cx);

        let answered = cx
            .update(|cx| {
                over.definitions(&calling, position, editor::GotoDefinitionKind::Symbol, cx)
            })
            .expect("the index takes the question")
            .await
            .expect("and answers it")
            .expect("with somewhere to go");

        assert_eq!(answered.len(), 1, "{answered:?}");
        let (path, words) = what_it_points_at(&answered[0].target, cx);
        assert_eq!(path, "one.rs", "the declaring file");
        assert_eq!(words, "take_stock", "the name itself, not its line");
    }

    #[gpui::test]
    async fn find_all_references_answers_from_the_index_with_no_server(cx: &mut TestAppContext) {
        let (_project_at, _index_at, _project, calling, over) =
            a_project_that_calls_what_another_file_declares(cx).await;
        let position = at_the_name(&calling, "take_stock", cx);

        let answered = cx
            .update(|cx| over.references(&calling, position, cx))
            .expect("the index takes the question")
            .await
            .expect("and answers it")
            .expect("with places");

        let places: Vec<(String, String)> = answered
            .iter()
            .map(|location| what_it_points_at(location, cx))
            .collect();
        assert!(
            places
                .iter()
                .any(|(path, words)| path == "two.rs" && words == "take_stock"),
            "the call is one of the places: {places:?}"
        );
        assert!(
            places.iter().all(|(_, words)| words == "take_stock"),
            "every place is the name and nothing else: {places:?}"
        );
    }

    #[gpui::test]
    async fn hovering_shows_the_line_that_declares_the_name(cx: &mut TestAppContext) {
        let (_project_at, _index_at, _project, calling, over) =
            a_project_that_calls_what_another_file_declares(cx).await;
        let position = at_the_name(&calling, "take_stock", cx);

        let answered = cx
            .update(|cx| over.hover(&calling, position, cx))
            .expect("the index takes the question")
            .await
            .expect("and answers it");

        let shown: Vec<String> = answered
            .iter()
            .flat_map(|hover| hover.contents.iter())
            .map(|block| block.text.clone())
            .collect();
        assert!(
            shown
                .iter()
                .any(|text| text == "pub fn take_stock() -> u32 {"),
            "the declaring line is quoted: {shown:?}"
        );
        assert!(
            shown.iter().any(|text| text.contains("one.rs")),
            "and where it lives is said: {shown:?}"
        );
    }
}
