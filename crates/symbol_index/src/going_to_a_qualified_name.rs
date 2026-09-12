#[cfg(test)]
mod tests {
    use editor::SemanticsProvider as _;
    use fs::FakeFs;
    use gpui::AppContext as _;
    use gpui::{Entity, TestAppContext};
    use language::Buffer;
    use project::Project;
    use settings::SettingsStore;
    use text::ToOffset as _;

    use crate::SymbolIndex;
    use crate::index_semantics::IndexFirst;

    const ROOT_MODULE: &str = "module pd-trkd-ts1\n\ngo 1.24.1\n\nrequire (\n)\n";

    const CLIENT_MODULE: &str = "module github.com/example/thing/client\n\ngo 1.24.1\n";

    /// The shape the report came from: a grouped `type (...)` block, which is
    /// where the type the reader clicks through to actually lives.
    const INTERNAL_MODELS: &str = "package models\n\
        \n\
        type (\n\
        \tGetSymbolParams struct {\n\
        \t\tInterval string\n\
        \t}\n\
        \n\
        \tSymbol struct {\n\
        \t\tInstrumentID int64\n\
        \t\tIs24Hour     bool\n\
        \t}\n\
        )\n";

    /// The same name again, as a field of another struct in the same package.
    /// This is what makes the bare name ambiguous, and it is not invented for
    /// the test: the project the report came from has two of them.
    const INTERNAL_RESULT: &str = "package models\n\
        \n\
        type Result struct {\n\
        \tSymbol   string\n\
        \tExchange string\n\
        }\n";

    /// A name the project declares exactly once, which is what the bare-name
    /// reading answers with -- and what a qualified name of another package
    /// must never be answered with.
    const LOGGING: &str = "package logging\n\
        \n\
        type Logger struct {\n\
        \tLevel string\n\
        }\n";

    const CLIENT_MODELS: &str = "package models\n\
        \n\
        type Symbol struct {\n\
        \tRic string\n\
        }\n";

    const JOB: &str = "package process\n\
        \n\
        import (\n\
        \t\"context\"\n\
        \t\"pd-trkd-ts1/internal/models\"\n\
        \n\
        \tpkgModels \"github.com/example/thing/client/pkg/models\"\n\
        \t\"go.uber.org/zap\"\n\
        )\n\
        \n\
        type Job struct {\n\
        \tname string\n\
        }\n\
        \n\
        func (j *Job) processSymbol(ctx context.Context, symbolParams models.Symbol) {\n\
        \t_ = ctx\n\
        \t_ = symbolParams\n\
        }\n\
        \n\
        func (j *Job) report(out pkgModels.Symbol) {\n\
        \t_ = out\n\
        \t_ = j.name\n\
        }\n\
        \n\
        func (j *Job) latest(result models.Result) string {\n\
        \treturn result.Symbol\n\
        }\n\
        \n\
        func (j *Job) note(logger *zap.Logger) {\n\
        \t_ = logger\n\
        }\n\
        \n\
        func (j *Job) run(params models.Symbol) {\n\
        \tj.processSymbol(nil, params)\n\
        }\n";

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
    }

    /// A project that exists twice over at one path -- on the in-memory
    /// filesystem the editor reads, and on the real disk the index walks --
    /// with directories, which is what a Go package is.
    async fn a_go_project(
        files: &[(&str, &str)],
        cx: &mut TestAppContext,
    ) -> (tempfile::TempDir, Entity<Project>) {
        let held = tempfile::tempdir().expect("a directory to put a project in");
        let mut tree = serde_json::Value::Object(serde_json::Map::new());
        for (name, contents) in files {
            let at = held.path().join(name);
            if let Some(directory) = at.parent() {
                std::fs::create_dir_all(directory).expect("the directories of a project file");
            }
            std::fs::write(&at, contents).expect("a project file on disk");
            insert_into(&mut tree, name, contents);
        }
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(held.path(), tree).await;
        let project = Project::test(fs, [held.path()], cx).await;
        (held, project)
    }

    fn insert_into(tree: &mut serde_json::Value, path: &str, contents: &str) {
        let mut at = tree;
        let mut parts = path.split('/').peekable();
        while let Some(part) = parts.next() {
            let object = at
                .as_object_mut()
                .expect("every step above a file is a directory");
            if parts.peek().is_none() {
                object.insert(
                    part.to_string(),
                    serde_json::Value::String(contents.to_string()),
                );
                return;
            }
            at = object
                .entry(part.to_string())
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
        }
    }

    /// The project of the report, its index, the buffer of the file that does
    /// the naming, and the provider built over both.
    async fn a_project_of_two_packages_that_share_a_name(
        cx: &mut TestAppContext,
    ) -> (
        tempfile::TempDir,
        tempfile::TempDir,
        Entity<Project>,
        Entity<SymbolIndex>,
        Entity<Buffer>,
        IndexFirst,
    ) {
        init_test(cx);
        let (project_at, project) = a_go_project(
            &[
                ("go.mod", ROOT_MODULE),
                ("internal/models/instrument.go", INTERNAL_MODELS),
                ("internal/models/result.go", INTERNAL_RESULT),
                ("client/go.mod", CLIENT_MODULE),
                ("client/pkg/models/model.go", CLIENT_MODELS),
                ("internal/logging/logger.go", LOGGING),
                ("internal/process/job.go", JOB),
            ],
            cx,
        )
        .await;

        let index_at = tempfile::tempdir().expect("a directory for the index's own files");
        let index = cx.update(|cx| {
            crate::ensure_index_at(project.clone(), index_at.path().join("symbol_index"), cx)
        });
        cx.run_until_parked();

        let job = project
            .update(cx, |project, cx| {
                project.open_local_buffer(project_at.path().join("internal/process/job.go"), cx)
            })
            .await
            .expect("the file that names both packages opens");
        let over = IndexFirst::over(&project, &index);
        (project_at, index_at, project, index, job, over)
    }

    /// The cursor inside the name written after `qualifier.`, which is where a
    /// reader's pointer is when they hold the modifier down and click.
    fn at_the_qualified_name(
        buffer: &Entity<Buffer>,
        written: &str,
        cx: &mut TestAppContext,
    ) -> text::Anchor {
        buffer.read_with(cx, |buffer, _| {
            let snapshot = buffer.snapshot();
            let text = snapshot.text();
            let at = text.find(written).expect("the selector is in this buffer");
            let after_the_dot = at
                + written
                    .find('.')
                    .expect("a qualified name is written with a dot")
                + 1;
            snapshot.anchor_before(after_the_dot + 1)
        })
    }

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

    async fn where_a_click_lands(
        over: &IndexFirst,
        buffer: &Entity<Buffer>,
        written: &str,
        cx: &mut TestAppContext,
    ) -> Option<(String, String)> {
        let position = at_the_qualified_name(buffer, written, cx);
        let answered = cx.update(|cx| {
            over.definitions(buffer, position, editor::GotoDefinitionKind::Symbol, cx)
        })?;
        let found = answered.await.ok()??;
        let first = found.first()?;
        Some(what_it_points_at(&first.target, cx))
    }

    /// What counts as a qualifier, read straight off a buffer, because every
    /// answer below rests on getting this right and the cases that go wrong
    /// are the ones nobody writes a project around.
    #[gpui::test]
    async fn what_is_read_as_a_qualifier(cx: &mut TestAppContext) {
        init_test(cx);
        let text = "models.Symbol foo().Bar a.b.C plain .Leading";
        let buffer = cx.update(|cx| cx.new(|cx| language::Buffer::local(text, cx)));
        let snapshot = buffer.read_with(cx, |buffer, _| buffer.snapshot());

        let asked_inside = |written: &str| {
            let at = text.find(written).expect("the word is in this buffer");
            crate::index_semantics::qualifier_before(&snapshot, at)
        };

        assert_eq!(asked_inside("Symbol"), Some("models".to_string()));
        assert_eq!(
            asked_inside("C"),
            Some("b".to_string()),
            "the nearest qualifier is the one written before the name"
        );
        assert_eq!(
            asked_inside("Bar"),
            None,
            "a call's result is a value, and there is no name before the dot to read"
        );
        assert_eq!(asked_inside("plain"), None);
        assert_eq!(
            asked_inside("Leading"),
            None,
            "a dot with nothing before it qualifies nothing"
        );
    }

    /// The report itself: the reader holds the modifier down over `Symbol` in
    /// `models.Symbol` and nothing happens, because three things in this
    /// project are called `Symbol` and the bare name cannot say which.
    ///
    /// This is the state before the fix, asserted directly rather than
    /// inferred, so that the fix cannot be said to have solved a problem that
    /// was not there.
    #[gpui::test]
    async fn the_bare_name_is_ambiguous_and_stays_declined(cx: &mut TestAppContext) {
        let (_project_at, _index_at, _project, index, _job, _over) =
            a_project_of_two_packages_that_share_a_name(cx).await;

        index.read_with(cx, |index, _| {
            assert!(
                !matches!(
                    index.what_a_name_means("Symbol"),
                    Some(semantic_index::resolution::WhatItMeans::TheseAre(_))
                ),
                "three declarations share this name, so the name alone means nothing"
            );
            assert_eq!(
                index.where_declared("Symbol"),
                None,
                "and there is no one place to send a reader to"
            );
        });
    }

    #[gpui::test]
    async fn a_grouped_type_block_is_indexed(cx: &mut TestAppContext) {
        let (_project_at, _index_at, _project, index, _job, _over) =
            a_project_of_two_packages_that_share_a_name(cx).await;

        index.read_with(cx, |index, _| {
            let declared: Vec<(String, u32)> = index
                .candidates("Symbol", 32)
                .into_iter()
                .filter(|found| found.name == "Symbol")
                .map(|found| (found.path.clone(), found.line))
                .collect();
            assert!(
                declared.contains(&("internal/models/instrument.go".to_string(), 8)),
                "the type inside `type ( ... )` is a declaration like any other: {declared:?}"
            );
        });
    }

    #[gpui::test]
    async fn the_import_says_which_package_a_qualified_name_is_of(cx: &mut TestAppContext) {
        let (_project_at, _index_at, _project, _index, job, over) =
            a_project_of_two_packages_that_share_a_name(cx).await;

        let landed = where_a_click_lands(&over, &job, "models.Symbol", cx).await;
        assert_eq!(
            landed,
            Some((
                "internal/models/instrument.go".to_string(),
                "Symbol".to_string()
            )),
            "`models` is the plain import, so this is the internal package's type"
        );
    }

    #[gpui::test]
    async fn an_alias_names_the_other_package_of_the_same_name(cx: &mut TestAppContext) {
        let (_project_at, _index_at, _project, _index, job, over) =
            a_project_of_two_packages_that_share_a_name(cx).await;

        let landed = where_a_click_lands(&over, &job, "pkgModels.Symbol", cx).await;
        assert_eq!(
            landed,
            Some((
                "client/pkg/models/model.go".to_string(),
                "Symbol".to_string()
            )),
            "the alias is bound to the client module, which is a module of its own in this tree"
        );
    }

    /// A qualifier the file does not import is a value, and what type a value
    /// has is the one thing nothing here knows. Answering anyway would send a
    /// reader to a field of some unrelated struct.
    #[gpui::test]
    async fn a_qualifier_that_is_not_an_import_is_declined(cx: &mut TestAppContext) {
        let (_project_at, _index_at, _project, _index, job, over) =
            a_project_of_two_packages_that_share_a_name(cx).await;

        let landed = where_a_click_lands(&over, &job, "result.Symbol", cx).await;
        assert_eq!(
            landed, None,
            "`result` is a value, so its `Symbol` is a field of whatever type it has -- \
             and both packages' `Symbol` would be the wrong place to land"
        );
    }

    /// A package outside the project owns the name written after it. The
    /// project declares exactly one `Logger` of its own, which is precisely
    /// what the bare-name reading would have answered with -- and it is not
    /// the `Logger` the reader asked about.
    #[gpui::test]
    async fn a_package_outside_the_project_is_not_answered_from_inside_it(cx: &mut TestAppContext) {
        let (_project_at, _index_at, _project, index, job, over) =
            a_project_of_two_packages_that_share_a_name(cx).await;

        index.read_with(cx, |index, _| {
            assert!(
                index.where_declared("Logger").is_some(),
                "the project declares exactly one Logger, so the bare name would answer"
            );
        });

        let landed = where_a_click_lands(&over, &job, "zap.Logger", cx).await;
        assert_eq!(
            landed, None,
            "`zap` is a dependency, so its Logger is not this project's Logger"
        );
    }

    /// The other half of the same decision: a qualifier that names no import
    /// is a value, and what the index already answered about its members is
    /// left exactly as it was.
    #[gpui::test]
    async fn a_member_reached_through_a_value_is_still_answered(cx: &mut TestAppContext) {
        let (_project_at, _index_at, _project, _index, job, over) =
            a_project_of_two_packages_that_share_a_name(cx).await;

        let landed = where_a_click_lands(&over, &job, "j.processSymbol", cx).await;
        assert_eq!(
            landed,
            Some((
                "internal/process/job.go".to_string(),
                "processSymbol".to_string()
            )),
            "the project declares this method once, and `j` names no package"
        );
    }

    /// The underline a reader sees before they click has to agree with where
    /// the click would take them: a name that is not navigable must not look
    /// navigable.
    #[gpui::test]
    async fn the_name_is_underlined_before_it_is_clicked(cx: &mut TestAppContext) {
        let (_project_at, _index_at, _project, _index, job, over) =
            a_project_of_two_packages_that_share_a_name(cx).await;

        let position = at_the_qualified_name(&job, "models.Symbol", cx);
        let underlined = cx.update(|cx| over.link_candidate_range(&job, position, cx));
        let shown = job.read_with(cx, |buffer, _| {
            let snapshot = buffer.snapshot();
            underlined.map(|range| {
                let range = range.start.to_offset(&snapshot)..range.end.to_offset(&snapshot);
                snapshot.text_for_range(range).collect::<String>()
            })
        });
        assert_eq!(shown, Some("Symbol".to_string()));
    }

    /// `models.Symbol` is a type, so go-to-type-definition lands on it too --
    /// which it did not before, because the kind the index records for a Go
    /// type is the one the outline query captures.
    #[gpui::test]
    async fn a_qualified_type_answers_go_to_type_definition(cx: &mut TestAppContext) {
        let (_project_at, _index_at, _project, _index, job, over) =
            a_project_of_two_packages_that_share_a_name(cx).await;

        let position = at_the_qualified_name(&job, "models.Symbol", cx);
        let answered = cx
            .update(|cx| over.definitions(&job, position, editor::GotoDefinitionKind::Type, cx))
            .expect("the index takes the question")
            .await
            .expect("and answers it")
            .expect("with a place to go");
        assert_eq!(
            what_it_points_at(&answered[0].target, cx).0,
            "internal/models/instrument.go"
        );
    }

    /// The imports are read from the buffer, not from the store, so an import
    /// the reader has just typed and not yet saved is the one that counts.
    #[gpui::test]
    async fn an_import_that_has_not_been_saved_yet_still_counts(cx: &mut TestAppContext) {
        let (_project_at, _index_at, _project, _index, job, over) =
            a_project_of_two_packages_that_share_a_name(cx).await;

        job.update(cx, |buffer, cx| {
            let text = buffer.snapshot().text();
            let at = text
                .find("\tpkgModels \"github.com/example/thing/client/pkg/models\"\n")
                .expect("the alias line is in the file");
            buffer.edit(
                [(at..at + "\tpkgModels".len(), "\tfromTheClient")],
                None,
                cx,
            );
            let text = buffer.snapshot().text();
            let used = text.find("pkgModels.Symbol").expect("the use of the alias");
            buffer.edit(
                [(used..used + "pkgModels".len(), "fromTheClient")],
                None,
                cx,
            );
        });

        let landed = where_a_click_lands(&over, &job, "fromTheClient.Symbol", cx).await;
        assert_eq!(
            landed,
            Some((
                "client/pkg/models/model.go".to_string(),
                "Symbol".to_string()
            )),
            "the store still holds the old alias; the buffer is what the reader sees"
        );
    }
}
