#[cfg(test)]
mod tests {
    use fs::FakeFs;
    use gpui::{Entity, TestAppContext};
    use language::Buffer;
    use lsp::{CompletionContext, CompletionTriggerKind};
    use project::{CompletionIntent, Project};
    use settings::SettingsStore;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            crate::symbol_completions::init(cx);
        });
    }

    /// A project that exists twice over at one path, the way this crate's other
    /// tests build one: the editor reads the in-memory filesystem, and the
    /// index reads the real directory, because it walks the disk with the
    /// standard library rather than through the editor.
    async fn a_project(
        files: &[(&str, &str)],
        cx: &mut TestAppContext,
    ) -> (tempfile::TempDir, Entity<Project>) {
        let held = tempfile::tempdir().expect("a directory to put a project in");
        for (path, contents) in files {
            let at = held.path().join(path);
            if let Some(directory) = at.parent() {
                std::fs::create_dir_all(directory).expect("a directory for a project file");
            }
            std::fs::write(&at, contents).expect("a project file on disk");
        }
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree_from_real_fs(held.path(), held.path()).await;
        let project = Project::test(fs, [held.path()], cx).await;
        (held, project)
    }

    /// The project, its index built, and the buffer of the file being typed
    /// into -- with no language server anywhere.
    async fn writing_into(
        files: &[(&str, &str)],
        writing_in: &str,
        cx: &mut TestAppContext,
    ) -> (
        tempfile::TempDir,
        tempfile::TempDir,
        Entity<Project>,
        Entity<Buffer>,
    ) {
        init_test(cx);
        let (project_at, project) = a_project(files, cx).await;
        let index_at = tempfile::tempdir().expect("a directory for the index's own files");
        cx.update(|cx| {
            crate::ensure_index_at(project.clone(), index_at.path().join("symbol_index"), cx)
        });
        // The index builds in the background, and a test that reads it before
        // that finishes reads an empty one.
        cx.executor().run_until_parked();
        let buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer(project_at.path().join(writing_in), cx)
            })
            .await
            .expect("the file being typed into opens");
        (project_at, index_at, project, buffer)
    }

    /// Accepts the completion that offers `name`, at the end of the last
    /// occurrence of `typed` in the buffer.
    ///
    /// The two halves happen in the order the editor does them in: the name
    /// replaces what was typed, and only then is the completion's own
    /// confirmation run.
    async fn accepting(
        project: &Entity<Project>,
        buffer: &Entity<Buffer>,
        typed: &str,
        name: &str,
        cx: &mut TestAppContext,
    ) {
        let at = buffer.read_with(cx, |buffer, _| {
            let text = buffer.text();
            text.rfind(typed)
                .map(|found| found + typed.len())
                .expect("what was typed is in the buffer")
        });
        let responses = project
            .update(cx, |project, cx| {
                project.completions(
                    buffer,
                    at,
                    CompletionContext {
                        trigger_kind: CompletionTriggerKind::INVOKED,
                        trigger_character: None,
                    },
                    cx,
                )
            })
            .await
            .expect("the project answers with no server running");
        let completion = responses
            .iter()
            .flat_map(|response| &response.completions)
            .find(|completion| completion.new_text == name)
            .cloned()
            .unwrap_or_else(|| panic!("the index offers {name}"));

        buffer.update(cx, |buffer, cx| {
            buffer.edit(
                [(
                    completion.replace_range.clone(),
                    completion.new_text.as_str(),
                )],
                None,
                cx,
            );
        });
        let window = cx.add_window(|_, _| gpui::Empty);
        window
            .update(cx, |_, window, cx| {
                if let Some(confirm) = completion.confirm.as_ref() {
                    confirm(CompletionIntent::Complete, window, cx);
                }
            })
            .expect("the window takes the confirmation");
        cx.executor().run_until_parked();
    }

    fn text_of(buffer: &Entity<Buffer>, cx: &mut TestAppContext) -> String {
        buffer.read_with(cx, |buffer, _| buffer.text())
    }

    const MANIFEST: &str = "[package]\nname = \"holding\"\n\n[lib]\npath = \"src/holding.rs\"\n";
    const DECLARES: &str = "pub fn take_stock() -> u32 {\n    1\n}\n";

    /// This workspace names a crate's library root after the crate rather than
    /// calling it `lib.rs`, so every one of these is written that way: a
    /// mapping that only works for `lib.rs` is wrong about this very
    /// repository.
    fn a_crate_with_a_named_library_root(
        writing: &'static str,
    ) -> Vec<(&'static str, &'static str)> {
        vec![
            ("crates/holding/Cargo.toml", MANIFEST),
            (
                "crates/holding/src/holding.rs",
                "pub mod window;\npub mod writing;\n",
            ),
            ("crates/holding/src/window.rs", DECLARES),
            ("crates/holding/src/writing.rs", writing),
        ]
    }

    #[gpui::test]
    async fn a_name_from_another_module_arrives_with_its_use_line(cx: &mut TestAppContext) {
        let files =
            a_crate_with_a_named_library_root("pub fn writes() {\n    let held = take_st;\n}\n");
        let (_project_at, _index_at, project, buffer) =
            writing_into(&files, "crates/holding/src/writing.rs", cx).await;

        accepting(&project, &buffer, "take_st", "take_stock", cx).await;

        assert_eq!(
            text_of(&buffer, cx),
            "use crate::window::take_stock;\n\npub fn writes() {\n    let held = take_stock;\n}\n",
            "the name lands and the line that makes it resolve comes with it"
        );
    }

    #[gpui::test]
    async fn a_name_already_imported_is_not_imported_a_second_time(cx: &mut TestAppContext) {
        let files = a_crate_with_a_named_library_root(
            "use crate::window::take_stock;\n\npub fn writes() {\n    let held = take_st;\n}\n",
        );
        let (_project_at, _index_at, project, buffer) =
            writing_into(&files, "crates/holding/src/writing.rs", cx).await;

        accepting(&project, &buffer, "take_st", "take_stock", cx).await;

        assert_eq!(
            text_of(&buffer, cx),
            "use crate::window::take_stock;\n\npub fn writes() {\n    let held = take_stock;\n}\n",
            "the import the file already had is the only one"
        );
    }

    #[gpui::test]
    async fn a_name_declared_in_this_very_file_needs_no_import(cx: &mut TestAppContext) {
        let files = a_crate_with_a_named_library_root(
            "pub fn take_stock() -> u32 {\n    2\n}\n\npub fn writes() {\n    let held = take_st;\n}\n",
        );
        let (_project_at, _index_at, project, buffer) =
            writing_into(&files, "crates/holding/src/writing.rs", cx).await;

        accepting(&project, &buffer, "take_st", "take_stock", cx).await;

        assert_eq!(
            text_of(&buffer, cx),
            "pub fn take_stock() -> u32 {\n    2\n}\n\npub fn writes() {\n    let held = take_stock;\n}\n",
            "a name the file declares itself is already in scope"
        );
    }

    #[gpui::test]
    async fn a_language_outside_the_two_gets_the_bare_name_it_always_did(cx: &mut TestAppContext) {
        let files = [
            (
                "one.go",
                "package main\n\nfunc TakeStock() int {\n\treturn 1\n}\n",
            ),
            (
                "two.go",
                "package main\n\nfunc writes() {\n\theld := TakeSt\n}\n",
            ),
        ];
        let (_project_at, _index_at, project, buffer) = writing_into(&files, "two.go", cx).await;

        accepting(&project, &buffer, "TakeSt", "TakeStock", cx).await;

        assert_eq!(
            text_of(&buffer, cx),
            "package main\n\nfunc writes() {\n\theld := TakeStock\n}\n",
            "the name and nothing else, which is what it has always been"
        );
    }

    #[gpui::test]
    async fn a_python_name_arrives_with_the_import_its_package_needs(cx: &mut TestAppContext) {
        let files = [
            ("pkg/__init__.py", ""),
            ("pkg/shapes.py", "def compute_area():\n    return 1\n"),
            ("main.py", "def run():\n    return compute_ar\n"),
        ];
        let (_project_at, _index_at, project, buffer) = writing_into(&files, "main.py", cx).await;

        accepting(&project, &buffer, "compute_ar", "compute_area", cx).await;

        assert_eq!(
            text_of(&buffer, cx),
            "from pkg.shapes import compute_area\n\ndef run():\n    return compute_area\n",
            "the package the file lives in is what the import names"
        );
    }
}
