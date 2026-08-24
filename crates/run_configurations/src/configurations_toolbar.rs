use gpui::{App, Context, Entity, Render, SharedString, Subscription, WeakEntity, Window};
use task::TaskTemplate;
use ui::{ContextMenu, PopoverMenu, Tooltip, prelude::*};
use workspace::Workspace;

use crate::configurations_file::Kind;
use crate::configurations_store::ConfigurationsStore;

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
        let listed = self.listed(cx);
        let temporaries = self.store.read(cx).temporary().len();
        let toolbar = cx.entity();

        h_flex()
            .id("run-configurations-toolbar")
            .debug_selector(|| "run-configurations-toolbar".to_string())
            .gap_1()
            .items_center()
            .child(
                PopoverMenu::new("run-configurations-switcher")
                    .trigger_with_tooltip(
                        Button::new(
                            "run-configurations-chosen",
                            match &pointing_at {
                                Some((name, _)) => name.clone(),
                                None => SharedString::from("Nothing to run yet"),
                            },
                        )
                        .label_size(LabelSize::Small)
                        .style(ButtonStyle::Subtle)
                        .color(match &pointing_at {
                            Some((_, true)) => Color::Default,
                            Some((_, false)) => Color::Accent,
                            None => Color::Muted,
                        })
                        .end_icon(Icon::new(IconName::ChevronDown).size(IconSize::XSmall)),
                        Tooltip::text(match &pointing_at {
                            Some(_) => "What will run",
                            None => "Say how this project is run",
                        }),
                    )
                    .menu({
                        move |window, cx| {
                            let toolbar = toolbar.clone();
                            let listed = listed.clone();
                            Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                                let mut said_kept = false;
                                let mut said_temporary = false;
                                for (name, pointing) in listed {
                                    match &pointing {
                                        Pointing::Kept { .. } if !said_kept => {
                                            said_kept = true;
                                            menu = menu.header("Kept in the project");
                                        }
                                        Pointing::Remembered { .. } if !said_temporary => {
                                            said_temporary = true;
                                            menu = menu.separator().header("Run on the spot");
                                        }
                                        _ => {}
                                    }
                                    let toolbar = toolbar.clone();
                                    menu = menu.entry(name, None, move |_window, cx| {
                                        toolbar.update(cx, |toolbar, cx| {
                                            toolbar.point_at(pointing.clone(), cx)
                                        });
                                    });
                                }
                                if temporaries > 0 {
                                    menu = menu.separator().entry(
                                        "Keep the newest one in the project",
                                        None,
                                        move |_window, cx| {
                                            toolbar.update(cx, |toolbar, cx| toolbar.pin(0, cx));
                                        },
                                    );
                                }
                                menu.separator()
                                    .action(
                                        "New configuration...",
                                        Box::new(
                                            zed_actions::run_configurations::CreateFromEntryPoint,
                                        ),
                                    )
                                    .action(
                                        "All configurations...",
                                        Box::new(
                                            zed_actions::run_configurations::OpenRunConfigurations,
                                        ),
                                    )
                            }))
                        }
                    }),
            )
            .when(pointing_at.is_some(), |plaque| {
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
            .into_any_element()
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
