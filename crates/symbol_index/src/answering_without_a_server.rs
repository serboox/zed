#[cfg(test)]
mod tests {
    use editor::SemanticsProvider as _;
    use fs::FakeFs;
    use gpui::{Entity, TestAppContext};
    use language::Buffer;
    use project::Project;
    use settings::SettingsStore;
    use text::ToOffset as _;

    use crate::call_hierarchy;
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

    const SHOP: &str = "\
pub fn take_stock() -> u32 {
    1
}

pub fn hand_over() {}
";

    /// One file that calls what another declares, three ways over: from a
    /// method nested in an `impl`, from a free function, and from outside
    /// every declaration in the file.
    const STORE: &str = "\
pub struct Shelf;

impl Shelf {
    pub fn restock(&self) {
        take_stock();
    }
}

pub fn open_up() {
    take_stock();
    hand_over();
    let held = LIMIT;
}

seed!(take_stock);
use crate::shop::take_stock as counted_again;

pub const LIMIT: u32 = 10;
";

    /// A project with no language server anywhere near it -- none is
    /// registered, none is started -- and its index built over the real
    /// directory the files are in.
    async fn a_project_with_no_server(
        cx: &mut TestAppContext,
    ) -> (
        tempfile::TempDir,
        tempfile::TempDir,
        Entity<Project>,
        Entity<crate::SymbolIndex>,
    ) {
        init_test(cx);
        let (project_at, project) = a_project(&[("shop.rs", SHOP), ("store.rs", STORE)], cx).await;
        let index_at = tempfile::tempdir().expect("a directory for the index's own files");
        let index = cx.update(|cx| {
            crate::ensure_index_at(project.clone(), index_at.path().join("symbol_index"), cx)
        });
        // The index is built in the background, and reading it before the
        // build has finished reads an empty one.
        cx.run_until_parked();
        (project_at, index_at, project, index)
    }

    /// What a caller or callee is, as a reader would name it.
    fn named(found: &[call_hierarchy::Called]) -> Vec<(String, String)> {
        found
            .iter()
            .map(|one| (one.name.clone(), one.kind.clone()))
            .collect()
    }

    #[gpui::test]
    async fn incoming_calls_name_the_calling_function_with_no_server(cx: &mut TestAppContext) {
        let (_project_at, _index_at, _project, index) = a_project_with_no_server(cx).await;

        let callers = cx
            .update(|cx| call_hierarchy::incoming(&index, "take_stock", cx))
            .expect("the index takes the question")
            .await;
        let named = named(&callers);

        assert!(
            named.contains(&("open_up".to_string(), "function_item".to_string())),
            "the free function that calls it: {named:?}"
        );
        // The declaration the call is written inside, not the `impl` block
        // that declaration is written inside.
        assert!(
            named.contains(&("restock".to_string(), "function_item".to_string())),
            "the method that calls it, and not the impl block: {named:?}"
        );
        assert!(
            !named.iter().any(|(_, kind)| kind == "impl_item"),
            "the impl block is not the caller: {named:?}"
        );
        // What a starting line alone cannot get right: the names written after
        // `open_up` are not written inside it, and the function is named once.
        assert!(
            callers.iter().any(|one| one.at_file_scope()),
            "a call outside every declaration is said to be exactly that: {named:?}"
        );
        assert_eq!(
            named.iter().filter(|(name, _)| name == "open_up").count(),
            1,
            "{named:?}"
        );
    }

    #[gpui::test]
    async fn outgoing_calls_name_what_the_function_calls_with_no_server(cx: &mut TestAppContext) {
        let (_project_at, _index_at, _project, index) = a_project_with_no_server(cx).await;

        let called = cx
            .update(|cx| call_hierarchy::outgoing(&index, "open_up", cx))
            .expect("the index takes the question")
            .await;
        let named = named(&called);

        assert!(
            named.contains(&("take_stock".to_string(), "function_item".to_string())),
            "{named:?}"
        );
        assert!(
            named.contains(&("hand_over".to_string(), "function_item".to_string())),
            "{named:?}"
        );
        // A name read as a value is not a call, and the call written inside the
        // method above is not this function's.
        assert!(
            !named.iter().any(|(name, _)| name == "LIMIT"),
            "a constant read is not a call: {named:?}"
        );
        assert_eq!(named.len(), 2, "{named:?}");
    }

    /// The other half of the same question: what the method calls is the
    /// method's own, and nothing the file's other functions call.
    #[gpui::test]
    async fn outgoing_calls_are_bounded_by_the_declaration_they_were_asked_about(
        cx: &mut TestAppContext,
    ) {
        let (_project_at, _index_at, _project, index) = a_project_with_no_server(cx).await;

        let called = cx
            .update(|cx| call_hierarchy::outgoing(&index, "take_stock", cx))
            .expect("the index takes the question")
            .await;
        assert!(
            called.is_empty(),
            "the declaration calls nothing: {:?}",
            named(&called)
        );
    }

    /// The root of the tree: the declaration the cursor is on.
    #[gpui::test]
    async fn the_declaration_under_the_cursor_roots_the_tree(cx: &mut TestAppContext) {
        let (project_at, _index_at, project, index) = a_project_with_no_server(cx).await;
        let calling = project
            .update(cx, |project, cx| {
                project.open_local_buffer(project_at.path().join("store.rs"), cx)
            })
            .await
            .expect("the calling file opens");
        let position = calling.read_with(cx, |buffer, _| {
            let snapshot = buffer.snapshot();
            let at = snapshot
                .text()
                .find("open_up")
                .expect("the name is in this buffer");
            snapshot.offset_to_point_utf16(at + 1)
        });

        let root = cx
            .update(|cx| call_hierarchy::declaration_under(&index, &calling, position, cx))
            .expect("the index knows where this name is declared");
        assert_eq!(root.name, "open_up");
        assert_eq!(root.kind, "function_item");
        assert!(!root.at_file_scope());
    }
}
