use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, Render, ScrollHandle,
    SharedString, Subscription, WeakEntity, Window,
};
use task::TaskTemplate;
use ui::cyberpunk::CyberpunkSurface as _;
use ui::{ButtonLike, KeyBinding, PopoverMenu, Tooltip, WithScrollbar, cyberpunk, prelude::*};
use util::ResultExt as _;
use workspace::Workspace;

use crate::configurations_file::Kind;
use crate::configurations_store::ConfigurationsStore;

/// The plaque's own height, from the mockup: it sits in a title bar, so it is
/// the bar's row and not a button's.
const PLAQUE_HEIGHT: f32 = 26.0;

/// Which way of running the switcher is pointing at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pointing {
    /// A configuration one of the project's files holds.
    Kept { kind: Kind, at: usize },
    /// A way that was run on the spot and is still remembered, named rather than
    /// numbered: the list of them shifts as things are run, evicted and pinned, and
    /// an index would quietly come to mean a different way.
    Remembered { label: String },
}

/// The switcher above the editor: what will run, and the two presses that run it.
///
/// It is the one control that answers "again, the same way" without opening
/// anything -- which is the whole point of keeping configurations at all.
pub struct ConfigurationsToolbar {
    store: Entity<ConfigurationsStore>,
    workspace: WeakEntity<Workspace>,
    pointing: Option<Pointing>,
    _subscriptions: Vec<Subscription>,
}

impl ConfigurationsToolbar {
    pub fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        let store = crate::configurations_store::store_for(workspace.project(), cx);
        let subscription = cx.observe(&store, |toolbar, _, cx| {
            toolbar.keep_pointing_at_something(cx);
            cx.notify();
        });
        let mut toolbar = Self {
            store,
            workspace: workspace.weak_handle(),
            pointing: None,
            _subscriptions: vec![subscription],
        };
        toolbar.keep_pointing_at_something(cx);
        toolbar
    }

    /// The first configuration the project keeps, or the newest way run on the
    /// spot, whenever what was pointed at is gone.
    fn keep_pointing_at_something(&mut self, cx: &App) {
        if self.what_it_points_at(cx).is_some() {
            return;
        }
        let store = self.store.read(cx);
        self.pointing = match store.of_kind(Kind::Task).configurations.is_empty() {
            false => Some(Pointing::Kept {
                kind: Kind::Task,
                at: 0,
            }),
            true => match store.of_kind(Kind::Debug).configurations.is_empty() {
                false => Some(Pointing::Kept {
                    kind: Kind::Debug,
                    at: 0,
                }),
                true => store.temporary().first().map(|task| Pointing::Remembered {
                    label: task.label.clone(),
                }),
            },
        };
    }

    /// The name of what will run, and whether it is written down anywhere.
    pub fn what_it_points_at(&self, cx: &App) -> Option<(SharedString, bool)> {
        let store = self.store.read(cx);
        match self.pointing.as_ref()? {
            Pointing::Kept { kind, at } => store
                .get(*kind, *at)
                .map(|configuration| (SharedString::from(configuration.shown_label()), true)),
            Pointing::Remembered { label } => store
                .temporary()
                .iter()
                .find(|task| &task.label == label)
                .map(|task| (SharedString::from(task.label.clone()), false)),
        }
    }

    fn point_at(&mut self, pointing: Pointing, cx: &mut Context<Self>) {
        self.pointing = Some(pointing);
        cx.notify();
    }

    /// The task the switcher would run, if it is a task at all.
    fn task_it_points_at(&self, cx: &App) -> Option<TaskTemplate> {
        let store = self.store.read(cx);
        match self.pointing.as_ref()? {
            Pointing::Kept { kind, at } => store.get(*kind, *at)?.task.clone(),
            Pointing::Remembered { label } => store
                .temporary()
                .iter()
                .find(|task| &task.label == label)
                .cloned(),
        }
    }

    fn run(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let scenario = match self.pointing.as_ref() {
            Some(Pointing::Kept { kind, at }) => store
                .get(*kind, *at)
                .and_then(|configuration| configuration.scenario.clone()),
            _ => None,
        };
        if let Some(scenario) = scenario {
            // A debug configuration has only one way of being started.
            self.start_debugging(scenario, window, cx);
            return;
        }
        let Some(task) = self.task_it_points_at(cx) else {
            return;
        };
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |_, cx| {
            crate::configurations_view::run_a_task(&workspace, task, cx).await;
        })
        .detach();
    }

    fn debug(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let scenario = match self.pointing.as_ref() {
            Some(Pointing::Kept { kind, at }) => store
                .get(*kind, *at)
                .and_then(|configuration| configuration.scenario.clone()),
            _ => None,
        };
        match scenario {
            Some(scenario) => self.start_debugging(scenario, window, cx),
            // A task is not a debug configuration; the window that lists them is
            // where one is written for it, rather than one being conjured here.
            None => {
                window.dispatch_action(
                    Box::new(zed_actions::run_configurations::OpenRunConfigurations),
                    cx,
                );
            }
        }
    }

    fn start_debugging(
        &mut self,
        scenario: task::DebugScenario,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |_, cx| {
            crate::configurations_view::start_a_debug_session(&workspace, scenario, cx).await;
        })
        .detach();
    }

    /// Keeps the temporary that carries this name, wherever it now sits: the list
    /// the reader clicked in was made before they clicked, and what is
    /// remembered shifts as things are run and evicted.
    fn pin_named(&mut self, label: &str, cx: &mut Context<Self>) {
        let at = self
            .store
            .read(cx)
            .temporary()
            .iter()
            .position(|task| task.label == label);
        if let Some(at) = at {
            self.pin(at, cx);
        }
    }

    fn pin(&mut self, at: usize, cx: &mut Context<Self>) {
        let writing = self
            .store
            .update(cx, |store, cx| store.pin_temporary(at, cx));
        cx.spawn(async move |toolbar, cx| {
            writing.await.ok();
            toolbar
                .update(cx, |toolbar, cx| {
                    toolbar.keep_pointing_at_something(cx);
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    /// What the dropdown lists: the ones the project keeps, then the ones run on
    /// the spot, each with what pressing it would do.
    pub fn listed(&self, cx: &App) -> Vec<(SharedString, Pointing)> {
        let store = self.store.read(cx);
        let mut listed = Vec::new();
        for kind in [Kind::Task, Kind::Debug] {
            for (at, configuration) in store.of_kind(kind).configurations.iter().enumerate() {
                listed.push((
                    SharedString::from(configuration.shown_label()),
                    Pointing::Kept { kind, at },
                ));
            }
        }
        for task in store.temporary() {
            listed.push((
                SharedString::from(task.label.clone()),
                Pointing::Remembered {
                    label: task.label.clone(),
                },
            ));
        }
        listed
    }
}

impl Render for ConfigurationsToolbar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let pointing_at = self.what_it_points_at(cx);
        let toolbar = cx.entity();
        let running = pointing_at.is_some();

        h_flex()
            .id("run-configurations-toolbar")
            .debug_selector(|| "run-configurations-toolbar".to_string())
            .flex_none()
            .gap_1()
            .items_center()
            .child(
                PopoverMenu::new("run-configurations-switcher")
                    .anchor(gpui::Anchor::TopLeft)
                    .trigger_with_tooltip(
                        Self::plaque(pointing_at.clone(), cx),
                        Tooltip::text(match &pointing_at {
                            Some(_) => "What will run",
                            None => "Say how this project is run",
                        }),
                    )
                    .menu(move |window, cx| {
                        Some(ConfigurationsList::new(toolbar.clone(), window, cx))
                    }),
            )
            .when(running, |plaque| {
                plaque
                    .child(
                        IconButton::new("run-configurations-run", IconName::PlayFilled)
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Accent)
                            .tooltip(Tooltip::text("Run it"))
                            .on_click(
                                cx.listener(|toolbar, _, window, cx| toolbar.run(window, cx)),
                            ),
                    )
                    .child(
                        IconButton::new("run-configurations-debug", IconName::Debug)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Debug it"))
                            .on_click(
                                cx.listener(|toolbar, _, window, cx| toolbar.debug(window, cx)),
                            ),
                    )
            })
    }
}

impl ConfigurationsToolbar {
    /// The plaque itself: the glyph of what will run, its name, and the arrow
    /// that says there are others. Wide enough to read a name in, and no wider,
    /// since the bar's two ends have to fit beside it.
    fn plaque(pointing_at: Option<(SharedString, bool)>, cx: &App) -> ButtonLike {
        let (name, kept) = match pointing_at {
            Some((name, kept)) => (name, kept),
            None => (SharedString::from("Nothing to run yet"), true),
        };
        // A `ButtonLike` rather than a styled div: what opens the list has to be
        // clickable and toggleable for the popover to drive it, and a bare div is
        // neither.
        ButtonLike::new("run-configurations-plaque")
            .style(ButtonStyle::OutlinedCustom(cyberpunk::border_dim()))
            .size(ButtonSize::None)
            .height(px(PLAQUE_HEIGHT).into())
            .child(
                h_flex()
                    .debug_selector(|| "run-configurations-plaque".to_string())
                    .h(px(PLAQUE_HEIGHT))
                    .w(px(280.))
                    .min_w_0()
                    .max_w(px(340.))
                    .px_2()
                    .gap_2()
                    .items_center()
                    .child(
                        Icon::new(match kept {
                            true => IconName::PlayFilled,
                            false => IconName::PlayOutlined,
                        })
                        .size(IconSize::XSmall)
                        .color(Color::Accent),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .cyberpunk_monospace(cx)
                            .text_size(px(13.))
                            .text_color(cyberpunk::text_primary())
                            .when(!kept, |name| name.italic())
                            .child(name),
                    )
                    .child(
                        Icon::new(IconName::ChevronDown)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
    }
}

/// One row of the list: what it is called, what pointing at it means, and which
/// of the three kinds it is -- which is the whole of its look.
struct Row {
    name: SharedString,
    pointing: Pointing,
    kind: RowKind,
}

#[derive(PartialEq, Eq)]
enum RowKind {
    /// A task one of the project's files holds.
    Kept,
    /// A debug configuration one of the project's files holds.
    Debugged,
    /// Run on the spot and still remembered.
    Temporary,
}

impl RowKind {
    fn of(pointing: &Pointing) -> Self {
        match pointing {
            Pointing::Kept {
                kind: Kind::Debug, ..
            } => RowKind::Debugged,
            Pointing::Kept { .. } => RowKind::Kept,
            Pointing::Remembered { .. } => RowKind::Temporary,
        }
    }

    fn heading(&self) -> &'static str {
        match self {
            RowKind::Temporary => "RUN ON THE SPOT",
            _ => "KEPT IN THE PROJECT",
        }
    }

    fn icon(&self) -> IconName {
        match self {
            RowKind::Kept => IconName::PlayFilled,
            RowKind::Debugged => IconName::Debug,
            RowKind::Temporary => IconName::PlayOutlined,
        }
    }
}

/// The list the plaque drops down. A view of its own rather than a context
/// menu: the rows carry a heading, two hints on the one that is chosen, and an
/// action of their own for keeping a temporary one, and a menu of entries can
/// carry none of that.
pub struct ConfigurationsList {
    toolbar: WeakEntity<ConfigurationsToolbar>,
    focus: FocusHandle,
    rows: Vec<Row>,
    /// The row the arrow keys are on, which is the one Enter runs.
    highlighted: usize,
    scroll: ScrollHandle,
}

impl EventEmitter<gpui::DismissEvent> for ConfigurationsList {}

impl Focusable for ConfigurationsList {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl ConfigurationsList {
    /// Every row is this tall, from the mockup: two lines of this list take the
    /// room one line of the form does, so a long list is still a list.
    const ROW_HEIGHT: f32 = 28.0;
    /// As wide as the mockup has it -- half again the plaque, so a name that the
    /// plaque truncates is readable here.
    const WIDTH: f32 = 420.0;
    /// The list scrolls past this; the actions below the rule stay put.
    const MOST_ROWS_SHOWN: f32 = 12.0;

    fn new(
        toolbar: Entity<ConfigurationsToolbar>,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        let listed = toolbar.read(cx).listed(cx);
        let pointing = toolbar.read(cx).pointing.clone();
        let rows: Vec<Row> = listed
            .into_iter()
            .map(|(name, pointing)| Row {
                kind: RowKind::of(&pointing),
                name,
                pointing,
            })
            .collect();
        // Opened on the row that would run, so Enter means what the plaque says.
        let highlighted = pointing
            .and_then(|pointing| rows.iter().position(|row| row.pointing == pointing))
            .unwrap_or(0);
        let list = cx.new(|cx| Self {
            toolbar: toolbar.downgrade(),
            focus: cx.focus_handle(),
            rows,
            highlighted,
            scroll: ScrollHandle::new(),
        });
        // Focused, or the arrow keys and Enter would go to whatever had the
        // focus before the list opened.
        list.read(cx).focus.clone().focus(window, cx);
        list
    }

    fn move_to(&mut self, row: usize, cx: &mut Context<Self>) {
        if self.rows.is_empty() {
            return;
        }
        self.highlighted = row.min(self.rows.len() - 1);
        self.scroll.scroll_to_item(self.child_of(self.highlighted));
        cx.notify();
    }

    /// Where a row sits among the scroll container's children, which is its own
    /// place plus the headings drawn above it. Scrolling to the row's own index
    /// would land on a heading, and further off with every heading passed.
    fn child_of(&self, at: usize) -> usize {
        let mut children: usize = 0;
        let mut said: Option<&'static str> = None;
        for row in self.rows.iter().take(at + 1) {
            let heading = row.kind.heading();
            if said != Some(heading) {
                said = Some(heading);
                children += 1;
            }
            children += 1;
        }
        children.saturating_sub(1)
    }

    fn choose(&mut self, at: usize, debugging: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(at) else {
            return;
        };
        let pointing = row.pointing.clone();
        self.toolbar
            .update(cx, |toolbar, cx| {
                toolbar.point_at(pointing, cx);
                match debugging {
                    true => toolbar.debug(window, cx),
                    false => toolbar.run(window, cx),
                }
            })
            .log_err();
        cx.emit(gpui::DismissEvent);
    }

    /// Points the plaque at a row without running it, which is what clicking one
    /// means: the reader is choosing what the button will do next.
    fn point_at(&mut self, at: usize, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(at) else {
            return;
        };
        let pointing = row.pointing.clone();
        self.toolbar
            .update(cx, |toolbar, cx| toolbar.point_at(pointing, cx))
            .log_err();
        cx.emit(gpui::DismissEvent);
    }

    fn keep_in_the_project(&mut self, label: SharedString, cx: &mut Context<Self>) {
        self.toolbar
            .update(cx, |toolbar, cx| toolbar.pin_named(&label, cx))
            .log_err();
        cx.emit(gpui::DismissEvent);
    }

    /// The key that does this, if there is one. An unbound action gets no hint
    /// rather than an empty box where one would be.
    fn hint(
        selector: &'static str,
        action: &dyn gpui::Action,
        focus: &FocusHandle,
        window: &Window,
        cx: &App,
    ) -> Option<AnyElement> {
        let binding = KeyBinding::for_action_in(action, focus, cx);
        binding.has_binding(window).then(|| {
            div()
                .debug_selector(move || selector.to_string())
                .child(binding)
                .into_any_element()
        })
    }

    fn render_row(&self, at: usize, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(row) = self.rows.get(at) else {
            return div().into_any_element();
        };
        let temporary = row.kind == RowKind::Temporary;
        let chosen = at == self.highlighted;
        let keeping = temporary.then(|| row.name.clone());
        h_flex()
            .id(("configuration-row", at))
            .debug_selector(move || format!("CONFIGURATION-{at}"))
            .h(px(Self::ROW_HEIGHT))
            .w_full()
            .px_2()
            .gap_2()
            .items_center()
            .when(chosen, |row| row.bg(cyberpunk::row_chosen()))
            .hover(|row| row.bg(cyberpunk::row_hovered()))
            .child(
                Icon::new(row.kind.icon())
                    .size(IconSize::XSmall)
                    .color(match temporary {
                        true => Color::Muted,
                        false => Color::Accent,
                    }),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .cyberpunk_monospace(cx)
                    .text_size(px(13.))
                    .text_color(match temporary {
                        true => cyberpunk::text_secondary(),
                        false => cyberpunk::text_primary(),
                    })
                    .when(temporary, |name| name.italic())
                    .child(row.name.clone()),
            )
            // A temporary one is offered the thing it lacks: a place in the
            // project's own files.
            .when_some(keeping, |row, label| {
                row.child(
                    div()
                        .id(("keep-it", at))
                        .debug_selector(move || format!("KEEP-{at}"))
                        .text_size(px(11.))
                        .text_color(cyberpunk::Accent::Cyan.border())
                        .child("keep it")
                        .on_click(cx.listener(move |list, _, _, cx| {
                            list.keep_in_the_project(label.clone(), cx)
                        })),
                )
            })
            // The two presses, on the row they would act on.
            .when(chosen, |row| {
                row.children(Self::hint(
                    "HINT-RUN",
                    &menu::Confirm,
                    &self.focus,
                    window,
                    cx,
                ))
                .children(Self::hint(
                    "HINT-DEBUG",
                    &menu::SecondaryConfirm,
                    &self.focus,
                    window,
                    cx,
                ))
            })
            .on_click(cx.listener(move |list, _, _, cx| list.point_at(at, cx)))
            .into_any_element()
    }

    fn render_action(
        &self,
        selector: &'static str,
        icon: IconName,
        label: &'static str,
        action: Box<dyn gpui::Action>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        h_flex()
            .id(selector)
            .debug_selector(move || selector.to_string())
            .h(px(Self::ROW_HEIGHT))
            .w_full()
            .px_2()
            .gap_2()
            .items_center()
            .hover(|row| row.bg(cyberpunk::row_hovered()))
            .child(Icon::new(icon).size(IconSize::XSmall).color(Color::Muted))
            .child(
                div()
                    .flex_1()
                    .text_size(px(13.))
                    .text_color(cyberpunk::text_secondary())
                    .child(label),
            )
            .children(Self::hint(
                "HINT-ACTION",
                action.as_ref(),
                &self.focus,
                window,
                cx,
            ))
            .on_click(cx.listener(move |_, _, window, cx| {
                window.dispatch_action(action.boxed_clone(), cx);
                cx.emit(gpui::DismissEvent);
            }))
            .into_any_element()
    }
}

impl Render for ConfigurationsList {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut rows: Vec<AnyElement> = Vec::new();
        let mut said: Option<&'static str> = None;
        for at in 0..self.rows.len() {
            let heading = self.rows[at].kind.heading();
            if said != Some(heading) {
                said = Some(heading);
                rows.push(
                    div()
                        .px_2()
                        .pt_2()
                        .pb_1()
                        .text_size(px(11.))
                        .text_color(cyberpunk::text_tertiary())
                        .child(heading)
                        .into_any_element(),
                );
            }
            rows.push(self.render_row(at, window, cx));
        }
        let nothing_yet = self.rows.is_empty();

        v_flex()
            .key_context("RunConfigurationsList")
            .track_focus(&self.focus)
            .debug_selector(|| "run-configurations-list".to_string())
            .w(px(Self::WIDTH))
            .cyberpunk_surface()
            .on_action(cx.listener(|list, _: &menu::SelectNext, _, cx| {
                list.move_to(list.highlighted.saturating_add(1), cx)
            }))
            .on_action(cx.listener(|list, _: &menu::SelectPrevious, _, cx| {
                list.move_to(list.highlighted.saturating_sub(1), cx)
            }))
            .on_action(cx.listener(|list, _: &menu::SelectFirst, _, cx| list.move_to(0, cx)))
            .on_action(
                cx.listener(|list, _: &menu::SelectLast, _, cx| list.move_to(usize::MAX, cx)),
            )
            .on_action(cx.listener(|list, _: &menu::Confirm, window, cx| {
                list.choose(list.highlighted, false, window, cx)
            }))
            .on_action(cx.listener(|list, _: &menu::SecondaryConfirm, window, cx| {
                list.choose(list.highlighted, true, window, cx)
            }))
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(gpui::DismissEvent)))
            .child(
                v_flex()
                    .id("configurations-rows")
                    .track_scroll(&self.scroll)
                    .max_h(px(Self::ROW_HEIGHT * Self::MOST_ROWS_SHOWN))
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(rows)
                    .when(nothing_yet, |list| {
                        list.child(
                            div()
                                .px_2()
                                .py_2()
                                .text_size(px(12.))
                                .text_color(cyberpunk::text_tertiary())
                                .child("This project has not been told how to run yet."),
                        )
                    })
                    .vertical_scrollbar_for(&self.scroll, window, cx),
            )
            // The rule keeps the list and the two ways out of it apart, so a
            // glance does not confuse one for the other.
            .child(
                div()
                    .w_full()
                    .h(px(1.))
                    .flex_none()
                    .bg(cyberpunk::border_dim()),
            )
            .child(self.render_action(
                "NEW-CONFIGURATION",
                IconName::Plus,
                "New configuration…",
                Box::new(zed_actions::run_configurations::CreateFromEntryPoint),
                window,
                cx,
            ))
            .child(self.render_action(
                "ALL-CONFIGURATIONS",
                IconName::Settings,
                "All configurations…",
                Box::new(zed_actions::run_configurations::OpenRunConfigurations),
                window,
                cx,
            ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};
    use project::{FakeFs, Project};
    use serde_json::json;
    use util::path;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            // A test app has no keymap of its own, and a list whose rows show
            // which key runs them has to be tested with those keys bound -- the
            // hints are read out of the keymap, and an empty one shows none.
            // The keys themselves are the same ones the shipped keymap carries.
            cx.bind_keys([
                gpui::KeyBinding::new("down", menu::SelectNext, None),
                gpui::KeyBinding::new("up", menu::SelectPrevious, None),
                gpui::KeyBinding::new("enter", menu::Confirm, None),
                gpui::KeyBinding::new(
                    "shift-enter",
                    menu::SecondaryConfirm,
                    Some("RunConfigurationsList"),
                ),
            ]);
        });
    }

    async fn a_toolbar(
        cx: &mut TestAppContext,
    ) -> (
        Entity<ConfigurationsStore>,
        Entity<ConfigurationsToolbar>,
        VisualTestContext,
    ) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".zed": {
                    "tasks.json": r#"[
                      { "label": "api server", "command": "go run ./cmd/api" },
                      { "label": "unit tests", "command": "go test ./..." }
                    ]"#,
                },
            }),
        )
        .await;
        let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
        let window = cx.add_window(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        let workspace = window.root(&mut cx).unwrap();
        let toolbar = workspace.update_in(&mut cx, |workspace, _window, cx| {
            cx.new(|cx| ConfigurationsToolbar::new(workspace, cx))
        });
        cx.run_until_parked();
        let store = toolbar.read_with(&cx, |toolbar, _| toolbar.store.clone());
        (store, toolbar, cx)
    }

    /// The switcher opens on something to run, and says which of the two kinds it
    /// is: one the project keeps, or one run on the spot.
    #[gpui::test]
    async fn it_points_at_the_first_configuration_the_project_keeps(cx: &mut TestAppContext) {
        let (store, toolbar, mut cx) = a_toolbar(cx).await;

        assert_eq!(
            toolbar.read_with(&cx, |toolbar, cx| toolbar.what_it_points_at(cx)),
            Some((SharedString::from("api server"), true)),
            "the first one the file holds, and it is written down"
        );
        assert_eq!(
            toolbar.read_with(&cx, |toolbar, cx| toolbar
                .listed(cx)
                .into_iter()
                .map(|(name, _)| name.to_string())
                .collect::<Vec<_>>()),
            vec!["api server".to_string(), "unit tests".to_string()],
        );

        // A way run on the spot joins the list, after the kept ones.
        store.update_in(&mut cx, |store, _window, cx| {
            store.remember_temporary(
                TaskTemplate {
                    label: "go run ./cmd/one-off".to_string(),
                    command: "go".to_string(),
                    ..TaskTemplate::default()
                },
                cx,
            );
        });
        cx.run_until_parked();
        let listed = toolbar.read_with(&cx, |toolbar, cx| toolbar.listed(cx));
        assert_eq!(
            listed
                .iter()
                .map(|(name, _)| name.to_string())
                .collect::<Vec<_>>(),
            vec![
                "api server".to_string(),
                "unit tests".to_string(),
                "go run ./cmd/one-off".to_string(),
            ],
            "the ones run on the spot come after the ones the project keeps"
        );

        // Pointing at it says, in the switcher itself, that it is not written down.
        let (_, pointing) = listed.last().cloned().expect("the temporary one");
        toolbar.update_in(&mut cx, |toolbar, _window, cx| {
            toolbar.point_at(pointing, cx)
        });
        assert_eq!(
            toolbar.read_with(&cx, |toolbar, cx| toolbar.what_it_points_at(cx)),
            Some((SharedString::from("go run ./cmd/one-off"), false))
        );
    }

    /// The window the plaque lives in during a test: the plaque is a strip in a
    /// title bar, so it is given a bar to sit in rather than a whole screen.
    struct BarWithThePlaque {
        toolbar: Entity<ConfigurationsToolbar>,
    }

    impl Render for BarWithThePlaque {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .w(px(900.))
                .h(px(600.))
                .child(div().h(px(40.)).child(self.toolbar.clone()))
        }
    }

    /// A bar with the plaque in it, over a project holding `tasks`.
    async fn a_bar_with_the_plaque(
        tasks: &str,
        cx: &mut TestAppContext,
    ) -> (
        Entity<ConfigurationsToolbar>,
        gpui::WindowHandle<BarWithThePlaque>,
        VisualTestContext,
    ) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({ ".zed": { "tasks.json": tasks } }),
        )
        .await;
        let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
        let workspace_window =
            cx.add_window(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let mut workspace_cx = VisualTestContext::from_window(workspace_window.into(), cx);
        let workspace = workspace_window.root(&mut workspace_cx).unwrap();
        let toolbar = workspace.update_in(&mut workspace_cx, |workspace, _window, cx| {
            cx.new(|cx| ConfigurationsToolbar::new(workspace, cx))
        });
        let bar = cx.add_window(|_window, _cx| BarWithThePlaque {
            toolbar: toolbar.clone(),
        });
        let cx = VisualTestContext::from_window(bar.into(), cx);
        cx.run_until_parked();
        (toolbar, bar, cx)
    }

    fn draw_the_bar(window: gpui::WindowHandle<BarWithThePlaque>, cx: &mut VisualTestContext) {
        let _ = window;
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
    }

    const THREE_TASKS: &str = r#"[
      { "label": "api server", "command": "go run ./cmd/api" },
      { "label": "unit tests", "command": "go test ./..." },
      { "label": "integration tests", "command": "go test -tags=integration ./..." }
    ]"#;

    /// The list is the panel the mockup draws: as wide as a name needs, a
    /// heading over the rows, and the two presses shown on the row they would
    /// act on. Measured on what is painted, since a list that is laid out
    /// off-screen or at no width is a list nobody can use.
    #[gpui::test]
    async fn the_list_opens_as_the_mockup_has_it(cx: &mut TestAppContext) {
        let (_toolbar, bar, mut cx) = a_bar_with_the_plaque(THREE_TASKS, cx).await;

        let plaque = cx
            .debug_bounds("run-configurations-plaque")
            .expect("the plaque is painted in the bar");
        assert!(
            plaque.size.width >= px(200.),
            "the plaque has room for a name: {:?}",
            plaque.size
        );

        cx.simulate_click(plaque.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        draw_the_bar(bar, &mut cx);

        let list = cx
            .debug_bounds("run-configurations-list")
            .expect("clicking the plaque opens the list");
        assert!(
            (list.size.width - px(ConfigurationsList::WIDTH)).abs() < px(2.),
            "the list is as wide as the mockup has it: {:?}",
            list.size
        );
        assert!(
            list.size.width > plaque.size.width,
            "and wider than the plaque, so a truncated name is readable in it: \
             {:?} against {:?}",
            list.size.width,
            plaque.size.width
        );

        let first = cx
            .debug_bounds("CONFIGURATION-0")
            .expect("the project's first configuration is listed");
        assert!(
            (first.size.height - px(ConfigurationsList::ROW_HEIGHT)).abs() < px(2.),
            "a row is the height the mockup gives it: {:?}",
            first.size
        );
        assert!(
            first.origin.y >= list.origin.y && first.bottom() <= list.bottom(),
            "the rows are inside the panel: {first:?} against {list:?}"
        );

        // The two presses, on the row that would run.
        let run_hint = cx
            .debug_bounds("HINT-RUN")
            .expect("the chosen row says which key runs it");
        let debug_hint = cx
            .debug_bounds("HINT-DEBUG")
            .expect("and which key debugs it");
        assert!(
            (run_hint.center().y - first.center().y).abs() < px(4.),
            "the hints belong on the chosen row: {run_hint:?} against {first:?}"
        );
        assert!(
            debug_hint.origin.x > run_hint.origin.x,
            "run first, then debug, as the mockup has them"
        );
        assert!(
            debug_hint.right() <= first.right() + px(1.),
            "and both inside the row: {debug_hint:?} against {first:?}"
        );
        // A row that is not chosen carries no hints -- one row, one pair.
        assert!(
            cx.debug_bounds("CONFIGURATION-1").is_some(),
            "the second configuration is listed too"
        );

        // The two ways out of the list, under the rule that separates them.
        let new_one = cx
            .debug_bounds("NEW-CONFIGURATION")
            .expect("the list offers writing a new configuration");
        let all = cx
            .debug_bounds("ALL-CONFIGURATIONS")
            .expect("and opening the window with all of them");
        assert!(
            new_one.origin.y > first.bottom(),
            "the actions come after the rows: {new_one:?} against {first:?}"
        );
        assert!(
            all.origin.y >= new_one.bottom() - px(1.),
            "and in the order the mockup has them: {all:?} against {new_one:?}"
        );
    }

    /// Arrow keys move down the list and Enter runs what they land on -- through
    /// the real keymap, since the hints the rows show are only honest if the keys
    /// they name actually do it.
    #[gpui::test]
    async fn the_arrow_keys_and_enter_choose_a_row(cx: &mut TestAppContext) {
        let (toolbar, bar, mut cx) = a_bar_with_the_plaque(THREE_TASKS, cx).await;
        let plaque = cx
            .debug_bounds("run-configurations-plaque")
            .expect("the plaque is painted");
        cx.simulate_click(plaque.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        draw_the_bar(bar, &mut cx);

        let first = toolbar.read_with(&cx, |toolbar, cx| toolbar.what_it_points_at(cx));
        cx.simulate_keystrokes("down");
        cx.run_until_parked();
        draw_the_bar(bar, &mut cx);
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();

        let second = toolbar.read_with(&cx, |toolbar, cx| toolbar.what_it_points_at(cx));
        assert_ne!(
            first, second,
            "Enter on the row below had to point the plaque at it"
        );
        draw_the_bar(bar, &mut cx);
        assert!(
            cx.debug_bounds("run-configurations-list").is_none(),
            "and choosing closes the list"
        );
    }

    /// A project with more configurations than the panel is tall scrolls them
    /// rather than growing past the screen, and the two actions stay put below
    /// the rule where they can always be reached.
    #[gpui::test]
    async fn a_long_list_scrolls_rather_than_growing(cx: &mut TestAppContext) {
        let many: Vec<String> = (0..30)
            .map(|at| format!(r#"{{ "label": "task {at}", "command": "true" }}"#))
            .collect();
        let tasks = format!("[{}]", many.join(","));
        let (_toolbar, bar, mut cx) = a_bar_with_the_plaque(&tasks, cx).await;
        let plaque = cx
            .debug_bounds("run-configurations-plaque")
            .expect("the plaque is painted");
        cx.simulate_click(plaque.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        draw_the_bar(bar, &mut cx);

        let list = cx
            .debug_bounds("run-configurations-list")
            .expect("the list opened");
        let most_rows = px(ConfigurationsList::ROW_HEIGHT * ConfigurationsList::MOST_ROWS_SHOWN);
        assert!(
            list.size.height < most_rows + px(120.),
            "thirty configurations must not make a panel taller than the screen: {:?}",
            list.size
        );
        let all = cx
            .debug_bounds("ALL-CONFIGURATIONS")
            .expect("the way to the window with all of them is still reachable");
        assert!(
            all.bottom() <= list.bottom() + px(1.),
            "and still inside the panel: {all:?} against {list:?}"
        );
    }

    /// Walking down a long list has to keep the row under the cursor in view.
    /// The headings sit among the rows in the same scroll container, so the row's
    /// own number is not its number among that container's children -- getting
    /// that wrong scrolls to a heading, and further off with every heading
    /// passed.
    #[gpui::test]
    async fn walking_down_a_long_list_keeps_the_row_in_view(cx: &mut TestAppContext) {
        let many: Vec<String> = (0..30)
            .map(|at| format!(r#"{{ "label": "task {at}", "command": "true" }}"#))
            .collect();
        let tasks = format!("[{}]", many.join(","));
        let (_toolbar, bar, mut cx) = a_bar_with_the_plaque(&tasks, cx).await;
        let plaque = cx
            .debug_bounds("run-configurations-plaque")
            .expect("the plaque is painted");
        cx.simulate_click(plaque.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        draw_the_bar(bar, &mut cx);

        let list = cx
            .debug_bounds("run-configurations-list")
            .expect("the list opened");
        for step in 1..20 {
            cx.simulate_keystrokes("down");
            cx.run_until_parked();
            draw_the_bar(bar, &mut cx);
            let row = cx
                .debug_bounds(match step {
                    1 => "CONFIGURATION-1",
                    2 => "CONFIGURATION-2",
                    3 => "CONFIGURATION-3",
                    4 => "CONFIGURATION-4",
                    5 => "CONFIGURATION-5",
                    6 => "CONFIGURATION-6",
                    7 => "CONFIGURATION-7",
                    8 => "CONFIGURATION-8",
                    9 => "CONFIGURATION-9",
                    10 => "CONFIGURATION-10",
                    11 => "CONFIGURATION-11",
                    12 => "CONFIGURATION-12",
                    13 => "CONFIGURATION-13",
                    14 => "CONFIGURATION-14",
                    15 => "CONFIGURATION-15",
                    16 => "CONFIGURATION-16",
                    17 => "CONFIGURATION-17",
                    18 => "CONFIGURATION-18",
                    _ => "CONFIGURATION-19",
                })
                .unwrap_or_else(|| panic!("row {step} is painted once walked onto"));
            assert!(
                row.origin.y >= list.origin.y - px(1.) && row.bottom() <= list.bottom() + px(1.),
                "row {step} was walked onto but is outside the panel: {row:?} against {list:?}"
            );
        }
    }

    /// Keeping a temporary keeps the one the reader clicked. The list was made
    /// before they clicked, and what is remembered shifts in the meantime, so a
    /// row that remembers only where it sat would keep the wrong one.
    #[gpui::test]
    async fn keeping_a_temporary_keeps_the_one_that_was_clicked(cx: &mut TestAppContext) {
        let (toolbar, bar, mut cx) = a_bar_with_the_plaque(THREE_TASKS, cx).await;
        let store = toolbar.read_with(&cx, |toolbar, _| toolbar.store.clone());
        store.update(&mut cx, |store, cx| {
            store.remember_temporary(
                TaskTemplate {
                    label: "ran on the spot".to_string(),
                    command: "true".to_string(),
                    ..Default::default()
                },
                cx,
            );
        });
        cx.run_until_parked();

        let plaque = cx
            .debug_bounds("run-configurations-plaque")
            .expect("the plaque is painted");
        cx.simulate_click(plaque.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        draw_the_bar(bar, &mut cx);

        // The temporary is the last row, after the three the project keeps.
        let keep = cx
            .debug_bounds("KEEP-3")
            .expect("a temporary row offers keeping it");

        // Something else is run in the meantime, so what was the first
        // temporary is now the second.
        store.update(&mut cx, |store, cx| {
            store.remember_temporary(
                TaskTemplate {
                    label: "ran later".to_string(),
                    command: "true".to_string(),
                    ..Default::default()
                },
                cx,
            );
        });
        cx.run_until_parked();

        cx.simulate_click(keep.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        let kept: Vec<String> = store.read_with(&cx, |store, _| {
            store
                .of_kind(Kind::Task)
                .configurations
                .iter()
                .map(|configuration| configuration.shown_label())
                .collect()
        });
        assert!(
            kept.iter().any(|label| label == "ran on the spot"),
            "the one that was clicked had to be the one kept, not whatever took \
             its place: {kept:?}"
        );
    }

    /// A project that has not been told how to run it yet still gets the plaque:
    /// it is the way in to writing the first configuration. What it must not
    /// offer is a run button with nothing behind it.
    #[gpui::test]
    async fn with_nothing_to_run_it_offers_the_way_in(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({ "src": { "main.rs": "" } }))
            .await;
        let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
        let window = cx.add_window(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        let workspace = window.root(&mut cx).unwrap();
        let toolbar = workspace.update_in(&mut cx, |workspace, _window, cx| {
            cx.new(|cx| ConfigurationsToolbar::new(workspace, cx))
        });
        cx.run_until_parked();

        assert_eq!(
            toolbar.read_with(&cx, |toolbar, cx| toolbar.what_it_points_at(cx)),
            None
        );

        cx.draw(
            gpui::Point::default(),
            gpui::size(px(900.), px(40.)),
            |_window, _cx| gpui::div().w_full().h_full().child(toolbar.clone()),
        );
        assert!(
            cx.debug_bounds("run-configurations-toolbar").is_some(),
            "the plaque has to be there for a project with nothing to run yet"
        );
        assert!(
            cx.debug_bounds("ICON-PlayFilled").is_none(),
            "a run button with nothing behind it must not be offered"
        );
    }
}
