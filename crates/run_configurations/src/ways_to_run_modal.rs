use gpui::{
    AnyElement, App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render,
    SharedString, WeakEntity, Window, px,
};
use serde_json::Value;
use task::{BuildTaskDefinition, DebugScenario, TaskTemplate};
use ui::{ElevationIndex, KeyBinding, prelude::*};
use workspace::{ModalView, Workspace};
use zed_actions::run_configurations::EntryPointOffer;

use crate::configurations_file::Kind;
use crate::configurations_store::ConfigurationsStore;

/// One way of running what the reader asked about.
#[derive(Clone)]
pub enum Way {
    /// A configuration the project keeps in one of its files.
    Kept {
        label: SharedString,
        detail: SharedString,
        kind: Kind,
        at: usize,
    },
    /// What the editor itself would run for this line. It is not in any file yet,
    /// which is what makes it worth offering to keep.
    OnTheSpot {
        label: SharedString,
        detail: SharedString,
        task: TaskTemplate,
    },
    /// A way that was run on the spot before and is still remembered. It is not in
    /// any file either, so it carries the way to pin it into one -- named rather
    /// than numbered, since the list of them shifts as things are run and pinned.
    Remembered {
        label: SharedString,
        detail: SharedString,
        task: TaskTemplate,
    },
}

impl Way {
    fn label(&self) -> SharedString {
        match self {
            Way::Kept { label, .. }
            | Way::OnTheSpot { label, .. }
            | Way::Remembered { label, .. } => label.clone(),
        }
    }

    fn detail(&self) -> SharedString {
        match self {
            Way::Kept { detail, .. }
            | Way::OnTheSpot { detail, .. }
            | Way::Remembered { detail, .. } => detail.clone(),
        }
    }

    /// Whether this way is written down anywhere. The ones that are not are the
    /// ones worth offering to keep.
    fn is_in_a_file(&self) -> bool {
        matches!(self, Way::Kept { .. })
    }
}

/// Whether debugging is offered beside running one way in the list.
enum DebugOffer {
    /// Not backed by a task -- a way already backed by a debug scenario only
    /// ever debugs, and there is nothing to derive one from for it.
    NotATask,
    /// A debugger can be worked out from the command.
    Ready,
    /// It cannot, with the sentence saying why -- shown on the row rather than
    /// hidden, per the mockup's rule that this limitation stays visible.
    Withheld(SharedString),
}

/// The window the gutter's run button opens: every way of running this project,
/// the one the editor found for this very line first, and the way to write a new
/// one or go and edit them all.
pub struct WaysToRunModal {
    focus: FocusHandle,
    store: Entity<ConfigurationsStore>,
    workspace: WeakEntity<Workspace>,
    offer: EntryPointOffer,
    ways: Vec<Way>,
    chosen: usize,
    trouble: Option<SharedString>,
}

impl EventEmitter<DismissEvent> for WaysToRunModal {}
impl ModalView for WaysToRunModal {}

impl Focusable for WaysToRunModal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl WaysToRunModal {
    /// Every way of running that this project knows, the line's own first.
    pub fn ways_of(
        offer: &EntryPointOffer,
        store: &Entity<ConfigurationsStore>,
        cx: &App,
    ) -> Vec<Way> {
        let mut ways = Vec::new();
        let on_the_spot = crate::templates::task_from(offer);
        if !on_the_spot.command.trim().is_empty() {
            let task = on_the_spot;
            ways.push(Way::OnTheSpot {
                label: SharedString::from(task.label.clone()),
                detail: SharedString::from(match offer.file.as_deref() {
                    Some(file) => format!(
                        "on the spot -- {} line {}",
                        file.file_name()
                            .map(|name| name.to_string_lossy().to_string())
                            .unwrap_or_else(|| file.display().to_string()),
                        offer.line
                    ),
                    None => "on the spot".to_string(),
                }),
                task,
            });
        }
        let store = store.read(cx);
        for task in store.temporary() {
            let already_offered = ways.iter().any(|way| match way {
                Way::OnTheSpot { task: theirs, .. } => {
                    theirs.label == task.label && theirs.command == task.command
                }
                _ => false,
            });
            if already_offered {
                continue;
            }
            ways.push(Way::Remembered {
                label: SharedString::from(task.label.clone()),
                detail: SharedString::from(
                    format!("{} {}", task.command, task.args.join(" "))
                        .trim()
                        .to_string(),
                ),
                task: task.clone(),
            });
        }
        for kind in [Kind::Task, Kind::Debug] {
            for (at, configuration) in store.of_kind(kind).configurations.iter().enumerate() {
                let detail = match (&configuration.task, &configuration.scenario) {
                    (Some(task), _) => format!("{} {}", task.command, task.args.join(" ")),
                    (_, Some(scenario)) => format!("debug with {}", scenario.adapter),
                    _ => "as the file has it".to_string(),
                };
                ways.push(Way::Kept {
                    label: SharedString::from(configuration.shown_label()),
                    detail: SharedString::from(detail.trim().to_string()),
                    kind,
                    at,
                });
            }
        }
        ways
    }

    pub fn new(
        offer: EntryPointOffer,
        store: Entity<ConfigurationsStore>,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        let ways = Self::ways_of(&offer, &store, cx);
        Self {
            focus: cx.focus_handle(),
            store,
            workspace,
            offer,
            ways,
            chosen: 0,
            trouble: None,
        }
    }

    /// What the window lists, and which of them is not in a file yet. For tests and
    /// for the switcher, which shows the same set.
    pub fn shown_ways(&self) -> Vec<(String, bool)> {
        self.ways
            .iter()
            .map(|way| (way.label().to_string(), !way.is_in_a_file()))
            .collect()
    }

    fn move_by(&mut self, by: isize, cx: &mut Context<Self>) {
        if self.ways.is_empty() {
            return;
        }
        let last = self.ways.len() - 1;
        self.chosen = match by {
            by if by < 0 => match self.chosen {
                0 => last,
                at => at - 1,
            },
            _ => match self.chosen == last {
                true => 0,
                false => self.chosen + 1,
            },
        };
        cx.notify();
    }

    /// Whether debugging is offered beside running this way. Only a way backed
    /// by a task can offer it at all -- one already backed by a debug scenario
    /// only ever debugs, and there is nothing to derive a debugger from for it.
    fn debug_offer(&self, way: &Way, cx: &App) -> DebugOffer {
        let command = match way {
            Way::OnTheSpot { task, .. } | Way::Remembered { task, .. } => task.command.clone(),
            Way::Kept { kind, at, .. } => {
                let task = self
                    .store
                    .read(cx)
                    .get(*kind, *at)
                    .and_then(|configuration| configuration.task.clone());
                match task {
                    Some(task) => task.command,
                    None => return DebugOffer::NotATask,
                }
            }
        };
        match crate::debugging::why_it_cannot_be_debugged(&command) {
            None => DebugOffer::Ready,
            Some(reason) => DebugOffer::Withheld(reason.into()),
        }
    }

    fn run(&mut self, at: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.act(at, false, window, cx);
    }

    /// The second press: debugs the way at `at` instead of running it. A way
    /// already backed by a debug scenario has nothing extra for this to add --
    /// Enter already starts it.
    fn debug(&mut self, at: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.act(at, true, window, cx);
    }

    /// Runs or debugs the way at `at`. Debugging one backed by a task builds a
    /// scenario for it on the fly, under the mockup's rule that Run and Debug
    /// are two buttons on one configuration rather than two kinds of it.
    fn act(&mut self, at: usize, debug: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(way) = self.ways.get(at).cloned() else {
            return;
        };
        match way {
            Way::OnTheSpot { task, .. } | Way::Remembered { task, .. } => {
                // Engaged without being written down: remembered for the next
                // time, and offered for pinning rather than asked about now.
                self.store.update(cx, |store, cx| {
                    store.remember_temporary(task.clone(), cx);
                });
                self.run_or_debug_a_task(task, debug, window, cx);
            }
            Way::Kept { kind, at, .. } => {
                let configuration = self.store.read(cx).get(kind, at).cloned();
                let Some(configuration) = configuration else {
                    self.trouble = Some("It is no longer in the file.".into());
                    cx.notify();
                    return;
                };
                match (configuration.task, configuration.scenario) {
                    (Some(task), _) => self.run_or_debug_a_task(task, debug, window, cx),
                    (_, Some(scenario)) => self.start_debugging(scenario, window, cx),
                    _ => {
                        self.trouble =
                            Some("The file holds something this editor cannot run.".into());
                        cx.notify();
                    }
                }
            }
        }
    }

    /// Runs a task as itself, or debugs it by building a scenario for it on the
    /// fly -- withheld with the reason why when the command is too opaque to
    /// derive a debugger from, rather than guessed at.
    fn run_or_debug_a_task(
        &mut self,
        task: TaskTemplate,
        debug: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !debug {
            self.run_a_task(task, window, cx);
            return;
        }
        match crate::debugging::why_it_cannot_be_debugged(&task.command) {
            None => self.start_debugging(derive_a_debug_scenario(&task), window, cx),
            Some(reason) => {
                self.trouble = Some(reason.into());
                cx.notify();
            }
        }
    }

    fn run_a_task(&mut self, task: TaskTemplate, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |modal, cx| {
            crate::configurations_view::run_a_task(&workspace, task, cx).await;
            modal.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
        })
        .detach();
    }

    fn start_debugging(
        &mut self,
        scenario: DebugScenario,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |modal, cx| {
            crate::configurations_view::start_a_debug_session(&workspace, scenario, cx).await;
            modal.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
        })
        .detach();
    }

    /// Writes a remembered way into the project's file, where everybody else reads
    /// it. The list is rebuilt, since what was temporary is now kept.
    fn pin(&mut self, at: usize, cx: &mut Context<Self>) {
        let Some(Way::Remembered { task, .. }) = self.ways.get(at).cloned() else {
            return;
        };
        // Looked up by what it is, not by where it was: the list shifts as things
        // are run and pinned, and pinning the wrong one writes somebody else's
        // command into the project's file.
        let Some(which) = self
            .store
            .read(cx)
            .temporary()
            .iter()
            .position(|kept| kept.label == task.label && kept.command == task.command)
        else {
            self.trouble = Some("That way is no longer remembered.".into());
            cx.notify();
            return;
        };
        let writing = self
            .store
            .update(cx, |store, cx| store.pin_temporary(which, cx));
        let store = self.store.clone();
        let offer = self.offer.clone();
        cx.spawn(async move |modal, cx| match writing.await {
            Ok(()) => {
                modal
                    .update(cx, |modal, cx| {
                        modal.ways = Self::ways_of(&offer, &store, cx);
                        modal.chosen = 0;
                        cx.notify();
                    })
                    .ok();
            }
            Err(error) => {
                modal
                    .update(cx, |modal, cx| {
                        modal.trouble = Some(format!("{error:#}").into());
                        cx.notify();
                    })
                    .ok();
            }
        })
        .detach();
    }

    /// Opens the window that writes a new configuration, filled in from whatever
    /// the editor found on the line.
    fn write_a_new_one(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let offer = self.offer.clone();
        let workspace = self.workspace.clone();
        cx.emit(DismissEvent);
        let Some(alive) = workspace.upgrade() else {
            return;
        };
        let project = alive.read(cx).project().clone();
        crate::configurations_view::open_window_for_a_new_one(project, workspace, Some(offer), cx);
    }

    fn open_them_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
        window.dispatch_action(
            Box::new(zed_actions::run_configurations::OpenRunConfigurations),
            cx,
        );
    }
}

impl WaysToRunModal {
    /// The key that runs or debugs the chosen way, if one is bound in the
    /// keymap. Mirrors `ConfigurationsList::hint` in `configurations_toolbar` --
    /// an unbound action gets no hint rather than an empty box where one would
    /// be.
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
}

impl Render for WaysToRunModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let chosen = self.chosen;
        let mut list = v_flex().w_full().gap_0p5();
        for (at, way) in self.ways.iter().enumerate() {
            let label = way.label();
            let detail = way.detail();
            let temporary = !way.is_in_a_file();
            let pinnable = matches!(way, Way::Remembered { .. });
            let debug_offer = self.debug_offer(way, cx);
            let chosen_here = at == chosen;
            let row = h_flex()
                .id(SharedString::from(format!("way-{at}")))
                .debug_selector(move || format!("way-{at}"))
                .w_full()
                .px_2()
                .py_1()
                .gap_2()
                .items_center()
                .justify_between()
                .cursor_pointer()
                .when(chosen_here, |row| {
                    row.bg(cx.theme().colors().element_selected)
                })
                .hover(|row| row.bg(cx.theme().colors().element_hover))
                .on_click(cx.listener(move |modal, _, window, cx| modal.run(at, window, cx)))
                .child(
                    v_flex()
                        .gap_0p5()
                        .child(Label::new(label).size(LabelSize::Small))
                        .child(
                            Label::new(detail)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                )
                .when(temporary, |row| {
                    row.child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                Label::new("on the spot")
                                    .size(LabelSize::XSmall)
                                    .color(Color::Accent),
                            )
                            .when(pinnable, |row| {
                                row.child(
                                    Button::new(("pin-way", at), "Keep")
                                        .style(ButtonStyle::Subtle)
                                        .tooltip(ui::Tooltip::text(
                                            "Write it into the project's tasks.json",
                                        ))
                                        .on_click(cx.listener(move |modal, _, _window, cx| {
                                            modal.pin(at, cx)
                                        })),
                                )
                            }),
                    )
                })
                // The two presses, on the way they would act on: Enter always
                // runs a task-backed way, and Shift-Enter debugs it too when a
                // debugger can be worked out from its command.
                .when(
                    chosen_here && !matches!(debug_offer, DebugOffer::NotATask),
                    |row| {
                        row.children(Self::hint(
                            "WAY-HINT-RUN",
                            &menu::Confirm,
                            &self.focus,
                            window,
                            cx,
                        ))
                        .when(
                            matches!(debug_offer, DebugOffer::Ready),
                            |row| {
                                row.children(Self::hint(
                                    "WAY-HINT-DEBUG",
                                    &menu::SecondaryConfirm,
                                    &self.focus,
                                    window,
                                    cx,
                                ))
                            },
                        )
                    },
                );
            list = list.child(
                v_flex()
                    .w_full()
                    .gap_0p5()
                    .child(row)
                    // The limitation is said outright on the row it belongs to,
                    // rather than a debug offer that is simply absent.
                    .when(chosen_here, |column| match &debug_offer {
                        DebugOffer::Withheld(reason) => column.child(
                            div()
                                .px_2()
                                .debug_selector(|| "WAY-HINT-DEBUG-REASON".to_string())
                                .child(
                                    Label::new(reason.clone())
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                ),
                        ),
                        _ => column,
                    }),
            );
        }

        v_flex()
            .key_context("WaysToRun")
            .track_focus(&self.focus)
            .debug_selector(|| "ways-to-run".to_string())
            .w(px(560.))
            .p_3()
            .gap_2()
            .elevation_3(cx)
            .shadow(ElevationIndex::ModalSurface.shadow(cx))
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .on_action(cx.listener(|modal, _: &menu::SelectNext, _, cx| modal.move_by(1, cx)))
            .on_action(cx.listener(|modal, _: &menu::SelectPrevious, _, cx| modal.move_by(-1, cx)))
            .on_action(cx.listener(|modal, _: &menu::Confirm, window, cx| {
                let at = modal.chosen;
                modal.run(at, window, cx)
            }))
            .on_action(
                cx.listener(|modal, _: &menu::SecondaryConfirm, window, cx| {
                    let at = modal.chosen;
                    modal.debug(at, window, cx)
                }),
            )
            .child(
                Label::new("How to run this")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(list)
            .children(self.trouble.clone().map(|trouble| {
                Label::new(trouble)
                    .size(LabelSize::XSmall)
                    .color(Color::Error)
            }))
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("ways-new", "New configuration...")
                            .style(ButtonStyle::Subtle)
                            .on_click(cx.listener(|modal, _, window, cx| {
                                modal.write_a_new_one(window, cx)
                            })),
                    )
                    .child(
                        Button::new("ways-all", "All configurations")
                            .style(ButtonStyle::Subtle)
                            .on_click(
                                cx.listener(|modal, _, window, cx| modal.open_them_all(window, cx)),
                            ),
                    ),
            )
    }
}

/// A debug scenario built from a task's own command, on the fly: `build`
/// refers to the task by name, so the locator that recognizes the command
/// works out what to hand the adapter from it -- the mechanism the mockup
/// names, already in the model. Only ever built once
/// `crate::debugging::can_be_derived_from` said yes to the command, so there
/// is always a locator for it to find.
fn derive_a_debug_scenario(task: &TaskTemplate) -> DebugScenario {
    DebugScenario {
        adapter: debugger_named_for(&task.command),
        label: format!("debug {}", task.label).into(),
        build: Some(BuildTaskDefinition::ByName(task.label.clone().into())),
        config: Value::Null,
        tcp_connection: None,
    }
}

/// The debugger a derived way's command implies. `configurations_view` keeps
/// the fuller version of this table for its own guess-and-let-the-reader-fix
/// form, but that function is private to its module -- this one is only ever
/// asked about a command `can_be_derived_from` already said yes to, so it only
/// has to know the same handful of programs.
fn debugger_named_for(command: &str) -> SharedString {
    let program = command.split_whitespace().next().unwrap_or_default();
    let name = program.rsplit('/').next().unwrap_or(program);
    if name.starts_with("python") {
        return "Debugpy".into();
    }
    match name {
        "go" => "Delve",
        "npm" | "pnpm" | "yarn" => "JavaScript",
        // "cargo", and a `$ZED_...` variable standing in for one of the above --
        // the native debugger is the reasonable default for either.
        _ => "CodeLLDB",
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RunFromEntryPoint;
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
            crate::init(cx);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            // A test app has no keymap of its own, and a row that shows which
            // key runs or debugs it has to be tested with those keys bound --
            // the hints are read out of the keymap, and an empty one shows
            // none. The keys themselves are the ones the shipped keymap carries.
            cx.bind_keys([
                gpui::KeyBinding::new("enter", menu::Confirm, None),
                gpui::KeyBinding::new("shift-enter", menu::SecondaryConfirm, Some("WaysToRun")),
            ]);
        });
    }

    /// The window the gutter's run button opens, over a project whose
    /// `.zed/tasks.json` holds exactly `tasks` and nothing else to choose
    /// between -- so the file's own task is always `ways[0]`.
    async fn a_ways_modal_for(
        tasks: &str,
        cx: &mut TestAppContext,
    ) -> (Entity<WaysToRunModal>, Entity<Project>, VisualTestContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({ ".zed": { "tasks.json": tasks } }),
        )
        .await;
        let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
        cx.run_until_parked();

        // A language nothing is known about, so the line's own way stays empty
        // and only the file's own task is left to offer.
        cx.update(|_, cx| {
            cx.set_global(EntryPointOffer {
                language: Some("Brainfuck".to_string()),
                file: Some(std::path::PathBuf::from(path!("/project/src/main.bf"))),
                line: 1,
                label: None,
                command: None,
                args: Vec::new(),
                cwd: None,
                env: Default::default(),
            });
        });
        cx.dispatch_action(RunFromEntryPoint);
        cx.run_until_parked();

        let modal = workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_modal::<WaysToRunModal>(cx)
            })
            .expect("the file's task is a way to run");
        (modal, project, cx.clone())
    }

    fn draw(cx: &mut VisualTestContext) {
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
    }

    #[test]
    fn a_derived_scenario_points_at_the_task_it_debugs() {
        let task = TaskTemplate {
            label: "unit tests".to_string(),
            command: "cargo test".to_string(),
            ..TaskTemplate::default()
        };
        let scenario = derive_a_debug_scenario(&task);
        assert_eq!(scenario.adapter, SharedString::from("CodeLLDB"));
        assert_eq!(
            scenario.build,
            Some(BuildTaskDefinition::ByName("unit tests".into())),
            "the scenario refers to the task by name, the mechanism the mockup names"
        );
    }

    #[test]
    fn the_debugger_matches_the_commands_can_be_derived_from_recognizes() {
        for (command, expected) in [
            ("cargo test", "CodeLLDB"),
            ("go test ./...", "Delve"),
            ("npm run dev", "JavaScript"),
            ("pnpm test", "JavaScript"),
            ("yarn build", "JavaScript"),
            ("python -m pytest", "Debugpy"),
            ("/usr/bin/python3 -m app", "Debugpy"),
        ] {
            assert_eq!(
                debugger_named_for(command),
                SharedString::from(expected),
                "for {command:?}"
            );
        }
    }

    /// A way whose command a locator recognizes offers both presses, in the
    /// same two-press language `ConfigurationsList` uses for its own rows.
    #[gpui::test]
    async fn a_way_a_locator_recognizes_offers_running_and_debugging(cx: &mut TestAppContext) {
        let (_modal, _project, mut cx) = a_ways_modal_for(
            r#"[{ "label": "unit tests", "command": "cargo test" }]"#,
            cx,
        )
        .await;
        draw(&mut cx);

        assert!(
            cx.debug_bounds("WAY-HINT-RUN").is_some(),
            "the chosen row says which key runs it"
        );
        assert!(
            cx.debug_bounds("WAY-HINT-DEBUG").is_some(),
            "a locator exists for cargo, so it also says which key debugs it"
        );
        assert!(
            cx.debug_bounds("WAY-HINT-DEBUG-REASON").is_none(),
            "nothing to explain when debugging is offered"
        );
    }

    /// A way whose command is opaque to every locator still offers running,
    /// and says outright why debugging is withheld rather than leaving the
    /// button simply missing.
    #[gpui::test]
    async fn a_way_an_opaque_command_offers_running_only_and_says_why(cx: &mut TestAppContext) {
        let (modal, _project, mut cx) =
            a_ways_modal_for(r#"[{ "label": "build", "command": "make build" }]"#, cx).await;
        draw(&mut cx);

        assert!(
            cx.debug_bounds("WAY-HINT-RUN").is_some(),
            "running is still offered"
        );
        assert!(
            cx.debug_bounds("WAY-HINT-DEBUG").is_none(),
            "no locator can say what a Makefile target builds"
        );
        assert!(
            cx.debug_bounds("WAY-HINT-DEBUG-REASON").is_some(),
            "the limitation is said outright rather than the button simply missing"
        );

        let reason = modal.read_with(&cx, |modal, cx| {
            match modal.debug_offer(&modal.ways[0], cx) {
                DebugOffer::Withheld(reason) => Some(reason.to_string()),
                _ => None,
            }
        });
        assert_eq!(
            reason.as_deref(),
            crate::debugging::why_it_cannot_be_debugged("make build"),
            "the row says exactly what the mockup requires: why, not just that it cannot"
        );
    }

    /// Shift-Enter on a way the locator recognizes must not also run it: the
    /// two presses are two different paths on the same task, not a fallback
    /// into one another.
    #[gpui::test]
    async fn choosing_debug_does_not_take_the_running_path(cx: &mut TestAppContext) {
        let (modal, project, mut cx) = a_ways_modal_for(
            r#"[{ "label": "unit tests", "command": "cargo test" }]"#,
            cx,
        )
        .await;

        let inventory = project
            .read_with(&cx, |project, cx| {
                project.task_store().read(cx).task_inventory().cloned()
            })
            .expect("a project with tasks.json has an inventory");
        assert!(
            inventory
                .read_with(&cx, |inventory, _| inventory.last_scheduled_task(None))
                .is_none(),
            "nothing has run yet"
        );

        cx.dispatch_action(menu::SecondaryConfirm);
        cx.run_until_parked();

        assert!(
            inventory
                .read_with(&cx, |inventory, _| inventory.last_scheduled_task(None))
                .is_none(),
            "debugging must not take the running path"
        );
        assert_eq!(
            modal.read_with(&cx, |modal, _| modal.trouble.clone()),
            None,
            "cargo test can be derived, so debugging it should not fail either"
        );

        // The other half of the pair is shown by the way an opaque command
        // answers instead: it says why rather than quietly doing nothing, which
        // is the same branch reporting rather than acting. Scheduling itself
        // cannot be observed here -- a test workspace has no terminal for a task
        // to land in, so the inventory stays empty either way.
        let (opaque, _project, mut cx) = a_ways_modal_for(
            r#"[{ "label": "build", "command": "make build" }]"#,
            &mut cx,
        )
        .await;
        cx.dispatch_action(menu::SecondaryConfirm);
        cx.run_until_parked();
        assert_eq!(
            opaque.read_with(&cx, |modal, _| modal.trouble.clone()),
            crate::debugging::why_it_cannot_be_debugged("make build").map(Into::into),
            "an opaque command has to say why it cannot be debugged"
        );
    }
}
