use editor::Editor;
use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, ScrollHandle, Subscription, WeakEntity,
    Window, actions,
};
use semantic_index::relations::{
    Dependents, NoDependents, Relations, Usage, WhyNotUsage, what_this_rests_on,
};
use semantic_index::resolution::WhyNot;
use symbol_index::SymbolIndex;
use ui::prelude::*;
use ui::{Label, LabelSize, ScrollAxes, Scrollbars, WithScrollbar};
use util::ResultExt as _;
use workspace::Workspace;
use workspace::item::{Item, ItemEvent};

actions!(
    file_relations,
    [
        /// Shows which files depend on the file being read, what it declares,
        /// and which of those declarations nothing in the project uses.
        Toggle
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &Toggle, window, cx| {
            let Some(path) = the_file_being_read(workspace, cx) else {
                return;
            };
            // One tab, re-pointed, rather than one per file looked at: this
            // answers about whichever file the reader is in, and a pane filling
            // with a tab per file is the surface getting in the reader's way.
            let already_open = workspace
                .active_pane()
                .read(cx)
                .items()
                .find_map(|item| item.downcast::<FileRelationsView>());
            if let Some(open) = already_open {
                open.update(cx, |view, cx| view.look_at(path, cx));
                workspace.activate_item(&open, true, true, window, cx);
                return;
            }
            let Some(index) = symbol_index::of_project(workspace.project(), cx) else {
                return;
            };
            let handle = cx.entity().downgrade();
            let view = cx.new(|cx| FileRelationsView::new(index.downgrade(), handle, path, cx));
            workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
        });
    })
    .detach();
}

/// The path of the file in the active editor, keyed the way every row in the
/// store is: relative to the worktree, forward slashes.
fn the_file_being_read(workspace: &Workspace, cx: &App) -> Option<String> {
    let editor = workspace.active_item(cx)?.downcast::<Editor>()?;
    let buffer = editor.read(cx).buffer().read(cx).as_singleton()?;
    let file = buffer.read(cx).file()?;
    Some(file.path().to_string().replace('\\', "/"))
}

/// What there is to show, so that nothing on the surface is ever an empty list
/// standing in for an answer the index never gave.
enum Answer {
    /// The index is still reading the project.
    Building,
    /// A background pass holds the store, so the question cannot be put yet.
    Busy,
    /// There is no index for this project, or building one failed outright.
    None {
        reason: SharedString,
    },
    Ready(Relations),
}

/// What the index can say about the file being read: which files depend on it,
/// what it declares, and what became of each of those.
///
/// A workspace item rather than a panel or a hover, and for the reasons the
/// answer's own shape gives: it is three lists that want scrolling and a
/// paragraph of provenance that has to stay on screen while they are read, and
/// it is asked for deliberately rather than followed on every cursor move.
/// `structural_search` in this fork is the same shape of answer and is an item,
/// so this follows it rather than inventing a fourth kind of surface.
pub struct FileRelationsView {
    index: WeakEntity<SymbolIndex>,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    path: String,
    answer: Answer,
    scroll: ScrollHandle,
    _watching: Option<Subscription>,
}

impl FileRelationsView {
    pub fn new(
        index: WeakEntity<SymbolIndex>,
        workspace: WeakEntity<Workspace>,
        path: String,
        cx: &mut Context<Self>,
    ) -> Self {
        // Watched rather than read once: the first build finishes after the
        // reader has already opened this, and a surface left showing "still
        // reading the project" until it is reopened reads as a broken one.
        let watching = index
            .upgrade()
            .map(|index| cx.observe(&index, |this, _, cx| this.look(cx)));
        let mut this = Self {
            index,
            workspace,
            focus_handle: cx.focus_handle(),
            path,
            answer: Answer::Building,
            scroll: ScrollHandle::new(),
            _watching: watching,
        };
        this.look(cx);
        this
    }

    /// Points the surface at another file and answers about that one instead.
    pub fn look_at(&mut self, path: String, cx: &mut Context<Self>) {
        self.path = path;
        self.look(cx);
    }

    fn look(&mut self, cx: &mut Context<Self>) {
        let Some(index) = self.index.upgrade() else {
            self.answer = Answer::None {
                reason: "This project has no symbol index".into(),
            };
            cx.notify();
            return;
        };
        let index = index.read(cx);
        self.answer = match index.state() {
            symbol_index::State::NotBuilt => Answer::None {
                reason: "This project has nothing on disk to index".into(),
            },
            symbol_index::State::Failed { reason } => Answer::None {
                reason: SharedString::from(reason.to_string()),
            },
            symbol_index::State::Building => Answer::Building,
            symbol_index::State::Ready { .. } => match index.relations_of(&self.path) {
                Some(relations) => Answer::Ready(relations),
                None => Answer::Busy,
            },
        };
        cx.notify();
    }

    /// The name of the file being read, which is all of the path worth putting
    /// on a tab.
    fn file_name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }

    /// What is said in place of a list of dependents, and never the same thing
    /// for two different kinds of nothing.
    fn said_instead_of_dependents(&self) -> Option<String> {
        let Answer::Ready(relations) = &self.answer else {
            return None;
        };
        match &relations.dependents {
            Dependents::TheseFiles { files, .. } if files.is_empty() => Some(
                "No file in the project brings in a name this one declares. That is what the \
                 index found, not a proof that nothing does."
                    .to_string(),
            ),
            Dependents::TheseFiles { .. } => None,
            Dependents::CannotTell(NoDependents::NothingIsIndexed) => {
                Some("The index has never read this file, so it holds nothing to look for.".into())
            }
            Dependents::CannotTell(NoDependents::LanguageIsUnknown) => Some(
                "Nothing here knows what language this file is, so nothing can be said about \
                 what depends on it."
                    .into(),
            ),
            Dependents::CannotTell(NoDependents::ImportsAreNotRead { language }) => Some(format!(
                "Cannot tell: what a {language} file brings into scope is not read, so an empty \
                 list here would claim something nothing supports."
            )),
        }
    }

    fn open_at(&mut self, path: String, row: u32, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.index.upgrade() else {
            return;
        };
        let full = index.read(cx).root().join(&path);
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
}

/// What one declaration's fate reads as, and in which colour.
///
/// The three answers are deliberately not equally loud. A count is a plain
/// fact; "used nowhere" is the one a reader acts on by deleting code, so it is
/// marked as a warning rather than as a result; and a refusal says which gate
/// stopped it, because a reader who cannot see the reason cannot judge whether
/// to trust the answer.
fn usage_said(usage: &Usage) -> (String, Color) {
    match usage {
        Usage::Used { elsewhere: 1 } => ("Used in 1 place".to_string(), Color::Default),
        Usage::Used { elsewhere } => (format!("Used in {elsewhere} places"), Color::Default),
        Usage::UsedNowhere => ("Used nowhere".to_string(), Color::Warning),
        Usage::CannotTell(why) => (format!("Cannot tell — {}", why_said(why)), Color::Muted),
    }
}

fn why_said(why: &WhyNotUsage) -> String {
    match why {
        WhyNotUsage::TheName(WhyNot::DeclaredMoreThanOnce) => {
            "the project declares this name more than once".into()
        }
        WhyNotUsage::TheName(WhyNot::AlsoALocalBinding) => "the name is also bound locally".into(),
        WhyNotUsage::TheName(WhyNot::AMemberOfAType) => {
            "the name is a member of a type somewhere".into()
        }
        WhyNotUsage::TheName(WhyNot::NothingIsIndexed) => {
            "nothing in the index declares this name".into()
        }
        WhyNotUsage::TheName(WhyNot::NotEverythingIsExplained) => {
            "something written under this name was not accounted for".into()
        }
        WhyNotUsage::AlsoBoundLocally => {
            "the name is also bound locally, so a use may have been dropped as that binding".into()
        }
        WhyNotUsage::ImportsAreNotRead { language } => {
            format!("imports are not read for {language}")
        }
    }
}

impl EventEmitter<()> for FileRelationsView {}

impl Focusable for FileRelationsView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for FileRelationsView {
    type Event = ();

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        format!("Relations of {}", self.file_name()).into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::ListTree))
    }

    fn to_item_events(_event: &Self::Event, _f: &mut dyn FnMut(ItemEvent)) {}

    fn show_toolbar(&self) -> bool {
        false
    }
}

impl Render for FileRelationsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut body = v_flex().flex_none().w_full().gap_1();

        match &self.answer {
            Answer::Building => {
                body = body.child(
                    Label::new("The index is still reading this project")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                );
            }
            Answer::Busy => {
                body = body.child(
                    Label::new("The index is refreshing; ask again in a moment")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                );
            }
            Answer::None { reason } => {
                body = body.child(
                    Label::new(reason.clone())
                        .size(LabelSize::Small)
                        .color(Color::Error),
                );
            }
            Answer::Ready(relations) => {
                body = body.child(ui::cyberpunk::dialog_section("Depended upon by"));
                if let Dependents::TheseFiles { files, cut_short } = &relations.dependents {
                    if *cut_short {
                        body = body.child(
                            Label::new(format!(
                                "Showing the first {}; more files depend on this one than a list \
                                 can hold",
                                semantic_index::relations::MOST_WORTH_LISTING
                            ))
                            .size(LabelSize::XSmall)
                            .color(Color::Warning),
                        );
                    }
                    for (at, found) in files.iter().enumerate() {
                        let path = found.path.clone();
                        let row = found.row;
                        let brings_in = found.names.join(", ");
                        body = body.child(
                            h_flex()
                                .id(("dependent", at))
                                .debug_selector(move || format!("relations-dependent-{at}"))
                                .w_full()
                                .px_2()
                                .py_1()
                                .gap_2()
                                .items_center()
                                .cursor_pointer()
                                .hover(|hovered| hovered.bg(ui::cyberpunk::row_hovered()))
                                .on_click(cx.listener(move |view, _, window, cx| {
                                    view.open_at(path.clone(), row, window, cx)
                                }))
                                .child(Label::new(found.path.clone()).size(LabelSize::Small))
                                .child(div().flex_1())
                                .child(
                                    Label::new(brings_in)
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                ),
                        );
                    }
                }
                if let Some(said) = self.said_instead_of_dependents() {
                    body = body.child(Label::new(said).size(LabelSize::Small).color(Color::Muted));
                }

                body = body.child(ui::cyberpunk::dialog_section("Exports"));
                if relations.exports_cut_short {
                    body = body.child(
                        Label::new(format!(
                            "Showing the first {}; this file declares more than a list can hold",
                            semantic_index::relations::MOST_WORTH_LISTING
                        ))
                        .size(LabelSize::XSmall)
                        .color(Color::Warning),
                    );
                }
                if relations.exports.is_empty() {
                    body = body.child(
                        Label::new("The index found nothing declared at this file's top level")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    );
                }
                for (at, export) in relations.exports.iter().enumerate() {
                    let (said, colour) = usage_said(&export.usage);
                    let path = export.declaration.path.clone();
                    let row = export.declaration.line.saturating_sub(1);
                    let where_it_is =
                        format!("{}:{}", export.declaration.kind, export.declaration.line);
                    body = body.child(
                        h_flex()
                            .id(("export", at))
                            .debug_selector(move || format!("relations-export-{at}"))
                            .w_full()
                            .px_2()
                            .py_1()
                            .gap_2()
                            .items_center()
                            .cursor_pointer()
                            .hover(|hovered| hovered.bg(ui::cyberpunk::row_hovered()))
                            .on_click(cx.listener(move |view, _, window, cx| {
                                view.open_at(path.clone(), row, window, cx)
                            }))
                            .child(
                                Label::new(export.declaration.name.clone()).size(LabelSize::Small),
                            )
                            .child(
                                Label::new(where_it_is)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(div().flex_1())
                            .child(Label::new(said).size(LabelSize::XSmall).color(colour)),
                    );
                }
            }
        }

        let placed = match &self.answer {
            Answer::Ready(relations) => relations
                .placement
                .as_ref()
                .map(|placed| format!("{} · {}", placed.unit, placed.path)),
            _ => None,
        };

        v_flex()
            .id("file-relations")
            .debug_selector(|| "file-relations".to_string())
            .key_context("FileRelations")
            .track_focus(&self.focus_handle)
            .size_full()
            .p_4()
            .gap_3()
            .child(
                h_flex()
                    .flex_none()
                    .gap_2()
                    .items_center()
                    .child(
                        Label::new(format!("Relations of {}", self.file_name()))
                            .size(LabelSize::Large),
                    )
                    .child(div().flex_1())
                    .children(placed.map(|placed| {
                        Label::new(placed)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .into_any_element()
                    }))
                    .child(
                        Button::new("file-relations-again", "Ask again")
                            .label_size(LabelSize::Small)
                            .style(ui::cyberpunk::Rank::Quiet.style())
                            .on_click(cx.listener(|view, _, _window, cx| view.look(cx))),
                    ),
            )
            .child(
                // Never allowed to shrink: the note is what keeps a reader from
                // taking "used nowhere" for proof, and a note squeezed out of
                // sight by a long list is a claim made without its basis.
                div().flex_none().w_full().child(
                    Label::new(what_this_rests_on())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
            )
            .child(
                div()
                    .id("file-relations-body")
                    .debug_selector(|| "file-relations-body".to_string())
                    .flex_1()
                    .min_h_0()
                    .overflow_scroll()
                    .track_scroll(&self.scroll)
                    .child(body)
                    .custom_scrollbars(
                        Scrollbars::always_visible(ScrollAxes::Both)
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
    use fs::FakeFs;
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use project::Project;

    const STOCK: &str = "pub fn take_stock() -> u32 {\n    1\n}\n\npub fn unswept() {}\n";
    const ONE: &str = "use crate::stock::take_stock;\n\npub fn first() {\n    take_stock();\n}\n";
    const TWO: &str = "use crate::stock::take_stock;\n\npub fn second() {\n    take_stock();\n}\n";

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            editor::init(cx);
        });
    }

    /// A project that exists twice over at one path, the way every test of the
    /// index in this fork builds one: the editor reads the in-memory
    /// filesystem, and the index walks the real directory with the standard
    /// library rather than through the editor's filesystem.
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
        looking_at: &str,
        cx: &mut TestAppContext,
    ) -> (
        tempfile::TempDir,
        tempfile::TempDir,
        Entity<FileRelationsView>,
        VisualTestContext,
    ) {
        init_test(cx);
        let (project_at, project) =
            a_project(&[("stock.rs", STOCK), ("one.rs", ONE), ("two.rs", TWO)], cx).await;
        let index_at = tempfile::tempdir().expect("a directory for the index's own files");
        let index = cx.update(|cx| {
            symbol_index::ensure_index_at(project.clone(), index_at.path().join("symbol_index"), cx)
        });
        // Without this the index is read while its first pass is still running
        // and every answer here is the empty one -- a race that has already
        // cost this fork nine tests.
        cx.executor().run_until_parked();

        let looking_at = looking_at.to_string();
        let window = cx.add_window(|_window, cx| {
            FileRelationsView::new(index.downgrade(), WeakEntity::new_invalid(), looking_at, cx)
        });
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        let view = window.root(&mut cx).expect("the window was built");
        (project_at, index_at, view, cx)
    }

    fn relations(view: &Entity<FileRelationsView>, cx: &mut VisualTestContext) -> Relations {
        view.read_with(cx, |view, _| match &view.answer {
            Answer::Ready(relations) => relations.clone(),
            Answer::Building => panic!("the index was read before its first pass finished"),
            Answer::Busy => panic!("a pass still held the store"),
            Answer::None { reason } => panic!("no index: {reason}"),
        })
    }

    /// The whole point of the surface, with no language server anywhere: the two
    /// files that import this one are named, and nothing else is.
    #[gpui::test]
    async fn the_two_files_that_import_this_one_are_the_two_it_reports(cx: &mut TestAppContext) {
        let (_project_at, _index_at, view, mut cx) = a_view("stock.rs", cx).await;
        let relations = relations(&view, &mut cx);
        let Dependents::TheseFiles { files, cut_short } = &relations.dependents else {
            panic!(
                "expected a list of dependents, found {:?}",
                relations.dependents
            );
        };
        assert!(!cut_short);
        let named: Vec<&str> = files.iter().map(|found| found.path.as_str()).collect();
        assert_eq!(named, vec!["one.rs", "two.rs"]);
        for found in files {
            assert_eq!(found.names, vec!["take_stock".to_string()]);
        }
        assert!(
            view.read_with(&cx, |view, _| view.said_instead_of_dependents())
                .is_none(),
            "there is a list, so nothing is said in place of one"
        );
    }

    /// The two claims a reader acts on, in the words the surface puts on screen.
    #[gpui::test]
    async fn what_is_used_and_what_is_used_nowhere_read_differently(cx: &mut TestAppContext) {
        let (_project_at, _index_at, view, mut cx) = a_view("stock.rs", cx).await;
        let relations = relations(&view, &mut cx);
        let said = |name: &str| {
            relations
                .exports
                .iter()
                .find(|export| export.declaration.name == name)
                .map(|export| usage_said(&export.usage))
                .unwrap_or_else(|| panic!("{name} is not among the exports"))
        };
        let (used, used_colour) = said("take_stock");
        assert!(used.starts_with("Used in "), "{used}");
        assert!(!used.contains("nowhere"), "{used}");
        assert_eq!(used_colour, Color::Default);

        let (nowhere, nowhere_colour) = said("unswept");
        assert_eq!(nowhere, "Used nowhere");
        assert_eq!(
            nowhere_colour,
            Color::Warning,
            "the claim a reader deletes code over is marked as one"
        );
    }

    /// An empty list would read as "nothing depends on this file". A file the
    /// index has never read has to say that instead.
    #[gpui::test]
    async fn a_file_the_index_never_read_says_so_rather_than_showing_an_empty_list(
        cx: &mut TestAppContext,
    ) {
        let (_project_at, _index_at, view, mut cx) = a_view("nowhere/absent.rs", cx).await;
        let relations = relations(&view, &mut cx);
        assert_eq!(
            relations.dependents,
            Dependents::CannotTell(NoDependents::NothingIsIndexed)
        );
        let said = view
            .read_with(&cx, |view, _| view.said_instead_of_dependents())
            .expect("something is said in place of a list");
        assert!(said.contains("never read this file"), "{said}");
    }

    /// The tab has to name the file, or two of these are indistinguishable.
    #[gpui::test]
    async fn the_tab_names_the_file_being_read(cx: &mut TestAppContext) {
        let (_project_at, _index_at, view, cx) = a_view("stock.rs", cx).await;
        let named = view.read_with(&cx, |view, cx| view.tab_content_text(0, cx));
        assert_eq!(named, SharedString::from("Relations of stock.rs"));
    }

    /// Pointed at another file, the same tab answers about that one.
    #[gpui::test]
    async fn the_surface_can_be_pointed_at_another_file(cx: &mut TestAppContext) {
        let (_project_at, _index_at, view, mut cx) = a_view("stock.rs", cx).await;
        view.update(&mut cx, |view, cx| view.look_at("one.rs".to_string(), cx));
        let relations = relations(&view, &mut cx);
        assert_eq!(relations.path, "one.rs");
        let names: Vec<&str> = relations
            .exports
            .iter()
            .map(|export| export.declaration.name.as_str())
            .collect();
        assert!(names.contains(&"first"), "{names:?}");
    }
}
