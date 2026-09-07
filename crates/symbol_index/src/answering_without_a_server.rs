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

    const SHELVES: &str = "\
pub struct Shelf;

pub fn take_stock() -> u32 {
    1
}
";

    const USES: &str = "\
pub fn open_up() {
    let shelf = Shelf;
    let held = take_stock();
}
";

    /// A project whose second file names both a type and a function that its
    /// first file declares, with the provider built over its index.
    async fn a_project_that_names_a_type_and_a_function(
        cx: &mut TestAppContext,
    ) -> (
        tempfile::TempDir,
        tempfile::TempDir,
        Entity<Project>,
        Entity<Buffer>,
        IndexFirst,
    ) {
        init_test(cx);
        let (project_at, project) = a_project(&[("one.rs", SHELVES), ("two.rs", USES)], cx).await;
        let index_at = tempfile::tempdir().expect("a directory for the index's own files");
        let index = cx.update(|cx| {
            crate::ensure_index_at(project.clone(), index_at.path().join("symbol_index"), cx)
        });
        // The index is built in the background, and reading it before the
        // build has finished reads an empty one.
        cx.run_until_parked();

        let using = project
            .update(cx, |project, cx| {
                project.open_local_buffer(project_at.path().join("two.rs"), cx)
            })
            .await
            .expect("the file that names them opens");
        let over = IndexFirst::over(&project, &index);
        (project_at, index_at, project, using, over)
    }

    /// Where a go-to of `kind` lands, as the reader would see it, or nothing
    /// where the question was answered with nowhere to go.
    async fn where_a_goto_lands(
        over: &IndexFirst,
        buffer: &Entity<Buffer>,
        position: text::Anchor,
        kind: editor::GotoDefinitionKind,
        cx: &mut TestAppContext,
    ) -> Option<(String, String)> {
        let answered = cx
            .update(|cx| over.definitions(buffer, position, kind, cx))
            .expect("the provider always hands the question on")
            .await
            .expect("and it answers");
        let found = answered?;
        let first = found.first()?;
        Some(what_it_points_at(&first.target, cx))
    }

    #[gpui::test]
    async fn each_go_to_kind_either_lands_where_the_index_says_or_says_it_cannot(
        cx: &mut TestAppContext,
    ) {
        let (_project_at, _index_at, _project, using, over) =
            a_project_that_names_a_type_and_a_function(cx).await;
        let at_the_type = at_the_name(&using, "Shelf", cx);
        let at_the_function = at_the_name(&using, "take_stock", cx);

        // A declaration is the very thing the index records, so the key lands.
        assert_eq!(
            where_a_goto_lands(
                &over,
                &using,
                at_the_function,
                editor::GotoDefinitionKind::Declaration,
                cx
            )
            .await,
            Some(("one.rs".to_string(), "take_stock".to_string())),
            "go to declaration lands on the declaring line"
        );

        // A type definition is reachable from the type's own name.
        assert_eq!(
            where_a_goto_lands(
                &over,
                &using,
                at_the_type,
                editor::GotoDefinitionKind::Type,
                cx
            )
            .await,
            Some(("one.rs".to_string(), "Shelf".to_string())),
            "go to type definition lands on the type's declaration"
        );

        // And from nothing else: what type a function's result has needs
        // inference, so the reader is told rather than sent somewhere wrong.
        assert_eq!(
            where_a_goto_lands(
                &over,
                &using,
                at_the_function,
                editor::GotoDefinitionKind::Type,
                cx
            )
            .await,
            None,
            "no type is claimed for the name of a function"
        );
        let said = cx
            .update(|cx| {
                over.why_a_goto_finds_nothing(
                    editor::GotoDefinitionKind::Type,
                    &using,
                    at_the_function,
                    cx,
                )
            })
            .expect("the reader is told why");
        assert!(
            said.contains("type") && said.contains("language server"),
            "{said}"
        );

        // An implementation is a relation the index does not record, for any
        // name at all.
        assert_eq!(
            where_a_goto_lands(
                &over,
                &using,
                at_the_type,
                editor::GotoDefinitionKind::Implementation,
                cx
            )
            .await,
            None,
            "no implementation is claimed, even for a type the index knows"
        );
        let said = cx
            .update(|cx| {
                over.why_a_goto_finds_nothing(
                    editor::GotoDefinitionKind::Implementation,
                    &using,
                    at_the_type,
                    cx,
                )
            })
            .expect("the reader is told why");
        assert!(
            said.contains("implement") && said.contains("language server"),
            "{said}"
        );
    }

    /// The words a range covers in the buffer it points into.
    fn words_in(
        buffer: &Entity<Buffer>,
        range: std::ops::Range<text::Anchor>,
        cx: &mut TestAppContext,
    ) -> String {
        buffer.read_with(cx, |buffer, _| {
            let snapshot = buffer.snapshot();
            let from = range.start.to_offset(&snapshot);
            let to = range.end.to_offset(&snapshot);
            snapshot.text_for_range(from..to).collect()
        })
    }

    #[gpui::test]
    async fn holding_cmd_over_a_name_underlines_it_and_nothing_the_index_declines(
        cx: &mut TestAppContext,
    ) {
        let (_project_at, _index_at, _project, using, over) =
            a_project_that_names_a_type_and_a_function(cx).await;

        let at_the_function = at_the_name(&using, "take_stock", cx);
        let underlined = cx
            .update(|cx| over.link_candidate_range(&using, at_the_function, cx))
            .expect("a name the index can open reads as navigable");
        assert_eq!(
            words_in(&using, underlined, cx),
            "take_stock",
            "the whole name and nothing around it"
        );

        // A local the index never declared: nothing is underlined, so nothing
        // reads as navigable where clicking it would not move the reader.
        let at_a_local = at_the_name(&using, "held", cx);
        assert!(
            cx.update(|cx| over.link_candidate_range(&using, at_a_local, cx))
                .is_none(),
            "a name the index has no declaration for is not underlined"
        );
    }

    #[gpui::test]
    async fn a_name_the_project_declares_twice_is_neither_underlined_nor_gone_to(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let twice = "pub fn hand_over() {}\n";
        let (project_at, project) = a_project(
            &[
                ("one.rs", twice),
                ("two.rs", twice),
                ("three.rs", "pub fn open_up() {\n    hand_over();\n}\n"),
            ],
            cx,
        )
        .await;
        let index_at = tempfile::tempdir().expect("a directory for the index's own files");
        let index = cx.update(|cx| {
            crate::ensure_index_at(project.clone(), index_at.path().join("symbol_index"), cx)
        });
        cx.run_until_parked();
        let calling = project
            .update(cx, |project, cx| {
                project.open_local_buffer(project_at.path().join("three.rs"), cx)
            })
            .await
            .expect("the calling file opens");
        let over = IndexFirst::over(&project, &index);
        let position = at_the_name(&calling, "hand_over", cx);

        assert!(
            cx.update(|cx| over.link_candidate_range(&calling, position, cx))
                .is_none(),
            "an ambiguous name is not underlined"
        );
        assert_eq!(
            where_a_goto_lands(
                &over,
                &calling,
                position,
                editor::GotoDefinitionKind::Symbol,
                cx
            )
            .await,
            None,
            "and it is not gone to either: one of the two would be a guess"
        );
    }

    /// Where a place points, down to the row, so that a position given for a
    /// file nobody edited can be checked and not just its file name.
    fn row_and_words_of(
        location: &language::Location,
        cx: &mut TestAppContext,
    ) -> (String, u32, String) {
        location.buffer.read_with(cx, |buffer, _| {
            let snapshot = buffer.snapshot();
            let path = buffer
                .file()
                .expect("a location points into a file of the project")
                .path()
                .to_string();
            let from = location.range.start.to_offset(&snapshot);
            let to = location.range.end.to_offset(&snapshot);
            (
                path,
                snapshot.offset_to_point(from).row,
                snapshot.text_for_range(from..to).collect(),
            )
        })
    }

    #[gpui::test]
    async fn references_still_answer_with_an_unsaved_edit_and_leave_out_only_that_file(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let (project_at, project) = a_project(
            &[("one.rs", DECLARES), ("two.rs", CALLS), ("three.rs", CALLS)],
            cx,
        )
        .await;
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

        // One keystroke, which moves every row of this file below it and none
        // of any other file's.
        calling.update(cx, |buffer, cx| {
            buffer.edit(
                [(0..0, "// a line the file on disk does not have\n")],
                None,
                cx,
            );
        });
        cx.run_until_parked();
        assert!(
            calling.read_with(cx, |buffer, _| buffer.is_dirty()),
            "the file is being edited"
        );

        let position = at_the_name(&calling, "take_stock", cx);
        let answered = cx
            .update(|cx| over.references(&calling, position, cx))
            .expect("the index takes the question")
            .await
            .expect("and answers it")
            .expect("with places, rather than collapsing to nothing");

        let places: Vec<(String, u32, String)> = answered
            .iter()
            .map(|location| row_and_words_of(location, cx))
            .collect();
        assert!(
            places.iter().all(|(path, _, _)| path != "two.rs"),
            "the file being edited is left out rather than pointed at wrongly: {places:?}"
        );
        assert!(
            places.contains(&("three.rs".to_string(), 1, "take_stock".to_string())),
            "the call in a file nobody edited is there, on its own row: {places:?}"
        );
        assert!(
            places.iter().all(|(_, _, words)| words == "take_stock"),
            "every place given is the name itself, so no position is stale: {places:?}"
        );
    }
}
