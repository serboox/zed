use std::path::PathBuf;

use editor::{Editor, MultiBuffer};
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, ScrollHandle, Task, WeakEntity,
    Window, actions,
};
use project::Project;
use semantic_index::languages::{self, Readable};
use semantic_index::structural::{QueryProblem, StructuralMatch};
use ui::prelude::*;
use ui::{Label, LabelSize, ScrollAxes, Scrollbars, WithScrollbar};
use util::ResultExt as _;
use workspace::Workspace;
use workspace::item::{Item, ItemEvent};

actions!(
    structural_search,
    [
        /// Opens a window for searching the project by the shape of its code.
        Toggle
    ]
);

/// How many matches are kept. A structural query written loosely can match most
/// of a project, and a list nobody can read is not a better answer than a list
/// that says it stopped.
const AT_MOST: usize = 2000;

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &Toggle, window, cx| {
            let project = workspace.project().clone();
            let handle = cx.entity().downgrade();
            let view = cx.new(|cx| StructuralSearchView::new(project, handle, window, cx));
            workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
        });
    })
    .detach();
}

/// What the search is doing, so an empty list can always say which kind of empty
/// it is.
#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    /// Nothing has been asked yet.
    Waiting,
    /// A pass is under way. Matches arrive as they are found.
    Looking,
    /// The pass finished. `stopped_early` when the cap was reached.
    Finished { stopped_early: bool },
    /// The query itself does not compile, with the place in the reader's own
    /// text where it went wrong.
    Refused(QueryProblem),
    /// Something else went wrong, such as a project with nothing to read.
    Failed(String),
}

pub struct StructuralSearchView {
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    query: Entity<Editor>,
    /// Which language's grammar the query is compiled against. A query is
    /// written for one grammar: the node names in it belong to that grammar and
    /// mean nothing in another.
    language: String,
    readable: Vec<String>,
    found: Vec<StructuralMatch>,
    state: State,
    scroll: ScrollHandle,
    _searching: Option<Task<()>>,
}

impl StructuralSearchView {
    pub fn new(
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let query = cx.new(|cx| {
            let buffer = cx.new(|cx| {
                MultiBuffer::singleton(cx.new(|cx| language::Buffer::local(String::new(), cx)), cx)
            });
            let mut editor = Editor::new(
                editor::EditorMode::AutoHeight {
                    min_lines: 3,
                    max_lines: Some(12),
                },
                buffer,
                None,
                window,
                cx,
            );
            editor.set_placeholder_text(
                "A tree-sitter query, such as (call_expression function: (identifier) @call)",
                window,
                cx,
            );
            editor
        });
        let (readable, _) = languages::readable();
        let mut readable: Vec<String> = readable.into_iter().map(|one| one.name).collect();
        readable.sort();
        let language = readable
            .iter()
            .find(|name| *name == "rust")
            .cloned()
            .or_else(|| readable.first().cloned())
            .unwrap_or_default();
        Self {
            project,
            workspace,
            focus_handle: cx.focus_handle(),
            query,
            language,
            readable,
            found: Vec::new(),
            state: State::Waiting,
            scroll: ScrollHandle::new(),
            _searching: None,
        }
    }

    /// The project's own root, which is what a structural pass walks.
    fn root(&self, cx: &App) -> Option<PathBuf> {
        self.project
            .read(cx)
            .visible_worktrees(cx)
            .find(|worktree| worktree.read(cx).is_local())
            .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
    }

    fn language_named(&self, cx: &App) -> Option<Readable> {
        let _ = cx;
        let (readable, _) = languages::readable();
        readable
            .into_iter()
            .find(|language| language.name == self.language)
    }

    fn look(&mut self, cx: &mut Context<Self>) {
        let text = self.query.read(cx).text(cx);
        if text.trim().is_empty() {
            self.found.clear();
            self.state = State::Waiting;
            cx.notify();
            return;
        }
        let Some(root) = self.root(cx) else {
            self.state = State::Failed("This project has nothing on disk to search.".into());
            cx.notify();
            return;
        };
        let Some(language) = self.language_named(cx) else {
            self.state = State::Failed(format!("No grammar is shipped for {}.", self.language));
            cx.notify();
            return;
        };

        self.found.clear();
        self.state = State::Looking;
        cx.notify();

        let cores = cx.background_executor().num_cpus().saturating_sub(1).max(1);
        // Matches are drained on a background thread and handed over in batches.
        // The pass sends on a blocking channel, which the foreground must never
        // wait on; batching also keeps a query that matches thousands of times
        // from asking for a redraw thousands of times.
        let (sender, mut receiver) = futures::channel::mpsc::unbounded();
        self._searching = Some(cx.spawn(async move |this, cx| {
            let started = cx.background_spawn(async move {
                match semantic_index::structural::search(&root, &language, &text, cores) {
                    Ok(matches) => {
                        let mut batch = Vec::new();
                        for found in matches.iter() {
                            batch.push(found);
                            if batch.len() >= 64 {
                                if sender
                                    .unbounded_send(Ok(std::mem::take(&mut batch)))
                                    .is_err()
                                {
                                    return;
                                }
                            }
                        }
                        if !batch.is_empty() {
                            sender.unbounded_send(Ok(batch)).ok();
                        }
                    }
                    Err(problem) => {
                        sender.unbounded_send(Err(problem)).ok();
                    }
                }
            });

            use futures::StreamExt as _;
            while let Some(arrived) = receiver.next().await {
                let carry_on = this
                    .update(cx, |this, cx| match arrived {
                        Ok(batch) => {
                            let room = AT_MOST.saturating_sub(this.found.len());
                            let taking = batch.len().min(room);
                            this.found.extend(batch.into_iter().take(taking));
                            cx.notify();
                            this.found.len() < AT_MOST
                        }
                        Err(problem) => {
                            this.state = State::Refused(problem);
                            cx.notify();
                            false
                        }
                    })
                    .unwrap_or(false);
                if !carry_on {
                    break;
                }
            }
            started.await;
            this.update(cx, |this, cx| {
                if !matches!(this.state, State::Refused(_) | State::Failed(_)) {
                    this.state = State::Finished {
                        stopped_early: this.found.len() >= AT_MOST,
                    };
                }
                cx.notify();
            })
            .log_err();
        }));
    }

    fn open(&mut self, at: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(found) = self.found.get(at).cloned() else {
            return;
        };
        let Some(root) = self.root(cx) else {
            return;
        };
        let full = root.join(&found.path);
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |_, cx| {
            let opened = workspace.update_in(cx, |workspace, window, cx| {
                workspace.open_abs_path(full, Default::default(), window, cx)
            });
            let Ok(opened) = opened else {
                return;
            };
            // Awaited rather than dropped: opening is the task, and a dropped
            // task is a cancelled one.
            let Ok(item) = opened.await else {
                return;
            };
            let Some(editor) = item.downcast::<Editor>() else {
                return;
            };
            editor
                .update_in(cx, |editor, window, cx| {
                    let row = found.line.saturating_sub(1);
                    editor.change_selections(
                        editor::SelectionEffects::default(),
                        window,
                        cx,
                        |selections| {
                            selections.select_ranges([
                                language::Point::new(row, 0)..language::Point::new(row, 0)
                            ]);
                        },
                    );
                })
                .log_err();
        })
        .detach();
    }

    /// What to say in place of a list, and never the same thing for two
    /// different kinds of nothing.
    fn nothing_to_show(&self) -> Option<String> {
        if !self.found.is_empty() {
            return None;
        }
        Some(match &self.state {
            State::Waiting => "Write a query and press Search.".to_string(),
            State::Looking => "Searching…".to_string(),
            State::Finished { .. } => "Nothing in this project has that shape.".to_string(),
            State::Refused(problem) => format!(
                "The query does not compile: {} at line {}, column {}.",
                problem.message,
                problem.row + 1,
                problem.column + 1
            ),
            State::Failed(said) => said.clone(),
        })
    }
}

impl EventEmitter<()> for StructuralSearchView {}

impl Focusable for StructuralSearchView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for StructuralSearchView {
    type Event = ();

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Structural search".into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::MagnifyingGlass))
    }

    fn to_item_events(_event: &Self::Event, _f: &mut dyn FnMut(ItemEvent)) {}

    fn show_toolbar(&self) -> bool {
        false
    }
}

impl Render for StructuralSearchView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let languages: Vec<String> = self.readable.clone();
        let chosen = self.language.clone();

        let language_row = ui::cyberpunk::segmented(languages.into_iter().map(|name| {
            let this = name.clone();
            let is_chosen = name == chosen;
            Button::new(SharedString::from(format!("language-{name}")), name)
                .label_size(LabelSize::Small)
                .style(match is_chosen {
                    true => ui::cyberpunk::Rank::Accent.style(),
                    false => ui::cyberpunk::Rank::Quiet.style(),
                })
                .on_click(cx.listener(move |view, _, _window, cx| {
                    view.language = this.clone();
                    cx.notify();
                }))
                .into_any_element()
        }));

        let mut rows = v_flex().flex_none().gap_0p5();
        for (at, found) in self.found.iter().enumerate() {
            let where_it_is = format!("{}:{}", found.path, found.line);
            let excerpt = found.excerpt.clone();
            rows = rows.child(
                h_flex()
                    .id(("structural-result", at))
                    .debug_selector(move || format!("structural-result-{at}"))
                    .w_full()
                    .px_2()
                    .py_1()
                    .gap_2()
                    .items_center()
                    .cursor_pointer()
                    .hover(|row| row.bg(ui::cyberpunk::row_hovered()))
                    .on_click(cx.listener(move |view, _, window, cx| view.open(at, window, cx)))
                    .child(
                        Label::new(where_it_is)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(Label::new(excerpt).size(LabelSize::Small)),
            );
        }

        v_flex()
            .id("structural-search")
            .debug_selector(|| "structural-search".to_string())
            .key_context("StructuralSearch")
            .track_focus(&self.focus_handle)
            .size_full()
            .p_4()
            .gap_3()
            .child(
                h_flex()
                    .flex_none()
                    .gap_2()
                    .items_center()
                    .child(Label::new("Search by shape").size(LabelSize::Large))
                    .child(div().flex_1())
                    .child(language_row)
                    .child(
                        div()
                            .debug_selector(|| "structural-search-run".to_string())
                            .child(
                                Button::new("structural-search-run", "Search")
                                    .style(ui::cyberpunk::Rank::Accent.style())
                                    .on_click(cx.listener(|view, _, _window, cx| view.look(cx))),
                            ),
                    ),
            )
            .child(div().flex_none().child(self.query.clone()))
            .children(self.nothing_to_show().map(|said| {
                Label::new(said)
                    .size(LabelSize::Small)
                    .color(match self.state {
                        State::Refused(_) | State::Failed(_) => Color::Error,
                        _ => Color::Muted,
                    })
                    .into_any_element()
            }))
            .children(
                matches!(
                    self.state,
                    State::Finished {
                        stopped_early: true
                    }
                )
                .then(|| {
                    Label::new(format!(
                        "Showing the first {AT_MOST}; the query matches more than a list can hold."
                    ))
                    .size(LabelSize::XSmall)
                    .color(Color::Warning)
                    .into_any_element()
                }),
            )
            .child(
                div()
                    .id("structural-results")
                    .debug_selector(|| "structural-results".to_string())
                    .flex_1()
                    .min_h_0()
                    .overflow_scroll()
                    .track_scroll(&self.scroll)
                    .child(rows)
                    .custom_scrollbars(
                        Scrollbars::always_visible(ScrollAxes::Vertical)
                            .tracked_scroll_handle(&self.scroll),
                        window,
                        cx,
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};
    use project::FakeFs;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            editor::init(cx);
        });
    }

    /// A project that exists twice over at one and the same path: the editor's
    /// side is the deterministic in-memory filesystem, and the search's side is
    /// a real directory, because a structural pass walks and reads the disk
    /// itself rather than going through the editor's filesystem abstraction.
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

    async fn a_view(
        files: &[(&str, &str)],
        cx: &mut TestAppContext,
    ) -> (
        tempfile::TempDir,
        Entity<StructuralSearchView>,
        VisualTestContext,
    ) {
        init_test(cx);
        let (held, project) = a_project(files, cx).await;
        let window = cx.add_window(|window, cx| {
            StructuralSearchView::new(project, WeakEntity::new_invalid(), window, cx)
        });
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        let view = window.root(&mut cx).expect("the window was built");
        (held, view, cx)
    }

    fn ask(view: &Entity<StructuralSearchView>, query: &str, cx: &mut VisualTestContext) {
        view.update_in(cx, |view, window, cx| {
            view.query.update(cx, |editor, cx| {
                editor.set_text(query, window, cx);
            });
            view.look(cx);
        });
        cx.run_until_parked();
    }

    /// The plan's own gate for this step: a query that does not compile is shown
    /// to the reader with the place in their own text where it went wrong, and
    /// never as a silent list of no results -- which reads as "nothing in this
    /// project has that shape", a different and wrong answer.
    #[gpui::test]
    async fn a_query_that_does_not_compile_says_where_it_went_wrong(cx: &mut TestAppContext) {
        let (_held, view, mut cx) = a_view(&[("one.rs", "pub fn work() {}\n")], cx).await;
        ask(&view, "(no_such_node) @thing", &mut cx);

        view.read_with(&cx, |view, _| {
            let State::Refused(problem) = &view.state else {
                panic!("expected the query to be refused, found {:?}", view.state);
            };
            assert!(!problem.message.is_empty(), "the reason is stated");
            let said = view.nothing_to_show().expect("something is said instead");
            assert!(
                said.contains("does not compile") && said.contains("line"),
                "{said}"
            );
            assert!(view.found.is_empty());
        });
    }

    #[gpui::test]
    async fn a_query_that_matches_lists_what_it_found(cx: &mut TestAppContext) {
        let (_held, view, mut cx) = a_view(
            &[(
                "one.rs",
                "pub fn first() {}\npub fn second() {}\npub struct Third;\n",
            )],
            cx,
        )
        .await;
        ask(
            &view,
            "(function_item name: (identifier) @name) @item",
            &mut cx,
        );

        view.read_with(&cx, |view, _| {
            assert!(
                matches!(view.state, State::Finished { .. }),
                "found {:?}",
                view.state
            );
            let lines: Vec<u32> = view.found.iter().map(|found| found.line).collect();
            assert_eq!(lines, vec![1, 2], "the two functions, not the struct");
            assert!(view.nothing_to_show().is_none(), "there is a list to show");
        });
    }

    /// An empty list has to say which kind of empty it is: nothing asked yet is
    /// not the same answer as nothing found.
    #[gpui::test]
    async fn an_empty_list_says_which_kind_of_empty_it_is(cx: &mut TestAppContext) {
        let (_held, view, mut cx) = a_view(&[("one.rs", "pub fn work() {}\n")], cx).await;

        let before = view.read_with(&cx, |view, _| view.nothing_to_show());
        assert!(
            before.as_deref().is_some_and(|said| said.contains("press")),
            "{before:?}"
        );

        ask(&view, "(struct_item) @item", &mut cx);
        let after = view.read_with(&cx, |view, _| view.nothing_to_show());
        assert!(
            after
                .as_deref()
                .is_some_and(|said| said.contains("Nothing in this project")),
            "{after:?}"
        );
        assert_ne!(before, after, "the two kinds of empty read differently");
    }
}
