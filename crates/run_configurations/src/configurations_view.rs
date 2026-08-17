use editor::Editor;
use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, ScrollHandle,
    SharedString, Subscription, WeakEntity, Window, actions,
};
use project::Project;
use serde_json::Value;
use task::{DebugScenario, TaskTemplate};
use ui::{Tooltip, WithScrollbar, prelude::*};
use workspace::{Item, Workspace, item::ItemEvent};

use crate::OpenRunConfigurations;
use crate::configurations_file::{self, Configuration, Kind};
use crate::configurations_store::{ConfigurationsChanged, ConfigurationsStore};

actions!(
    run_configurations,
    [
        /// Runs the configuration being shown.
        RunThisConfiguration,
        /// Debugs the configuration being shown.
        DebugThisConfiguration,
        /// Writes the configuration being shown back into its file.
        SaveThisConfiguration
    ]
);

/// The debuggers the editor ships with, and the commands each one belongs to.
///
/// Matched against the command's first word only. Anything looser is wrong: a
/// command containing "go " is not a Go command -- "cargo test" contains it.
const DEBUGGERS: [(&str, &[&str]); 4] = [
    ("Delve", &["go", "gotestsum", "dlv"]),
    (
        "Debugpy",
        &["python", "python3", "pytest", "uv", "poetry", "pipenv"],
    ),
    (
        "JavaScript",
        &[
            "node", "npm", "pnpm", "yarn", "bun", "deno", "tsx", "ts-node", "vitest", "jest",
        ],
    ),
    (
        "CodeLLDB",
        &[
            "cargo", "rustc", "gcc", "g++", "clang", "clang++", "make", "mise",
        ],
    ),
];

/// The debugger a command belongs to, as a first guess for a new configuration.
/// Whatever the reader saves into the file is what counts afterwards.
fn debugger_for(command: &str) -> &'static str {
    let first_word = command
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .to_lowercase();
    DEBUGGERS
        .iter()
        .find(|(_, commands)| commands.contains(&first_word.as_str()))
        .map(|(debugger, _)| *debugger)
        // Anything built into a binary and run directly: the native debugger is
        // the one that can attach to it.
        .unwrap_or("CodeLLDB")
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &OpenRunConfigurations, window, cx| {
            let existing = workspace
                .active_pane()
                .read(cx)
                .items()
                .find_map(|item| item.downcast::<RunConfigurationsView>());
            match existing {
                Some(existing) => {
                    workspace.activate_item(&existing, true, true, window, cx);
                }
                None => {
                    let view = cx.new(|cx| {
                        RunConfigurationsView::new(
                            workspace.project().clone(),
                            workspace.weak_handle(),
                            window,
                            cx,
                        )
                    });
                    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
                }
            }
        });
    })
    .detach();
}

/// The project's run configurations: what the two files hold, in a form that can
/// be clicked together instead of typed -- and written straight back into them.
pub struct RunConfigurationsView {
    store: Entity<ConfigurationsStore>,
    workspace: WeakEntity<Workspace>,
    focus: FocusHandle,
    /// Which configuration is being shown, by the file it is in and where in it.
    chosen: Option<(Kind, usize)>,
    /// Set for a configuration that is not in a file yet.
    unsaved: bool,
    /// Set once the reader has typed something the file has not been told about.
    edited: bool,
    /// Said when the file changed under an edit, rather than quietly throwing the
    /// edit away.
    changed_underneath: bool,
    label: Entity<Editor>,
    command: Entity<Editor>,
    args: Entity<Editor>,
    cwd: Entity<Editor>,
    env_file: Entity<Editor>,
    env: Entity<Editor>,
    adapter: Entity<Editor>,
    adapter_config: Entity<Editor>,
    builds: Entity<Editor>,
    trouble: Option<SharedString>,
    list_scroll: ScrollHandle,
    form_scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<()> for RunConfigurationsView {}

impl RunConfigurationsView {
    pub fn new(
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let store = cx.new(|cx| ConfigurationsStore::new(&project, cx));
        let mut subscriptions = vec![cx.subscribe_in(
            &store,
            window,
            |view: &mut Self, _, _: &ConfigurationsChanged, window, cx| {
                view.the_files_changed(window, cx)
            },
        )];

        let field = |placeholder: &'static str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_placeholder_text(placeholder, window, cx);
                editor
            })
        };
        let lines = |placeholder: &'static str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| {
                let mut editor = Editor::multi_line(window, cx);
                editor.set_placeholder_text(placeholder, window, cx);
                editor
            })
        };

        let label = field("What to call it", window, cx);
        let command = field("The command to run", window, cx);
        let args = lines("One argument a line", window, cx);
        let cwd = field("Where to run it, blank for the project root", window, cx);
        let env_file = field("A file of variables, such as .env.local", window, cx);
        let env = lines("NAME=value, one a line", window, cx);
        let adapter = field(
            "Which debugger: Delve, CodeLLDB, Debugpy, JavaScript, GDB",
            window,
            cx,
        );
        let adapter_config = lines("What the debugger needs, as JSON", window, cx);
        let builds = field("A task to build first, by its name", window, cx);

        for editor in [
            &label,
            &command,
            &args,
            &cwd,
            &env_file,
            &env,
            &adapter,
            &adapter_config,
            &builds,
        ] {
            subscriptions.push(cx.subscribe(editor, |view: &mut Self, _, event, cx| {
                if matches!(
                    event,
                    editor::EditorEvent::Edited { .. } | editor::EditorEvent::BufferEdited { .. }
                ) {
                    view.edited = true;
                    cx.notify();
                }
            }));
        }

        Self {
            store,
            workspace,
            focus: cx.focus_handle(),
            chosen: None,
            unsaved: false,
            edited: false,
            changed_underneath: false,
            label,
            command,
            args,
            cwd,
            env_file,
            env,
            adapter,
            adapter_config,
            builds,
            trouble: None,
            list_scroll: ScrollHandle::new(),
            form_scroll: ScrollHandle::new(),
            _subscriptions: subscriptions,
        }
    }

    /// The files were read again. What is being shown follows them, unless the
    /// reader has typed something that is not in them yet -- then they are told
    /// rather than having their work taken away.
    fn the_files_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.edited {
            self.changed_underneath = true;
            cx.notify();
            return;
        }
        if let Some((kind, at)) = self.chosen {
            let still_there = self
                .store
                .read(cx)
                .get(kind, at)
                .map(|configuration| configuration.clone());
            match still_there {
                Some(configuration) => self.show(&configuration, window, cx),
                None => self.chosen = None,
            }
        }
        cx.notify();
    }

    /// Puts a configuration into the form.
    fn show(&mut self, configuration: &Configuration, window: &mut Window, cx: &mut Context<Self>) {
        self.chosen = Some((configuration.kind, configuration.at));
        self.unsaved = false;
        self.edited = false;
        self.changed_underneath = false;
        self.trouble = None;

        let (label, command, args, cwd, env_file, env) = match &configuration.task {
            Some(task) => (
                task.label.clone(),
                task.command.clone(),
                task.args.join("\n"),
                task.cwd.clone().unwrap_or_default(),
                task.env_file.clone().unwrap_or_default(),
                task.env
                    .iter()
                    .map(|(name, value)| format!("{name}={value}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            None => (
                configuration.label.clone(),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
            ),
        };
        let (adapter, adapter_config, builds) = match &configuration.scenario {
            Some(scenario) => (
                scenario.adapter.to_string(),
                match scenario.config.is_null() {
                    true => String::new(),
                    false => serde_json::to_string_pretty(&scenario.config).unwrap_or_default(),
                },
                match &scenario.build {
                    Some(task::BuildTaskDefinition::ByName(name)) => name.to_string(),
                    Some(task::BuildTaskDefinition::Template { task_template, .. }) => {
                        task_template.label.clone()
                    }
                    None => String::new(),
                },
            ),
            None => (String::new(), String::new(), String::new()),
        };
        let label = match configuration.kind {
            Kind::Debug => configuration.label.clone(),
            Kind::Task => label,
        };

        // A configuration the editor cannot read is shown as it was written, so
        // the reader can see what is in the file and mend it.
        if configuration.task.is_none() && configuration.scenario.is_none() {
            self.trouble = Some(
                "The editor cannot read this one. It is shown as it was written; \
                 open the file to mend it."
                    .into(),
            );
        }

        for (editor, text) in [
            (&self.label, label),
            (&self.command, command),
            (&self.args, args),
            (&self.cwd, cwd),
            (&self.env_file, env_file),
            (&self.env, env),
            (&self.adapter, adapter),
            (&self.adapter_config, adapter_config),
            (&self.builds, builds),
        ] {
            editor.update(cx, |editor, cx| {
                editor.set_text(text, window, cx);
            });
        }
        self.edited = false;
        cx.notify();
    }

    fn start_a_new_one(&mut self, kind: Kind, window: &mut Window, cx: &mut Context<Self>) {
        let blank = Configuration {
            kind,
            at: self.store.read(cx).of_kind(kind).configurations.len(),
            label: String::new(),
            task: match kind {
                Kind::Task => Some(TaskTemplate::default()),
                Kind::Debug => None,
            },
            scenario: match kind {
                Kind::Debug => Some(DebugScenario {
                    adapter: "".into(),
                    label: "".into(),
                    build: None,
                    config: Value::Null,
                    tcp_connection: None,
                }),
                Kind::Task => None,
            },
            as_written: Value::Null,
        };
        self.show(&blank, window, cx);
        self.unsaved = true;
        self.trouble = None;
        cx.notify();
    }

    /// What the form says, as a task.
    fn task_in_the_form(&self, cx: &App) -> TaskTemplate {
        let text = |editor: &Entity<Editor>| editor.read(cx).text(cx).trim().to_string();
        let mut task = TaskTemplate {
            label: text(&self.label),
            command: text(&self.command),
            args: text(&self.args)
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect(),
            ..TaskTemplate::default()
        };
        let cwd = text(&self.cwd);
        task.cwd = (!cwd.is_empty()).then_some(cwd);
        let env_file = text(&self.env_file);
        task.env_file = (!env_file.is_empty()).then_some(env_file);
        for (name, value) in task::env_file_variables(&text(&self.env)) {
            task.env.insert(name, value);
        }
        // What the file said and the form does not show is kept: a configuration
        // may hold settings this form has no field for, and saving must not be a
        // way to lose them.
        if let Some((Kind::Task, at)) = self.chosen
            && let Some(existing) = self
                .store
                .read(cx)
                .get(Kind::Task, at)
                .and_then(|configuration| configuration.task.clone())
        {
            task.use_new_terminal = existing.use_new_terminal;
            task.allow_concurrent_runs = existing.allow_concurrent_runs;
            task.reveal = existing.reveal;
            task.reveal_target = existing.reveal_target;
            task.hide = existing.hide;
            task.tags = existing.tags;
            task.shell = existing.shell;
            task.show_summary = existing.show_summary;
            task.show_command = existing.show_command;
            task.save = existing.save;
            task.hooks = existing.hooks;
        }
        task
    }

    /// What the form says, as a debug configuration.
    fn scenario_in_the_form(&self, cx: &App) -> DebugScenario {
        let text = |editor: &Entity<Editor>| editor.read(cx).text(cx).trim().to_string();
        let builds = text(&self.builds);
        let config = match text(&self.adapter_config).is_empty() {
            true => Value::Null,
            false => serde_json::from_str(&text(&self.adapter_config)).unwrap_or(Value::Null),
        };
        DebugScenario {
            adapter: text(&self.adapter).into(),
            label: text(&self.label).into(),
            build: (!builds.is_empty()).then(|| task::BuildTaskDefinition::ByName(builds.into())),
            config,
            tcp_connection: self
                .chosen
                .and_then(|(kind, at)| self.store.read(cx).get(kind, at))
                .and_then(|configuration| configuration.scenario.as_ref())
                .and_then(|scenario| scenario.tcp_connection.clone()),
        }
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let Some((kind, at)) = self.chosen else {
            return;
        };
        let entry = match kind {
            Kind::Task => {
                let task = self.task_in_the_form(cx);
                if task.label.trim().is_empty() || task.command.trim().is_empty() {
                    self.trouble =
                        Some("A task needs a name and a command before it can be saved.".into());
                    cx.notify();
                    return;
                }
                configurations_file::task_as_written(&task)
            }
            Kind::Debug => {
                let scenario = self.scenario_in_the_form(cx);
                if scenario.label.trim().is_empty() || scenario.adapter.trim().is_empty() {
                    self.trouble = Some(
                        "A debug configuration needs a name and a debugger before it can be \
                         saved."
                            .into(),
                    );
                    cx.notify();
                    return;
                }
                configurations_file::scenario_as_written(&scenario)
            }
        };
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                self.trouble = Some(format!("{error:#}").into());
                cx.notify();
                return;
            }
        };
        let where_to = match self.unsaved {
            true => None,
            false => Some(at),
        };
        let writing = self.store.read(cx).save(kind, where_to, entry, cx);
        self.edited = false;
        self.changed_underneath = false;
        self.unsaved = false;
        self.trouble = None;
        cx.spawn(async move |view, cx| {
            if let Err(error) = writing.await {
                view.update(cx, |view, cx| {
                    view.trouble = Some(format!("{error:#}").into());
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
        cx.notify();
    }

    fn remove(&mut self, cx: &mut Context<Self>) {
        let Some((kind, at)) = self.chosen else {
            return;
        };
        if self.unsaved {
            self.chosen = None;
            self.unsaved = false;
            cx.notify();
            return;
        }
        let removing = self.store.read(cx).remove(kind, at, cx);
        self.chosen = None;
        cx.spawn(async move |view, cx| {
            if let Err(error) = removing.await {
                view.update(cx, |view, cx| {
                    view.trouble = Some(format!("{error:#}").into());
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
        cx.notify();
    }

    fn duplicate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((kind, _)) = self.chosen else {
            return;
        };
        self.label.update(cx, |editor, cx| {
            let copy = format!("{} (copy)", editor.text(cx));
            editor.set_text(copy, window, cx);
        });
        let at = self.store.read(cx).of_kind(kind).configurations.len();
        self.chosen = Some((kind, at));
        self.unsaved = true;
        self.edited = true;
        cx.notify();
    }

    /// Runs what is being shown. A task is run as itself; a debug configuration is
    /// started under its debugger.
    fn run(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((kind, _)) = self.chosen else {
            return;
        };
        match kind {
            Kind::Task => {
                let task = self.task_in_the_form(cx);
                if task.label.trim().is_empty() || task.command.trim().is_empty() {
                    self.trouble = Some("There is nothing to run yet.".into());
                    cx.notify();
                    return;
                }
                self.run_the_task(task, window, cx);
            }
            Kind::Debug => self.debug(window, cx),
        }
    }

    fn run_the_task(&mut self, task: TaskTemplate, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let contexts = workspace.update(cx, |workspace, cx| {
            tasks_ui::task_contexts(workspace, window, cx)
        });
        let workspace = workspace.downgrade();
        cx.spawn_in(window, async move |_, cx| {
            let contexts = contexts.await;
            let context = contexts.active_context().cloned().unwrap_or_default();
            workspace
                .update_in(cx, |workspace, window, cx| {
                    let comes_from = match contexts.worktree() {
                        Some(id) => project::TaskSourceKind::Worktree {
                            id,
                            directory_in_worktree: path::rel_path::RelPath::from_unix_str(".zed")
                                .expect("a relative path of our own")
                                .into_arc(),
                            id_base: "run configurations".into(),
                        },
                        None => project::TaskSourceKind::UserInput,
                    };
                    workspace.schedule_task(comes_from, &task, &context, false, window, cx);
                })
                .ok();
        })
        .detach();
    }

    /// Starts a debug session. For a task, the debug configuration that builds it
    /// is looked for; if the project has none, one is offered for saving rather
    /// than being conjured silently.
    fn debug(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((kind, at)) = self.chosen else {
            return;
        };
        match kind {
            Kind::Debug => {
                let scenario = self.scenario_in_the_form(cx);
                if scenario.adapter.trim().is_empty() {
                    self.trouble = Some("This configuration names no debugger yet.".into());
                    cx.notify();
                    return;
                }
                self.start_debugging(scenario, window, cx);
            }
            Kind::Task => {
                let task = self.task_in_the_form(cx);
                let existing = self
                    .store
                    .read(cx)
                    .of_kind(Kind::Debug)
                    .configurations
                    .iter()
                    .find_map(|configuration| {
                        let scenario = configuration.scenario.as_ref()?;
                        match &scenario.build {
                            Some(task::BuildTaskDefinition::ByName(name))
                                if name.as_ref() == task.label =>
                            {
                                Some(scenario.clone())
                            }
                            _ => None,
                        }
                    });
                match existing {
                    Some(scenario) => self.start_debugging(scenario, window, cx),
                    None => {
                        let _ = at;
                        self.offer_a_debug_configuration(&task, window, cx);
                    }
                }
            }
        }
    }

    fn start_debugging(
        &mut self,
        scenario: DebugScenario,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let contexts = workspace.update(cx, |workspace, cx| {
            tasks_ui::task_contexts(workspace, window, cx)
        });
        let workspace = workspace.downgrade();
        cx.spawn_in(window, async move |_, cx| {
            let contexts = contexts.await;
            let worktree = contexts.worktree();
            let context = contexts.active_context().cloned().unwrap_or_default();
            workspace
                .update_in(cx, |workspace, window, cx| {
                    workspace.start_debug_session(
                        scenario,
                        context.into(),
                        None,
                        worktree,
                        window,
                        cx,
                    );
                })
                .ok();
        })
        .detach();
    }

    /// Fills the form with a debug configuration for `task`, for the reader to
    /// look at and save. The debugger is guessed from the command; the file, once
    /// saved, is what decides.
    fn offer_a_debug_configuration(
        &mut self,
        task: &TaskTemplate,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let debugger = debugger_for(&task.command);
        let scenario = DebugScenario {
            adapter: debugger.into(),
            label: format!("debug {}", task.label).into(),
            build: Some(task::BuildTaskDefinition::ByName(task.label.clone().into())),
            config: Value::Null,
            tcp_connection: None,
        };
        let blank = Configuration {
            kind: Kind::Debug,
            at: self
                .store
                .read(cx)
                .of_kind(Kind::Debug)
                .configurations
                .len(),
            label: scenario.label.to_string(),
            task: None,
            scenario: Some(scenario),
            as_written: Value::Null,
        };
        self.show(&blank, window, cx);
        self.unsaved = true;
        self.edited = true;
        self.trouble = Some(
            "This project has no debug configuration for that task. Here is one -- \
             look it over and save it, and the button will use it from then on."
                .into(),
        );
        cx.notify();
    }

    fn open_the_file(&mut self, kind: Kind, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.store.read(cx).file_path(kind) else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let fs = workspace.read(cx).app_state().fs.clone();
        let empty = kind.empty_file().to_string();
        cx.spawn_in(window, async move |_, cx| {
            // Made if it is not there yet: the reader asked to see it, and an
            // editor showing nothing at all is worse than an empty list.
            if !fs.is_file(&path).await {
                configurations_file::write(&fs, &path, &empty).await.ok();
            }
            workspace
                .update_in(cx, |workspace, window, cx| {
                    workspace.open_abs_path(
                        path,
                        workspace::OpenOptions {
                            visible: Some(workspace::OpenVisible::None),
                            ..Default::default()
                        },
                        window,
                        cx,
                    )
                })
                .ok()?
                .await
                .ok();
            Some(())
        })
        .detach();
    }

    fn render_list(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let store = self.store.read(cx);
        let rows: Vec<AnyElement> = store
            .all()
            .map(|configuration| {
                let kind = configuration.kind;
                let at = configuration.at;
                let chosen = self.chosen == Some((kind, at));
                let what_it_runs = configuration
                    .task
                    .as_ref()
                    .map(|task| task.command.clone())
                    .or_else(|| {
                        configuration
                            .scenario
                            .as_ref()
                            .map(|scenario| scenario.adapter.to_string())
                    })
                    .unwrap_or_default();
                let shown_label = configuration.shown_label();
                let configuration = configuration.clone();
                h_flex()
                    .id((
                        "configuration",
                        at + matches!(kind, Kind::Debug) as usize * 10_000,
                    ))
                    .w_full()
                    .px_2()
                    .py_1()
                    .gap_2()
                    .items_center()
                    .when(chosen, |row| row.bg(ui::cyberpunk::row_chosen()))
                    .hover(|row| row.bg(ui::cyberpunk::row_hovered()))
                    .cursor_pointer()
                    .on_click(cx.listener(move |view, _, window, cx| {
                        let configuration = configuration.clone();
                        view.show(&configuration, window, cx);
                    }))
                    .child(
                        Label::new(match kind {
                            Kind::Task => "run",
                            Kind::Debug => "debug",
                        })
                        .size(LabelSize::XSmall)
                        .color(match kind {
                            Kind::Task => Color::Accent,
                            Kind::Debug => Color::Warning,
                        }),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(Label::new(shown_label).size(LabelSize::Small))
                            .child(
                                Label::new(what_it_runs)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    )
                    .child(
                        IconButton::new(
                            ("run", at + matches!(kind, Kind::Debug) as usize * 10_000),
                            IconName::PlayFilled,
                        )
                        .icon_size(IconSize::XSmall)
                        .tooltip(Tooltip::text("Run this"))
                        .on_click(cx.listener(
                            move |view, _, window, cx| {
                                if let Some(configuration) =
                                    view.store.read(cx).get(kind, at).cloned()
                                {
                                    view.show(&configuration, window, cx);
                                }
                                view.run(window, cx);
                            },
                        )),
                    )
                    .child(
                        IconButton::new(
                            ("debug", at + matches!(kind, Kind::Debug) as usize * 10_000),
                            IconName::Debug,
                        )
                        .icon_size(IconSize::XSmall)
                        .tooltip(Tooltip::text("Debug this"))
                        .on_click(cx.listener(
                            move |view, _, window, cx| {
                                if let Some(configuration) =
                                    view.store.read(cx).get(kind, at).cloned()
                                {
                                    view.show(&configuration, window, cx);
                                }
                                view.debug(window, cx);
                            },
                        )),
                    )
                    .into_any_element()
            })
            .collect();

        let nothing_yet = rows.is_empty();
        v_flex()
            .id("configurations-list")
            .flex_none()
            .w(px(320.))
            .h_full()
            .border_r_1()
            .border_color(ui::cyberpunk::border_dim())
            .overflow_y_scroll()
            .track_scroll(&self.list_scroll)
            .when(nothing_yet, |list| {
                list.child(
                    div().p_3().child(
                        Label::new(
                            "This project has no run configurations yet. Add one, or write \
                             .zed/tasks.json by hand -- both end up in the same file.",
                        )
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    ),
                )
            })
            .children(rows)
            .custom_scrollbars(
                ui::Scrollbars::always_visible(ui::ScrollAxes::Vertical)
                    .tracked_scroll_handle(&self.list_scroll),
                window,
                cx,
            )
            .into_any_element()
    }

    fn render_form(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some((kind, _)) = self.chosen else {
            return v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .child(
                    Label::new("Pick a configuration, or add one.")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element();
        };

        let field = |name: &'static str, editor: &Entity<Editor>, tall: bool| {
            v_flex()
                .w_full()
                .gap_1()
                .child(Label::new(name).size(LabelSize::XSmall).color(Color::Muted))
                .child(
                    div()
                        .w_full()
                        .when(!tall, |field| field.h(px(28.)))
                        .when(tall, |field| field.h(px(84.)))
                        .px_2()
                        .py_1()
                        .border_1()
                        .border_color(ui::cyberpunk::border_dim())
                        .child(editor.clone()),
                )
        };

        v_flex()
            .id("configuration-form")
            .flex_1()
            .min_w_0()
            .h_full()
            .p_3()
            .gap_3()
            .overflow_y_scroll()
            .track_scroll(&self.form_scroll)
            .children(self.changed_underneath.then(|| {
                Label::new(
                    "The file changed on disk while you were typing. Save to write what is \
                     here, or pick the configuration again to see what the file says.",
                )
                .size(LabelSize::Small)
                .color(Color::Warning)
            }))
            .children(self.trouble.clone().map(|trouble| {
                Label::new(trouble)
                    .size(LabelSize::Small)
                    .color(Color::Error)
            }))
            .child(field("Name", &self.label, false))
            .when(kind == Kind::Task, |form| {
                form.child(field("Command", &self.command, false))
                    .child(field("Arguments", &self.args, true))
                    .child(field("Working directory", &self.cwd, false))
                    .child(field("Environment file", &self.env_file, false))
                    .child(field("Environment", &self.env, true))
            })
            .when(kind == Kind::Debug, |form| {
                form.child(field("Debugger", &self.adapter, false))
                    .child(field("Builds first (a task's name)", &self.builds, false))
                    .child(field("What the debugger needs", &self.adapter_config, true))
            })
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(
                        Button::new("configuration-run", "Run")
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|view, _, window, cx| view.run(window, cx))),
                    )
                    .child(
                        Button::new("configuration-debug", "Debug")
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|view, _, window, cx| view.debug(window, cx))),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("configuration-save", "Save to the file")
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|view, _, _, cx| view.save(cx))),
                    )
                    .child(
                        Button::new("configuration-duplicate", "Duplicate")
                            .label_size(LabelSize::Small)
                            .on_click(
                                cx.listener(|view, _, window, cx| view.duplicate(window, cx)),
                            ),
                    )
                    .child(
                        Button::new("configuration-remove", "Remove")
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|view, _, _, cx| view.remove(cx))),
                    ),
            )
            .child(self.render_where_it_lives(window, cx))
            .into_any_element()
    }

    /// Says which file this comes from, and offers to open it: clicking a form
    /// together and writing the file are the same thing, and the reader should be
    /// able to switch at any moment.
    fn render_where_it_lives(&self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let store = self.store.read(cx);
        let shown = |kind: Kind| {
            store
                .file_path(kind)
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "no folder is open".to_string())
        };
        v_flex()
            .w_full()
            .gap_1()
            .pt_2()
            .border_t_1()
            .border_color(ui::cyberpunk::border_dim())
            .child(
                Label::new("Everything here is what these files say:")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .children([Kind::Task, Kind::Debug].map(|kind| {
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_center()
                    .child(
                        Label::new(shown(kind))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .buffer_font(cx),
                    )
                    .child(
                        Button::new(("open-file", kind as usize), "Open")
                            .label_size(LabelSize::XSmall)
                            .on_click(cx.listener(move |view, _, window, cx| {
                                view.open_the_file(kind, window, cx)
                            })),
                    )
            }))
            .into_any_element()
    }
}

impl Focusable for RunConfigurationsView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for RunConfigurationsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let trouble_in_files: Vec<AnyElement> = [Kind::Task, Kind::Debug]
            .into_iter()
            .filter_map(|kind| {
                let trouble = self.store.read(cx).of_kind(kind).trouble.clone()?;
                Some(
                    Label::new(format!("{}: {trouble}", kind.file_name()))
                        .size(LabelSize::Small)
                        .color(Color::Error)
                        .into_any_element(),
                )
            })
            .collect();

        v_flex()
            .key_context("RunConfigurations")
            .track_focus(&self.focus)
            .on_action(
                cx.listener(|view, _: &RunThisConfiguration, window, cx| view.run(window, cx)),
            )
            .on_action(
                cx.listener(|view, _: &DebugThisConfiguration, window, cx| view.debug(window, cx)),
            )
            .on_action(cx.listener(|view, _: &SaveThisConfiguration, _, cx| view.save(cx)))
            .size_full()
            .bg(ui::cyberpunk::surface())
            .child(
                h_flex()
                    .flex_none()
                    .w_full()
                    .px_2()
                    .py_1()
                    .gap_2()
                    .items_center()
                    .border_b_1()
                    .border_color(ui::cyberpunk::border_dim())
                    .child(
                        Button::new("configuration-new-task", "Add a task")
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.start_a_new_one(Kind::Task, window, cx)
                            })),
                    )
                    .child(
                        Button::new("configuration-new-debug", "Add a debug configuration")
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.start_a_new_one(Kind::Debug, window, cx)
                            })),
                    )
                    .child(div().flex_1())
                    .children(trouble_in_files),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(self.render_list(window, cx))
                    .child(self.render_form(window, cx)),
            )
    }
}

impl Item for RunConfigurationsView {
    type Event = ();

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Run configurations".into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::PlayFilled))
    }

    fn to_item_events(_event: &Self::Event, _f: &mut dyn FnMut(ItemEvent)) {}

    fn show_toolbar(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use fs::Fs as _;
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

    /// A view over a project whose `.zed` holds `tasks`.
    async fn a_view_of(
        tasks: Option<&str>,
        cx: &mut TestAppContext,
    ) -> (
        Entity<RunConfigurationsView>,
        std::sync::Arc<FakeFs>,
        VisualTestContext,
    ) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            match tasks {
                Some(tasks) => json!({ ".zed": { "tasks.json": tasks } }),
                None => json!({ "src": { "main.rs": "" } }),
            },
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let view = workspace.update_in(cx, |workspace, window, cx| {
            let view = cx.new(|cx| {
                RunConfigurationsView::new(
                    workspace.project().clone(),
                    workspace.weak_handle(),
                    window,
                    cx,
                )
            });
            workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx);
            view
        });
        cx.run_until_parked();
        (view, fs, cx.clone())
    }

    #[gpui::test]
    async fn what_the_file_holds_is_what_the_view_shows(cx: &mut TestAppContext) {
        let (view, _fs, mut cx) = a_view_of(
            Some(
                r#"[
                  { "label": "api server", "command": "go run ./cmd/api" },
                  { "label": "unit tests", "command": "go test ./..." }
                ]"#,
            ),
            cx,
        )
        .await;

        let shown: Vec<String> = view.read_with(&mut cx, |view, cx| {
            view.store
                .read(cx)
                .all()
                .map(|configuration| configuration.shown_label())
                .collect()
        });

        assert_eq!(shown, vec!["api server", "unit tests"]);
    }

    /// The promise of this whole surface: the file is the truth, so a file edited
    /// by hand while the view is open has to reach the view by itself.
    #[gpui::test]
    async fn a_file_written_by_hand_reaches_the_view(cx: &mut TestAppContext) {
        let (view, fs, mut cx) =
            a_view_of(Some(r#"[{ "label": "one", "command": "true" }]"#), cx).await;
        let at_first: Vec<String> = view.read_with(&mut cx, |view, cx| {
            view.store
                .read(cx)
                .all()
                .map(|configuration| configuration.shown_label())
                .collect()
        });
        assert_eq!(at_first, vec!["one"]);

        fs.save(
            path!("/project/.zed/tasks.json").as_ref(),
            &r#"[
                 { "label": "one", "command": "true" },
                 { "label": "two", "command": "false" }
               ]"#
            .into(),
            Default::default(),
        )
        .await
        .expect("the file can be written");
        cx.run_until_parked();

        let now: Vec<String> = view.read_with(&mut cx, |view, cx| {
            view.store
                .read(cx)
                .all()
                .map(|configuration| configuration.shown_label())
                .collect()
        });
        assert_eq!(
            now,
            vec!["one", "two"],
            "the view has to follow the file without being asked to"
        );
    }

    #[gpui::test]
    async fn what_is_clicked_together_ends_up_in_the_file(cx: &mut TestAppContext) {
        let (view, fs, mut cx) = a_view_of(None, cx).await;

        view.update_in(&mut cx, |view, window, cx| {
            view.start_a_new_one(Kind::Task, window, cx);
            view.label
                .update(cx, |editor, cx| editor.set_text("api server", window, cx));
            view.command.update(cx, |editor, cx| {
                editor.set_text("go run ./cmd/api", window, cx)
            });
            view.env_file
                .update(cx, |editor, cx| editor.set_text(".env.local", window, cx));
            view.env
                .update(cx, |editor, cx| editor.set_text("PORT=8080", window, cx));
            view.save(cx);
        });
        cx.run_until_parked();

        let written = fs
            .load(path!("/project/.zed/tasks.json").as_ref())
            .await
            .expect("the file was written");
        let read_back = crate::configurations_file::read(Kind::Task, &written);
        let task = read_back.configurations[0]
            .task
            .as_ref()
            .expect("what was written is a task the editor can read back");

        assert_eq!(task.label, "api server");
        assert_eq!(task.command, "go run ./cmd/api");
        assert_eq!(
            task.env_file.as_deref(),
            Some(".env.local"),
            "the file of variables is part of the configuration, not copied into it"
        );
        assert_eq!(task.env.get("PORT").map(String::as_str), Some("8080"));

        let showing: Vec<String> = view.read_with(&mut cx, |view, cx| {
            view.store
                .read(cx)
                .all()
                .map(|configuration| configuration.shown_label())
                .collect()
        });
        assert_eq!(
            showing,
            vec!["api server"],
            "and the view shows it because the file does, not because it remembers \
             having saved it"
        );
    }

    #[gpui::test]
    async fn an_edit_is_not_taken_away_when_the_file_changes(cx: &mut TestAppContext) {
        let (view, fs, mut cx) =
            a_view_of(Some(r#"[{ "label": "one", "command": "true" }]"#), cx).await;
        view.update_in(&mut cx, |view, window, cx| {
            let first = view
                .store
                .read(cx)
                .get(Kind::Task, 0)
                .cloned()
                .expect("the first configuration");
            view.show(&first, window, cx);
            view.command
                .update(cx, |editor, cx| editor.set_text("half typed", window, cx));
        });
        cx.run_until_parked();

        fs.save(
            path!("/project/.zed/tasks.json").as_ref(),
            &r#"[{ "label": "one", "command": "changed on disk" }]"#.into(),
            Default::default(),
        )
        .await
        .expect("the file can be written");
        cx.run_until_parked();

        let (still_typed, told) = view.read_with(&mut cx, |view, cx| {
            (view.command.read(cx).text(cx), view.changed_underneath)
        });
        assert_eq!(
            still_typed, "half typed",
            "what the reader typed must survive the file changing under it"
        );
        assert!(told, "and the reader has to be told that it changed");
    }

    #[test]
    fn a_task_command_suggests_the_debugger_that_fits_it() {
        for (command, expected) in [
            ("go run ./cmd/api", "Delve"),
            ("gotestsum ./...", "Delve"),
            // "cargo" holds "go " inside it, which is how a looser guess got this
            // one wrong.
            ("cargo test -p thing", "CodeLLDB"),
            ("python -m pytest", "Debugpy"),
            ("uv run pytest -k thing", "Debugpy"),
            ("npm run dev", "JavaScript"),
            ("/usr/local/bin/node server.js", "JavaScript"),
            ("./bin/thing --serve", "CodeLLDB"),
            ("", "CodeLLDB"),
        ] {
            assert_eq!(
                super::debugger_for(command),
                expected,
                "a first guess for {command:?} that the reader then changes"
            );
        }
    }
}
