use editor::{Bias, Editor, SelectionEffects, scroll::Autoscroll, styled_runs_for_code_label};
use fuzzy::{StringMatch, StringMatchCandidate};
use gpui::{
    AnyElement, App, Context, DismissEvent, Entity, HighlightStyle, ParentElement, StyledText,
    Task, TaskExt, TextStyle, WeakEntity, Window, relative,
};
use ordered_float::OrderedFloat;
use picker::{Picker, PickerDelegate, PreviewUpdate};
use project::{Project, Symbol, lsp_store::SymbolLocation};
use settings::Settings;
use std::{cmp::Reverse, sync::Arc};
use theme::ActiveTheme;
use theme_settings::ThemeSettings;
use util::ResultExt;
use workspace::{
    Workspace,
    ui::{LabelLike, ListItem, ListItemSpacing, prelude::*},
};

pub fn init(cx: &mut App) {
    cx.observe_new(
        |workspace: &mut Workspace, _window, _: &mut Context<Workspace>| {
            workspace.register_action(
                |workspace, _: &workspace::ToggleProjectSymbols, window, cx| {
                    let project = workspace.project().clone();
                    let handle = cx.entity().downgrade();
                    workspace.toggle_modal(window, cx, move |window, cx| {
                        let delegate = ProjectSymbolsDelegate::new(handle, project.clone());
                        let preview = picker_preview::editor_preview(project, window, cx);
                        Picker::uniform_list_with_preview(delegate, preview, window, cx)
                    })
                },
            );
        },
    )
    .detach();
}

pub type ProjectSymbols = Entity<Picker<ProjectSymbolsDelegate>>;

pub struct ProjectSymbolsDelegate {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    selected_match_index: usize,
    symbols: Vec<Symbol>,
    visible_match_candidates: Vec<StringMatchCandidate>,
    external_match_candidates: Vec<StringMatchCandidate>,
    show_worktree_root_name: bool,
    matches: Vec<StringMatch>,
    /// True when the source that answered had more matches than it was
    /// willing to list, so the list on screen is not all there is.
    cut_short: bool,
}

impl ProjectSymbolsDelegate {
    fn new(workspace: WeakEntity<Workspace>, project: Entity<Project>) -> Self {
        Self {
            workspace,
            project,
            selected_match_index: 0,
            symbols: Default::default(),
            visible_match_candidates: Default::default(),
            external_match_candidates: Default::default(),
            matches: Default::default(),
            show_worktree_root_name: false,
            cut_short: false,
        }
    }

    // Note if you make changes to this, also change `agent_ui::completion_provider::search_symbols`
    fn filter(&mut self, query: &str, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        const MAX_MATCHES: usize = 100;
        let mut visible_matches = cx.foreground_executor().block_on(fuzzy::match_strings(
            &self.visible_match_candidates,
            query,
            false,
            true,
            MAX_MATCHES,
            &Default::default(),
            cx.background_executor().clone(),
        ));
        let mut external_matches = cx.foreground_executor().block_on(fuzzy::match_strings(
            &self.external_match_candidates,
            query,
            false,
            true,
            MAX_MATCHES - visible_matches.len().min(MAX_MATCHES),
            &Default::default(),
            cx.background_executor().clone(),
        ));
        let sort_key_for_match = |mat: &StringMatch| {
            let symbol = &self.symbols[mat.candidate_id];
            (Reverse(OrderedFloat(mat.score)), symbol.label.filter_text())
        };

        visible_matches.sort_unstable_by_key(sort_key_for_match);
        external_matches.sort_unstable_by_key(sort_key_for_match);
        let mut matches = visible_matches;
        matches.append(&mut external_matches);

        for mat in &mut matches {
            let symbol = &self.symbols[mat.candidate_id];
            let filter_start = symbol.label.filter_range.start;
            for position in &mut mat.positions {
                *position += filter_start;
            }
        }

        self.matches = matches;
        self.set_selected_index(0, window, cx);
    }
}

impl PickerDelegate for ProjectSymbolsDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "project symbols"
    }
    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Search project symbols...".into()
    }

    fn confirm(&mut self, secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Some(symbol) = self
            .matches
            .get(self.selected_match_index)
            .map(|mat| self.symbols[mat.candidate_id].clone())
        {
            let buffer = self.project.update(cx, |project, cx| {
                project.open_buffer_for_symbol(&symbol, cx)
            });
            let symbol = symbol.clone();
            let workspace = self.workspace.clone();
            cx.spawn_in(window, async move |_, cx| {
                let buffer = buffer.await?;
                workspace.update_in(cx, |workspace, window, cx| {
                    let position = buffer
                        .read(cx)
                        .clip_point_utf16(symbol.range.start, Bias::Left);
                    let pane = if secondary {
                        workspace.adjacent_pane(window, cx)
                    } else {
                        workspace.active_pane().clone()
                    };

                    let editor = workspace.open_project_item::<Editor>(
                        pane, buffer, true, true, true, true, window, cx,
                    );

                    editor.update(cx, |editor, cx| {
                        let multibuffer_snapshot = editor.buffer().read(cx).snapshot(cx);
                        let Some(buffer_snapshot) = multibuffer_snapshot.as_singleton() else {
                            return;
                        };
                        let text_anchor = buffer_snapshot.anchor_before(position);
                        let Some(anchor) = multibuffer_snapshot.anchor_in_buffer(text_anchor)
                        else {
                            return;
                        };
                        editor.change_selections(
                            SelectionEffects::scroll(Autoscroll::center()),
                            window,
                            cx,
                            |s| s.select_ranges([anchor..anchor]),
                        );
                    });
                })?;
                anyhow::Ok(())
            })
            .detach_and_log_err(cx);
            cx.emit(DismissEvent);
        }
    }

    fn dismissed(&mut self, _window: &mut Window, _cx: &mut Context<Picker<Self>>) {}

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_match_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_match_index = ix;
    }

    fn try_get_preview_data_for_match(&self, _cx: &App) -> Option<PreviewUpdate> {
        let candidate_id = self.matches.get(self.selected_match_index)?.candidate_id;
        let symbol = self.symbols.get(candidate_id)?.clone();
        Some(PreviewUpdate::from_symbol(symbol))
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        // Try to support rust-analyzer's path based symbols feature which
        // allows to search by rust path syntax, in that case we only want to
        // filter names by the last segment
        // Ideally this was a first class LSP feature (rich queries)
        let query_filter = query
            .rsplit_once("::")
            .map_or(&*query, |(_, suffix)| suffix)
            .to_owned();
        self.filter(&query_filter, window, cx);
        self.show_worktree_root_name = self.project.read(cx).visible_worktrees(cx).count() > 1;
        let listing = self
            .project
            .update(cx, |project, cx| project.symbol_listing(&query, cx));
        cx.spawn_in(window, async move |this, cx| {
            let listing = listing.await.log_err();
            if let Some(listing) = listing {
                let (symbols, cut_short) = (listing.symbols, listing.cut_short);
                this.update_in(cx, |this, window, cx| {
                    let delegate = &mut this.delegate;
                    delegate.cut_short = cut_short;
                    let project = delegate.project.read(cx);
                    let (visible_match_candidates, external_match_candidates) = symbols
                        .iter()
                        .enumerate()
                        .map(|(id, symbol)| {
                            StringMatchCandidate::new(id, symbol.label.filter_text())
                        })
                        .partition(|candidate| {
                            if let SymbolLocation::InProject(path) = &symbols[candidate.id].path {
                                project
                                    .entry_for_path(path, cx)
                                    .is_some_and(|e| !e.is_ignored)
                            } else {
                                false
                            }
                        });

                    delegate.visible_match_candidates = visible_match_candidates;
                    delegate.external_match_candidates = external_match_candidates;
                    delegate.symbols = symbols;
                    delegate.filter(&query_filter, window, cx);
                })
                .log_err();
            }
        })
    }

    /// Says so when the list is not all there is. A reader who cannot find a
    /// name in a list that was cut short would otherwise conclude the project
    /// does not declare it.
    fn render_header(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<AnyElement> {
        if !self.cut_short {
            return None;
        }
        Some(
            h_flex()
                .w_full()
                .min_w_0()
                .flex_none()
                .px_2()
                .py_1()
                .child(
                    Label::new("Too many matches to list -- narrow the search.")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element(),
        )
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let path_style = self.project.read(cx).path_style(cx);
        let string_match = &self.matches.get(ix)?;
        let symbol = &self.symbols.get(string_match.candidate_id)?;
        let theme = cx.theme();
        let local_player = theme.players().local();
        let syntax_runs = styled_runs_for_code_label(&symbol.label, theme.syntax(), &local_player);

        let path = match &symbol.path {
            SymbolLocation::InProject(project_path) => {
                let project = self.project.read(cx);
                let mut path = project_path.path.to_rel_path_buf();
                if self.show_worktree_root_name
                    && let Some(worktree) = project.worktree_for_id(project_path.worktree_id, cx)
                {
                    path = worktree.read(cx).root_name().join(&path);
                }
                path.display(path_style).into_owned().into()
            }
            SymbolLocation::OutsideProject {
                abs_path,
                signature: _,
            } => abs_path.to_string_lossy(),
        };
        let label = symbol.label.text.clone();
        let line_number = symbol.range.start.0.row + 1;
        let path = path.into_owned();

        let settings = ThemeSettings::get_global(cx);

        let text_style = TextStyle {
            color: cx.theme().colors().text,
            font_family: settings.buffer_font.family.clone(),
            font_features: settings.buffer_font.features.clone(),
            font_fallbacks: settings.buffer_font.fallbacks.clone(),
            font_size: settings.buffer_font_size(cx).into(),
            font_weight: settings.buffer_font.weight,
            line_height: relative(1.),
            ..Default::default()
        };

        let highlight_style = HighlightStyle {
            background_color: Some(cx.theme().colors().text_accent.alpha(0.3)),
            ..Default::default()
        };
        let custom_highlights = string_match
            .positions
            .iter()
            .map(|pos| (*pos..label.ceil_char_boundary(pos + 1), highlight_style));

        let highlights = gpui::combine_highlights(custom_highlights, syntax_runs);

        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    v_flex()
                        .child(
                            LabelLike::new().child(
                                StyledText::new(&label)
                                    .with_default_highlights(&text_style, highlights),
                            ),
                        )
                        .child(
                            h_flex()
                                .child(Label::new(path).size(LabelSize::Small).color(Color::Muted))
                                .child(
                                    Label::new(format!(":{}", line_number))
                                        .size(LabelSize::Small)
                                        .color(Color::Placeholder),
                                ),
                        ),
                ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use gpui::{TestAppContext, VisualContext, VisualTestContext};
    use language::{FakeLspAdapter, Language, LanguageConfig, LanguageMatcher};
    use lsp::OneOf;
    use project::FakeFs;
    use serde_json::json;
    use settings::SettingsStore;
    use std::{path::Path, sync::Arc};
    use util::path;
    use workspace::MultiWorkspace;

    #[gpui::test]
    async fn test_project_symbols(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/dir"), json!({ "test.rs": "" }))
            .await;

        let project = Project::test(fs.clone(), [path!("/dir").as_ref()], cx).await;

        let language_registry = project.read_with(cx, |project, _| project.languages().clone());
        language_registry.add(Arc::new(Language::new(
            LanguageConfig {
                name: "Rust".into(),
                matcher: LanguageMatcher {
                    path_suffixes: vec!["rs".to_string()],
                    ..Default::default()
                },
                ..Default::default()
            },
            None,
        )));
        let mut fake_servers = language_registry.register_fake_lsp(
            "Rust",
            FakeLspAdapter {
                capabilities: lsp::ServerCapabilities {
                    workspace_symbol_provider: Some(OneOf::Left(true)),
                    ..Default::default()
                },
                ..Default::default()
            },
        );

        let _buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer_with_lsp(path!("/dir/test.rs"), cx)
            })
            .await
            .unwrap();

        // Set up fake language server to return fuzzy matches against
        // a fixed set of symbol names.
        let fake_symbols = [
            symbol("one", path!("/external")),
            symbol("ton", path!("/dir/test.rs")),
            symbol("uno", path!("/dir/test.rs")),
        ];
        let fake_server = fake_servers.next().await.unwrap();
        fake_server.set_request_handler::<lsp::WorkspaceSymbolRequest, _, _>(
            move |params: lsp::WorkspaceSymbolParams, cx| {
                let executor = cx.background_executor().clone();
                let fake_symbols = fake_symbols.clone();
                async move {
                    let (query, prefixed) = match params.query.strip_prefix("dir::") {
                        Some(query) => (query, true),
                        None => (&*params.query, false),
                    };
                    let candidates = fake_symbols
                        .iter()
                        .enumerate()
                        .filter(|(_, symbol)| {
                            !prefixed || symbol.location.uri.path().contains("dir")
                        })
                        .map(|(id, symbol)| StringMatchCandidate::new(id, &symbol.name))
                        .collect::<Vec<_>>();
                    let matches = if query.is_empty() {
                        Vec::new()
                    } else {
                        fuzzy::match_strings(
                            &candidates,
                            &query,
                            true,
                            true,
                            100,
                            &Default::default(),
                            executor.clone(),
                        )
                        .await
                    };

                    Ok(Some(lsp::WorkspaceSymbolResponse::Flat(
                        matches
                            .into_iter()
                            .map(|mat| fake_symbols[mat.candidate_id].clone())
                            .collect(),
                    )))
                }
            },
        );

        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        // Create the project symbols view.
        let symbols = cx.new_window_entity(|window, cx| {
            Picker::uniform_list(
                ProjectSymbolsDelegate::new(workspace.downgrade(), project.clone()),
                window,
                cx,
            )
        });

        // Spawn multiples updates before the first update completes,
        // such that in the end, there are no matches. Testing for regression:
        // https://github.com/zed-industries/zed/issues/861
        symbols.update_in(cx, |p, window, cx| {
            p.update_matches("o".to_string(), window, cx);
            p.update_matches("on".to_string(), window, cx);
            p.update_matches("onex".to_string(), window, cx);
        });

        cx.run_until_parked();
        symbols.read_with(cx, |symbols, _| {
            assert_eq!(symbols.delegate.matches.len(), 0);
        });

        // Spawn more updates such that in the end, there are matches.
        symbols.update_in(cx, |p, window, cx| {
            p.update_matches("one".to_string(), window, cx);
            p.update_matches("on".to_string(), window, cx);
        });

        cx.run_until_parked();
        symbols.read_with(cx, |symbols, _| {
            let delegate = &symbols.delegate;
            assert_eq!(delegate.matches.len(), 2);
            assert_eq!(delegate.matches[0].string, "ton");
            assert_eq!(delegate.matches[1].string, "one");
        });

        // Spawn more updates such that in the end, there are again no matches.
        symbols.update_in(cx, |p, window, cx| {
            p.update_matches("o".to_string(), window, cx);
            p.update_matches("".to_string(), window, cx);
        });

        cx.run_until_parked();
        symbols.read_with(cx, |symbols, _| {
            assert_eq!(symbols.delegate.matches.len(), 0);
        });

        // Check that rust-analyzer path style symbols work
        symbols.update_in(cx, |p, window, cx| {
            p.update_matches("dir::to".to_string(), window, cx);
        });

        cx.run_until_parked();
        symbols.read_with(cx, |symbols, _| {
            assert_eq!(symbols.delegate.matches.len(), 1);
        });
    }

    #[gpui::test]
    async fn test_project_symbols_renders_utf8_match(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/dir"), json!({ "test.rs": "" }))
            .await;

        let project = Project::test(fs.clone(), [path!("/dir").as_ref()], cx).await;

        let language_registry = project.read_with(cx, |project, _| project.languages().clone());
        language_registry.add(Arc::new(Language::new(
            LanguageConfig {
                name: "Rust".into(),
                matcher: LanguageMatcher {
                    path_suffixes: vec!["rs".to_string()],
                    ..Default::default()
                },
                ..Default::default()
            },
            None,
        )));
        let mut fake_servers = language_registry.register_fake_lsp(
            "Rust",
            FakeLspAdapter {
                capabilities: lsp::ServerCapabilities {
                    workspace_symbol_provider: Some(OneOf::Left(true)),
                    ..Default::default()
                },
                ..Default::default()
            },
        );

        let _buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer_with_lsp(path!("/dir/test.rs"), cx)
            })
            .await
            .unwrap();

        let fake_symbols = [symbol("안녕", path!("/dir/test.rs"))];
        let fake_server = fake_servers.next().await.unwrap();
        fake_server.set_request_handler::<lsp::WorkspaceSymbolRequest, _, _>(
            move |params: lsp::WorkspaceSymbolParams, cx| {
                let executor = cx.background_executor().clone();
                let fake_symbols = fake_symbols.clone();
                async move {
                    let candidates = fake_symbols
                        .iter()
                        .enumerate()
                        .map(|(id, symbol)| StringMatchCandidate::new(id, &symbol.name))
                        .collect::<Vec<_>>();
                    let matches = fuzzy::match_strings(
                        &candidates,
                        &params.query,
                        true,
                        true,
                        100,
                        &Default::default(),
                        executor,
                    )
                    .await;

                    Ok(Some(lsp::WorkspaceSymbolResponse::Flat(
                        matches
                            .into_iter()
                            .map(|mat| fake_symbols[mat.candidate_id].clone())
                            .collect(),
                    )))
                }
            },
        );

        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        let symbols = cx.new_window_entity(|window, cx| {
            Picker::uniform_list(
                ProjectSymbolsDelegate::new(workspace.downgrade(), project.clone()),
                window,
                cx,
            )
        });

        symbols.update_in(cx, |p, window, cx| {
            p.update_matches("안".to_string(), window, cx);
        });

        cx.run_until_parked();
        symbols.read_with(cx, |symbols, _| {
            assert_eq!(symbols.delegate.matches.len(), 1);
            assert_eq!(symbols.delegate.matches[0].string, "안녕");
        });

        symbols.update_in(cx, |p, window, cx| {
            assert!(p.delegate.render_match(0, false, window, cx).is_some());
        });
    }

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            editor::init(cx);
            crate::init(cx);
            symbol_index::symbols_of_the_project::init(cx);
            // Deliberately not `symbol_index::init`: its own `cx.observe_new`
            // would register a project's index at the real, shared
            // `paths::database_dir()` the moment a workspace opens. A test
            // that needs an index calls `symbol_index::ensure_index_at`
            // directly with a scratch directory instead.
        });
    }

    /// A project that exists twice over at one and the same path: the editor's
    /// side is the deterministic in-memory filesystem, and the index's side is
    /// a real directory, because the symbol index walks and reads the disk
    /// itself rather than going through the editor's filesystem abstraction.
    async fn a_project_on_disk(
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

    /// A project's index, kept at a scratch directory rather than the real
    /// `paths::database_dir()`, registered so that `symbol_index::of_project`
    /// finds it the same way it would find one `symbol_index::init` built.
    fn an_index_for(
        project: Entity<Project>,
        cx: &mut TestAppContext,
    ) -> (tempfile::TempDir, Entity<symbol_index::SymbolIndex>) {
        let held = tempfile::tempdir().expect("a directory for the index's own files");
        let index_dir = held.path().join("symbol_index");
        let index = cx.update(|cx| symbol_index::ensure_index_at(project, index_dir, cx));
        (held, index)
    }

    /// Opens a real workspace over `project` and opens the picker through the
    /// real action `cmd-t` is bound to, rather than constructing the delegate.
    fn open_the_picker(
        project: Entity<Project>,
        cx: &mut TestAppContext,
    ) -> (ProjectSymbols, Entity<Workspace>, &mut VisualTestContext) {
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        cx.dispatch_action(workspace::ToggleProjectSymbols);
        let picker = workspace.update(cx, |workspace, cx| {
            workspace
                .active_modal::<Picker<ProjectSymbolsDelegate>>(cx)
                .expect("the picker did not open")
        });
        (picker, workspace, cx)
    }

    /// The bug this guards against: `Project::symbols` went to the language
    /// servers and nowhere else, so with none running the most-used
    /// navigation shortcut in the editor listed nothing at all, whatever the
    /// project's own index held.
    #[gpui::test]
    async fn the_index_answers_when_no_language_server_does(cx: &mut TestAppContext) {
        init_test(cx);
        let (_on_disk, project) = a_project_on_disk(
            &[(
                "stock.rs",
                "pub fn open_up() {}\n\npub fn take_stock() {}\n",
            )],
            cx,
        )
        .await;
        let (_index_at, _index) = an_index_for(project.clone(), cx);
        // The index parses in the background; reading it before that has
        // finished reads an empty one.
        cx.executor().run_until_parked();

        let (picker, workspace, cx) = open_the_picker(project, cx);
        cx.simulate_input("take_stock");
        cx.run_until_parked();

        picker.read_with(cx, |picker, _| {
            let names: Vec<&str> = picker
                .delegate
                .matches
                .iter()
                .map(|matched| matched.string.as_str())
                .collect();
            assert_eq!(
                names,
                vec!["take_stock"],
                "with no language server the index must answer: {names:?}"
            );
        });

        cx.dispatch_action(menu::Confirm);
        cx.run_until_parked();

        let editor = workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_item_as::<Editor>(cx)
                .expect("confirming a symbol should have opened an editor")
        });
        editor.update_in(cx, |editor, _window, cx| {
            assert_eq!(editor.title(cx), "stock.rs");
            let snapshot = editor.display_snapshot(cx);
            let head = editor
                .selections
                .newest::<language::Point>(&snapshot)
                .head();
            assert_eq!(head.row, 2, "`take_stock` is declared on the third line");
        });
    }

    /// The precedence this guards: where a language server answered, its
    /// answer is what the reader sees, whole and unjoined by the index, so
    /// that no symbol is listed twice.
    #[gpui::test]
    async fn a_language_server_that_answers_is_not_joined_by_the_index(cx: &mut TestAppContext) {
        init_test(cx);
        let (on_disk, project) = a_project_on_disk(
            &[("stock.rs", "pub fn take_stock_from_the_index() {}\n")],
            cx,
        )
        .await;
        let (_index_at, _index) = an_index_for(project.clone(), cx);
        cx.executor().run_until_parked();

        let language_registry = project.read_with(cx, |project, _| project.languages().clone());
        language_registry.add(Arc::new(Language::new(
            LanguageConfig {
                name: "Rust".into(),
                matcher: LanguageMatcher {
                    path_suffixes: vec!["rs".to_string()],
                    ..Default::default()
                },
                ..Default::default()
            },
            None,
        )));
        let mut fake_servers = language_registry.register_fake_lsp(
            "Rust",
            FakeLspAdapter {
                capabilities: lsp::ServerCapabilities {
                    workspace_symbol_provider: Some(OneOf::Left(true)),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let stock = on_disk.path().join("stock.rs");
        let _buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer_with_lsp(&stock, cx)
            })
            .await
            .expect("the project file should open");

        let fake_symbols = [symbol("take_stock_from_the_server", &stock)];
        let fake_server = fake_servers.next().await.expect("a fake server to start");
        fake_server.set_request_handler::<lsp::WorkspaceSymbolRequest, _, _>(
            move |params: lsp::WorkspaceSymbolParams, cx| {
                let executor = cx.background_executor().clone();
                let fake_symbols = fake_symbols.clone();
                async move {
                    let candidates = fake_symbols
                        .iter()
                        .enumerate()
                        .map(|(id, symbol)| StringMatchCandidate::new(id, &symbol.name))
                        .collect::<Vec<_>>();
                    let matches = fuzzy::match_strings(
                        &candidates,
                        &params.query,
                        true,
                        true,
                        100,
                        &Default::default(),
                        executor,
                    )
                    .await;
                    Ok(Some(lsp::WorkspaceSymbolResponse::Flat(
                        matches
                            .into_iter()
                            .map(|mat| fake_symbols[mat.candidate_id].clone())
                            .collect(),
                    )))
                }
            },
        );

        let (picker, _workspace, cx) = open_the_picker(project, cx);
        cx.simulate_input("take_stock");
        cx.run_until_parked();

        picker.read_with(cx, |picker, _| {
            let names: Vec<&str> = picker
                .delegate
                .matches
                .iter()
                .map(|matched| matched.string.as_str())
                .collect();
            assert_eq!(
                names,
                vec!["take_stock_from_the_server"],
                "the server answered, so the index must stay out of the list: {names:?}"
            );
            assert!(
                !picker.delegate.cut_short,
                "the server's answer is never reported as cut short"
            );
        });
    }

    /// A list the index cut short must say so, rather than reading as though
    /// the project declares nothing else by that name.
    #[gpui::test]
    async fn a_list_cut_short_says_so(cx: &mut TestAppContext) {
        init_test(cx);
        let mut declarations = String::new();
        for at in 0..=symbol_index::index_semantics::MOST_PLACES_WORTH_OPENING {
            declarations.push_str(&format!("pub fn cutshort{at:05}() {{}}\n"));
        }
        let (_on_disk, project) = a_project_on_disk(&[("many.rs", &declarations)], cx).await;
        let (_index_at, _index) = an_index_for(project.clone(), cx);
        cx.executor().run_until_parked();

        let (picker, _workspace, cx) = open_the_picker(project, cx);
        cx.simulate_input("cutshort");
        cx.run_until_parked();

        picker.read_with(cx, |picker, _| {
            assert!(
                picker.delegate.cut_short,
                "more matches than are worth listing must be reported as cut short"
            );
            assert!(
                picker.delegate.match_count() > 0,
                "the header only shows alongside a list"
            );
        });
        picker.update_in(cx, |picker, window, cx| {
            assert!(
                picker.delegate.render_header(window, cx).is_some(),
                "a cut-short list must say so on screen, not only in the delegate"
            );
        });
    }

    fn symbol(name: &str, path: impl AsRef<Path>) -> lsp::SymbolInformation {
        #[allow(deprecated)]
        lsp::SymbolInformation {
            name: name.to_string(),
            kind: lsp::SymbolKind::FUNCTION,
            tags: None,
            deprecated: None,
            container_name: None,
            location: lsp::Location::new(
                lsp::Uri::from_file_path(path.as_ref()).unwrap(),
                lsp::Range::new(lsp::Position::new(0, 0), lsp::Position::new(0, 0)),
            ),
        }
    }
}
