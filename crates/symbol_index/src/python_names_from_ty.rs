#[cfg(test)]
mod tests {
    use editor::SemanticsProvider as _;
    use fs::FakeFs;
    use gpui::{Entity, TestAppContext};
    use language::Buffer;
    use project::Project;
    use settings::SettingsStore;
    use std::path::Path;
    use text::ToOffset as _;

    use crate::index_semantics::IndexFirst;

    /// A package whose class is imported under two different names elsewhere,
    /// and a second class that happens to have a member of the same name as
    /// the first one's -- which is what tells "the same word" apart from "the
    /// same symbol".
    fn a_python_project() -> Vec<(&'static str, &'static str)> {
        vec![
            // `ty` finds a project by ascending from the file until it finds
            // one of these. Without it the search leaves the test's own
            // directory and whatever it lands on decides which files `ty`
            // considers part of the project.
            (
                "pyproject.toml",
                "[project]\nname = \"asked\"\nversion = \"0\"\n",
            ),
            ("pkg/__init__.py", "\n"),
            ("pkg/mod.py", "class Thing:\n    label: int = 1\n"),
            (
                "uses.py",
                "from pkg.mod import Thing\n\n\ndef build() -> Thing:\n    return Thing()\n",
            ),
            (
                "other.py",
                "class Bucket:\n    label: str = \"kept\"\n\n\ndef describe(bucket: Bucket) -> str:\n    return bucket.label\n",
            ),
            (
                "main.py",
                "from pkg.mod import Thing as Other\n\nheld = Other()\nname = held.label\n",
            ),
        ]
    }

    /// A project that exists twice over at one path, the way this crate's own
    /// tests build one: the editor reads the in-memory filesystem, and both
    /// the index and `ty` read the real directory, because each walks the disk
    /// with the standard library rather than through the editor.
    ///
    /// Registers `python_types` as `main` does, so the provider is asked the
    /// same three-way question a reader's editor asks it.
    async fn a_project_with_ty_and_the_index(
        files: &[(&str, &str)],
        cx: &mut TestAppContext,
    ) -> (
        tempfile::TempDir,
        tempfile::TempDir,
        Entity<Project>,
        IndexFirst,
    ) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            python_types::init(cx);
        });

        let held = tempfile::tempdir().expect("a directory to put a project in");
        let mut tree = serde_json::Map::new();
        for (name, contents) in files {
            let at = held.path().join(name);
            if let Some(parent) = at.parent() {
                std::fs::create_dir_all(parent).expect("a package directory on disk");
            }
            std::fs::write(&at, contents).expect("a project file on disk");
            let mut into = &mut tree;
            let mut segments = name.split('/').peekable();
            while let Some(segment) = segments.next() {
                if segments.peek().is_none() {
                    into.insert(
                        segment.to_string(),
                        serde_json::Value::String((*contents).to_string()),
                    );
                    break;
                }
                into = into
                    .entry(segment.to_string())
                    .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
                    .as_object_mut()
                    .expect("a directory in the tree stays a directory");
            }
        }
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(held.path(), serde_json::Value::Object(tree))
            .await;
        let project = Project::test(fs, [held.path()], cx).await;

        let index_at = tempfile::tempdir().expect("a directory for the index's own files");
        let index = cx.update(|cx| {
            crate::ensure_index_at(project.clone(), index_at.path().join("symbol_index"), cx)
        });
        // The index is built in the background, and reading it before the build
        // has finished reads an empty one.
        cx.run_until_parked();

        let over = IndexFirst::over(&project, &index);
        (held, index_at, project, over)
    }

    async fn opened(
        project: &Entity<Project>,
        at: &Path,
        cx: &mut TestAppContext,
    ) -> Entity<Buffer> {
        project
            .update(cx, |project, cx| project.open_local_buffer(at, cx))
            .await
            .expect("the file opens")
    }

    /// An anchor just inside the first occurrence of `text` in the buffer.
    fn at_the_word(buffer: &Entity<Buffer>, text: &str, cx: &mut TestAppContext) -> text::Anchor {
        buffer.read_with(cx, |buffer, _| {
            let snapshot = buffer.snapshot();
            let at = snapshot
                .text()
                .find(text)
                .expect("the word is in this buffer");
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
                .to_string()
                .replace('\\', "/");
            let range =
                location.range.start.to_offset(&snapshot)..location.range.end.to_offset(&snapshot);
            (path, snapshot.text_for_range(range).collect())
        })
    }

    /// The alias is a name the index cannot follow: `Other` is not declared
    /// anywhere in the project, so the index sends the reader nowhere, while
    /// `ty` follows the import through the alias to the class itself.
    #[gpui::test]
    async fn go_to_definition_follows_an_aliased_import_with_no_server(cx: &mut TestAppContext) {
        let (at, _index_at, project, over) =
            a_project_with_ty_and_the_index(&a_python_project(), cx).await;
        let main = opened(&project, &at.path().join("main.py"), cx).await;
        let position = at_the_word(&main, "Other()", cx);

        let answered = cx
            .update(|cx| over.definitions(&main, position, editor::GotoDefinitionKind::Symbol, cx))
            .expect("something takes the question")
            .await
            .expect("and answers it")
            .expect("with somewhere to go");

        let places: Vec<(String, String)> = answered
            .iter()
            .map(|link| what_it_points_at(&link.target, cx))
            .collect();
        assert_eq!(
            places,
            vec![("pkg/mod.py".to_string(), "Thing".to_string())],
            "the class the alias was imported from, not the alias"
        );
    }

    /// `label` is written in two unrelated classes. Every occurrence of the
    /// word is the most the index could offer; the occurrences that are the
    /// same symbol is what `ty` offers instead.
    #[gpui::test]
    async fn find_references_keeps_the_same_symbol_apart_from_the_same_word(
        cx: &mut TestAppContext,
    ) {
        let (at, _index_at, project, over) =
            a_project_with_ty_and_the_index(&a_python_project(), cx).await;
        let main = opened(&project, &at.path().join("main.py"), cx).await;
        let position = at_the_word(&main, "held.label", cx);
        // Past `held.` and onto the member itself.
        let position = main.read_with(cx, |buffer, _| {
            let snapshot = buffer.snapshot();
            let at = position.to_offset(&snapshot) + "held.".len();
            snapshot.anchor_before(at)
        });

        let answered = cx
            .update(|cx| over.references(&main, position, cx))
            .expect("something takes the question")
            .await
            .expect("and answers it")
            .expect("with places");

        let mut places: Vec<(String, String)> = answered
            .iter()
            .map(|location| what_it_points_at(location, cx))
            .collect();
        places.sort();
        assert!(
            places.contains(&("main.py".to_string(), "label".to_string())),
            "the use that was asked about is one of them: {places:?}"
        );
        assert!(
            places.contains(&("pkg/mod.py".to_string(), "label".to_string())),
            "so is the member it resolves to: {places:?}"
        );
        assert!(
            !places.iter().any(|(path, _)| path == "other.py"),
            "the unrelated class's member of the same name is not this symbol: {places:?}"
        );
    }

    /// Renaming the class has to reach the `import` lines that name it, in
    /// both the file that imports it plainly and the file that aliases it --
    /// and must leave the alias itself alone, because the alias is a different
    /// name for the same thing.
    #[gpui::test]
    async fn rename_reaches_the_import_lines_and_leaves_the_alias_alone(cx: &mut TestAppContext) {
        let (at, _index_at, project, over) =
            a_project_with_ty_and_the_index(&a_python_project(), cx).await;
        let declaring = opened(&project, &at.path().join("pkg/mod.py"), cx).await;
        // Held open across the rename, the way an editor holds the buffers it
        // shows. A buffer nothing holds is released as soon as the rename's
        // transaction is dropped, and reopening it reads the file again.
        let uses = opened(&project, &at.path().join("uses.py"), cx).await;
        let main = opened(&project, &at.path().join("main.py"), cx).await;
        let position = at_the_word(&declaring, "Thing", cx);

        let range = cx
            .update(|cx| over.range_for_rename(&declaring, position, cx))
            .await
            .expect("preparing the rename should not fail")
            .expect("the class can be renamed");
        let asked_about = declaring.read_with(cx, |buffer, _| {
            let snapshot = buffer.snapshot();
            snapshot
                .text_for_range(range.start.to_offset(&snapshot)..range.end.to_offset(&snapshot))
                .collect::<String>()
        });
        assert_eq!(
            asked_about, "Thing",
            "the range offered is the class's name"
        );

        cx.update(|cx| over.perform_rename(&declaring, position, "Widget".to_string(), cx))
            .expect("something takes the rename")
            .await
            .expect("and performs it");

        let text_of = |buffer: &Entity<Buffer>, cx: &mut TestAppContext| {
            buffer.read_with(cx, |buffer, _| buffer.text())
        };
        assert_eq!(
            text_of(&declaring, cx),
            "class Widget:\n    label: int = 1\n"
        );

        assert_eq!(
            text_of(&uses, cx),
            "from pkg.mod import Widget\n\n\ndef build() -> Widget:\n    return Widget()\n",
            "the import line and both uses"
        );

        assert_eq!(
            text_of(&main, cx),
            "from pkg.mod import Widget as Other\n\nheld = Other()\nname = held.label\n",
            "the aliased import is renamed and the alias itself is left alone"
        );
    }

    const DECLARES: &str = "pub fn take_stock() -> u32 {\n    1\n}\n";
    const CALLS: &str = "pub fn main() {\n    let held = take_stock();\n}\n";

    /// A file `ty` cannot place in a Python project must cost the reader
    /// nothing: the index answers exactly as it did before `ty` was asked
    /// anything, because an empty answer that replaces a good one is the worst
    /// outcome available here.
    #[gpui::test]
    async fn a_file_ty_cannot_place_still_gets_the_index_answer(cx: &mut TestAppContext) {
        let (at, _index_at, project, over) =
            a_project_with_ty_and_the_index(&[("one.rs", DECLARES), ("two.rs", CALLS)], cx).await;
        let calling = opened(&project, &at.path().join("two.rs"), cx).await;
        let position = at_the_word(&calling, "take_stock", cx);

        let answered = cx
            .update(|cx| {
                over.definitions(&calling, position, editor::GotoDefinitionKind::Symbol, cx)
            })
            .expect("the index takes the question")
            .await
            .expect("and answers it")
            .expect("with somewhere to go");
        let places: Vec<(String, String)> = answered
            .iter()
            .map(|link| what_it_points_at(&link.target, cx))
            .collect();
        assert_eq!(
            places,
            vec![("one.rs".to_string(), "take_stock".to_string())],
            "the declaring file, found by the index"
        );

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
            "the call is one of the places the index knows: {places:?}"
        );
    }

    /// A Python name `ty` resolves to nothing -- a member read off a module it
    /// could not import -- is the other half of the same rule: the index knows
    /// the name from the file that declares it, and still answers.
    #[gpui::test]
    async fn a_python_name_ty_resolves_to_nothing_still_gets_the_index_answer(
        cx: &mut TestAppContext,
    ) {
        let files = [
            (
                "pyproject.toml",
                "[project]\nname = \"asked\"\nversion = \"0\"\n",
            ),
            ("counting.py", "def take_stock() -> int:\n    return 1\n"),
            (
                "asking.py",
                "import a_module_that_is_not_installed\n\nheld = a_module_that_is_not_installed.take_stock\n",
            ),
        ];
        let (at, _index_at, project, over) = a_project_with_ty_and_the_index(&files, cx).await;
        let asking = opened(&project, &at.path().join("asking.py"), cx).await;
        let position = at_the_word(&asking, "installed.take_stock", cx);
        let position = asking.read_with(cx, |buffer, _| {
            let snapshot = buffer.snapshot();
            let at = position.to_offset(&snapshot) + "installed.".len();
            snapshot.anchor_before(at)
        });

        let answered = cx
            .update(|cx| {
                over.definitions(&asking, position, editor::GotoDefinitionKind::Symbol, cx)
            })
            .expect("something takes the question")
            .await
            .expect("and answers it")
            .expect("with somewhere to go");
        let places: Vec<(String, String)> = answered
            .iter()
            .map(|link| what_it_points_at(&link.target, cx))
            .collect();
        assert_eq!(
            places,
            vec![("counting.py".to_string(), "take_stock".to_string())],
            "the declaring file, found by the index where ty resolved nothing"
        );
    }
}
