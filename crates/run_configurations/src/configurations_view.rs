use editor::Editor;
use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, MouseButton, Pixels,
    PromptLevel, ScrollHandle, SharedString, Size, Subscription, TitlebarOptions, WeakEntity,
    Window, WindowBounds, WindowOptions, actions, point,
};
use platform_title_bar::PlatformTitleBar;
use project::Project;
use project::TaskContexts;
use serde_json::Value;
use settings::Settings;
use task::{DebugScenario, TaskContext, TaskTemplate};
use ui::{ContextMenu, PopoverMenu, Tooltip, WithScrollbar, prelude::*};
use util::ResultExt as _;
use workspace::{
    Item, Toast, Workspace, client_side_decorations, item::ItemEvent, notifications::NotificationId,
};

use crate::configurations_file::{self, Configuration, Kind};

/// One variable of a run's environment, as the form holds it.
struct EnvRow {
    name: Entity<Editor>,
    value: Entity<Editor>,
}
use crate::configurations_store::{ConfigurationsChanged, ConfigurationsStore};
use crate::run_configurations_settings::RunConfigurationsSettings;
use crate::{CreateFromEntryPoint, OpenRunConfigurations, RunFromEntryPoint};

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
        &["cargo", "rustc", "gcc", "g++", "clang", "clang++"],
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
    cx.observe_new(|workspace: &mut Workspace, _window, cx| {
        // One store for the whole workspace, made when it opens: the files are read
        // once and are already warm by the time a window asks what the project
        // knows. A store made inside an action would still be empty when the window
        // it feeds is drawn.
        let store = crate::configurations_store::store_for(workspace.project(), cx);

        // The run button in the gutter. Every way of running the line is offered;
        // with no way at all there is nothing to choose between, so the window that
        // writes one opens instead. The offer itself was left in a global by the
        // editor, which cannot depend on this crate.
        workspace.register_action({
            move |workspace, _: &RunFromEntryPoint, window, cx| {
                let offer = taken_offer(cx);
                let handle = workspace.weak_handle();
                let store = store.clone();
                let ways = crate::ways_to_run_modal::WaysToRunModal::ways_of(&offer, &store, cx);
                match ways.is_empty() {
                    true => open_window_for_a_new_one(
                        workspace.project().clone(),
                        handle,
                        Some(offer),
                        cx,
                    ),
                    false => workspace.toggle_modal(window, cx, move |_window, cx| {
                        crate::ways_to_run_modal::WaysToRunModal::new(offer, store, handle, cx)
                    }),
                }
            }
        });
        workspace.register_action(|workspace, _: &CreateFromEntryPoint, _window, cx| {
            let offer = taken_offer(cx);
            open_window_for_a_new_one(
                workspace.project().clone(),
                workspace.weak_handle(),
                Some(offer),
                cx,
            );
        });
        workspace.register_action(|workspace, _: &OpenRunConfigurations, _window, cx| {
            open_window(workspace.project().clone(), workspace.weak_handle(), cx);
        });
    })
    .detach();
}

/// The name this window's remembered placement is stored under.
const REMEMBERED_AS: &str = "run-configurations";

/// Small enough to be pushed aside, large enough that the form still has a
/// column beside the list rather than one word a line.
const SMALLEST_SIZE: Size<Pixels> = Size {
    width: px(640.),
    height: px(400.),
};

/// Opens the configurations in a window of the reader's own: it is moved and
/// sized like any other, and the editor stays readable behind it.
pub fn open_window(project: Entity<Project>, workspace: WeakEntity<Workspace>, cx: &mut App) {
    // One window over one project's files. A second over the same project would
    // write the same file from two forms and the later save would quietly win,
    // so an open one is brought forward instead. A window belonging to another
    // project is somebody else's and is left where it is.
    let asked_for = workspace.entity_id();
    // Deferred to get the workspace off the stack: the action that led here is
    // still updating it, and opening a window reads it again. Everything else
    // waits until then as well, so two asks in one frame cannot both decide that
    // there is no window yet and open one each.
    cx.defer(move |cx| {
        if workspace.upgrade().is_none() {
            return;
        }
        if let Some(open_already) = window_over(asked_for, cx) {
            open_already
                .update(cx, |view, window, cx| {
                    window.activate_window();
                    view.focus.clone().focus(window, cx);
                })
                .log_err();
            return;
        }

        let bounds = where_to_open(cx);
        let app_id = release_channel::ReleaseChannel::global(cx).app_id();
        let decorations = match std::env::var("ZED_WINDOW_DECORATIONS") {
            Ok(asked) if asked == "server" => gpui::WindowDecorations::Server,
            Ok(asked) if asked == "client" => gpui::WindowDecorations::Client,
            _ => match workspace::WorkspaceSettings::get_global(cx).window_decorations {
                settings::WindowDecorations::Server => gpui::WindowDecorations::Server,
                settings::WindowDecorations::Client => gpui::WindowDecorations::Client,
            },
        };
        let options = WindowOptions {
            titlebar: Some(TitlebarOptions {
                title: Some("Zed — Run configurations".into()),
                appears_transparent: true,
                traffic_light_position: Some(point(px(12.), px(12.))),
            }),
            focus: true,
            show: true,
            is_movable: true,
            kind: gpui::WindowKind::Normal,
            window_background: cx.theme().window_background_appearance(),
            app_id: Some(app_id.to_owned()),
            window_decorations: Some(decorations),
            window_min_size: Some(SMALLEST_SIZE),
            window_bounds: Some(bounds),
            ..Default::default()
        };
        let opened = cx.open_window(options, |window, cx| {
            cx.new(|cx| RunConfigurationsView::new(project, workspace, window, cx))
        });
        if let Some(handle) = opened.log_err() {
            handle
                .update(cx, |view, window, cx| {
                    window.activate_window();
                    view.focus.clone().focus(window, cx);
                })
                .log_err();
        }
    });
}

/// Opens the window on a configuration that is not written down yet, filled in
/// from whatever the editor knew about the line the reader asked from -- or empty,
/// when they asked for a new one with nothing in mind.
pub fn open_window_for_a_new_one(
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    offer: Option<zed_actions::run_configurations::EntryPointOffer>,
    cx: &mut App,
) {
    let asked_for = workspace.entity_id();
    open_window(project, workspace, cx);
    // After the open, which is itself deferred: the window has to exist before
    // it can be told what to show.
    cx.defer(move |cx| {
        if let Some(form) = window_over(asked_for, cx) {
            form.update(cx, |view, window, cx| {
                view.start_from_an_offer(offer, window, cx)
            })
            .log_err();
        }
    });
}

/// The form already open over this workspace, if there is one. One window over
/// one project's files: a second over the same project would write the same file
/// from two forms and the later save would quietly win. A window belonging to
/// another project is somebody else's and is left where it is.
fn window_over(
    workspace: gpui::EntityId,
    cx: &App,
) -> Option<gpui::WindowHandle<RunConfigurationsView>> {
    cx.windows().into_iter().find_map(|window| {
        let handle = window.downcast::<RunConfigurationsView>()?;
        let same_project = handle
            .read(cx)
            .ok()
            .is_some_and(|view| view.workspace.entity_id() == workspace);
        same_project.then_some(handle)
    })
}

/// Where it was left, if that screen is still there and still holds it;
/// otherwise nearly the whole screen, so every field is in view without the
/// reader resizing anything.
fn where_to_open(cx: &mut App) -> WindowBounds {
    workspace::remembered_window::where_to_open(REMEMBERED_AS, cx)
}

/// How wide the band along each edge that the window can be pulled by is, and
/// how far into the window each corner's own reaches.
const A_BAND_TO_PULL: Pixels = px(5.);
const A_CORNER_TO_PULL: Pixels = px(14.);

/// The bands along the window's edges and corners that it is resized by.
///
/// The shell draws its own outside the window's visible border, in the few
/// transparent pixels the shadow occupies -- which is where nobody aims. A
/// reader aims at the edge they can see, and lands just inside it, on the
/// window's own content. These sit exactly there.
fn what_the_window_is_pulled_by() -> Vec<AnyElement> {
    use gpui::{CursorStyle, ResizeEdge};

    let band = |name: &'static str, edge: ResizeEdge, cursor: CursorStyle| {
        let grip = div()
            .absolute()
            .debug_selector(move || format!("run-configurations-pull-{name}"))
            .cursor(cursor)
            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                cx.stop_propagation();
                window.start_window_resize(edge);
            });
        match edge {
            ResizeEdge::Top => grip.top_0().left_0().right_0().h(A_BAND_TO_PULL),
            ResizeEdge::Bottom => grip.bottom_0().left_0().right_0().h(A_BAND_TO_PULL),
            ResizeEdge::Left => grip.left_0().top_0().bottom_0().w(A_BAND_TO_PULL),
            ResizeEdge::Right => grip.right_0().top_0().bottom_0().w(A_BAND_TO_PULL),
            // Corners last, and so painted over the edges they meet: a corner
            // pulls both ways at once, which is what the reader means there.
            ResizeEdge::TopLeft => grip
                .top_0()
                .left_0()
                .w(A_CORNER_TO_PULL)
                .h(A_CORNER_TO_PULL),
            ResizeEdge::TopRight => grip
                .top_0()
                .right_0()
                .w(A_CORNER_TO_PULL)
                .h(A_CORNER_TO_PULL),
            ResizeEdge::BottomLeft => grip
                .bottom_0()
                .left_0()
                .w(A_CORNER_TO_PULL)
                .h(A_CORNER_TO_PULL),
            ResizeEdge::BottomRight => grip
                .bottom_0()
                .right_0()
                .w(A_CORNER_TO_PULL)
                .h(A_CORNER_TO_PULL),
        }
        .into_any_element()
    };

    vec![
        band("top", ResizeEdge::Top, CursorStyle::ResizeUp),
        band("bottom", ResizeEdge::Bottom, CursorStyle::ResizeDown),
        band("left", ResizeEdge::Left, CursorStyle::ResizeLeft),
        band("right", ResizeEdge::Right, CursorStyle::ResizeRight),
        band(
            "top-left",
            ResizeEdge::TopLeft,
            CursorStyle::ResizeUpLeftDownRight,
        ),
        band(
            "top-right",
            ResizeEdge::TopRight,
            CursorStyle::ResizeUpRightDownLeft,
        ),
        band(
            "bottom-left",
            ResizeEdge::BottomLeft,
            CursorStyle::ResizeUpRightDownLeft,
        ),
        band(
            "bottom-right",
            ResizeEdge::BottomRight,
            CursorStyle::ResizeUpLeftDownRight,
        ),
    ]
}

/// Runs `task` in the workspace, with the project's own variables filled in.
/// Shared, because the window that offers a configuration for an entry point can
/// also run it straight away.
/// The context a configuration written down in the project is resolved against.
///
/// The open item's own context is the more specific one -- it knows the file and
/// the line -- but it says nothing about the project whenever the item is not a
/// file of it: a request the API client has open, a buffer that was never saved,
/// this very window. A configuration kept in `.zed` names `$ZED_WORKTREE_ROOT`,
/// and resolving that against a context without it fails outright, which used to
/// leave the press doing nothing at all. So the worktree's own variables go in
/// first and whatever the open item knows is laid over them.
fn what_to_resolve_against(contexts: &TaskContexts) -> TaskContext {
    let mut context = contexts
        .active_worktree_context
        .as_ref()
        .map(|(_, worktree)| worktree.clone())
        .unwrap_or_default();
    if let Some(active) = contexts.active_context() {
        context.task_variables.extend(active.task_variables.clone());
        context.project_env.extend(
            active
                .project_env
                .iter()
                .map(|(name, value)| (name.clone(), value.clone())),
        );
        if active.cwd.is_some() {
            context.cwd = active.cwd.clone();
        }
    }
    context
}

pub async fn run_a_task(
    workspace: &WeakEntity<Workspace>,
    task: TaskTemplate,
    cx: &mut gpui::AsyncWindowContext,
) {
    let Some(contexts) = workspace
        .update_in(cx, |workspace, window, cx| {
            tasks_ui::task_contexts(workspace, window, cx)
        })
        .ok()
    else {
        return;
    };
    let contexts = contexts.await;
    let context = what_to_resolve_against(&contexts);
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
    // `schedule_task` would resolve this itself and say nothing at all when the
    // command or one of its variables cannot be resolved -- which is exactly
    // what an unavailable `$ZED_WORKTREE_ROOT` or the like does. Resolving here
    // first means a reader who presses Run and sees nothing happen is told why,
    // instead of wondering whether the press even landed.
    match task.resolve_task(&comes_from.to_id_base(), &context) {
        Some(resolved) => {
            workspace
                .update_in(cx, |workspace, window, cx| {
                    workspace.schedule_resolved_task(comes_from, resolved, false, window, cx);
                })
                .ok();
        }
        None => {
            let label = task.label;
            workspace
                .update_in(cx, |workspace, _window, cx| {
                    workspace.show_toast(
                        Toast::new(
                            NotificationId::unique::<TaskTemplate>(),
                            format!(
                                "\"{label}\" could not be run: its command or a variable in \
                                 it (such as $ZED_WORKTREE_ROOT) could not be resolved for \
                                 this project."
                            ),
                        ),
                        cx,
                    );
                })
                .ok();
        }
    }
}

/// The offer the editor left behind, taken rather than read: it is answered once,
/// so a later ask that comes from somewhere else is not filled in from a line the
/// reader looked at long ago.
fn taken_offer(cx: &mut App) -> zed_actions::run_configurations::EntryPointOffer {
    match cx.has_global::<zed_actions::run_configurations::EntryPointOffer>() {
        true => cx.remove_global::<zed_actions::run_configurations::EntryPointOffer>(),
        false => Default::default(),
    }
}

/// Starts a debug session in the workspace, with the project's own variables
/// filled in. Shared, because a configuration is started from the window that
/// lists them, from the form, and from the gutter's own window.
pub async fn start_a_debug_session(
    workspace: &WeakEntity<Workspace>,
    scenario: DebugScenario,
    cx: &mut gpui::AsyncWindowContext,
) {
    let Some(contexts) = workspace
        .update_in(cx, |workspace, window, cx| {
            tasks_ui::task_contexts(workspace, window, cx)
        })
        .ok()
    else {
        return;
    };
    let contexts = contexts.await;
    let worktree = contexts.worktree();
    let context = what_to_resolve_against(&contexts);
    workspace
        .update_in(cx, |workspace, window, cx| {
            workspace.start_debug_session(scenario, context.into(), None, worktree, window, cx);
        })
        .ok();
}

/// The project's run configurations: what the two files hold, in a form that can
/// be clicked together instead of typed -- and written straight back into them.
pub struct RunConfigurationsView {
    store: Entity<ConfigurationsStore>,
    workspace: WeakEntity<Workspace>,
    focus: FocusHandle,
    /// Which configuration is being shown, by the file it is in and where in it.
    chosen: Option<(Kind, usize)>,
    /// The chosen entry exactly as the file had it when the form was filled in.
    /// Writing looks for this rather than trusting the place above, since the file
    /// may have been rewritten by hand in the meantime.
    as_read: Option<Value>,
    /// Set for a configuration that is not in a file yet.
    unsaved: bool,
    /// Set once the reader has typed something the file has not been told about.
    edited: bool,
    /// A terminal of its own for every run, rather than the last one reused.
    use_new_terminal: bool,
    /// Several runs of this configuration at once, rather than the running one
    /// being replaced.
    several_at_once: bool,
    /// What the running configuration is using, and the reading before it, which is
    /// what makes a rate out of two numbers.
    metrics: Option<crate::process_metrics::Metrics>,
    watcher: crate::process_metrics::Watcher,
    /// Whether the machine is being watched at all. Watching costs a reading a
    /// second, which is nothing, but a reader who does not want the row can say so.
    watching: bool,
    /// Whether this window is the one in front. A poll nobody can see is a poll
    /// for nothing, so it stops the moment focus leaves this window and starts
    /// again the moment focus comes back.
    window_active: bool,
    _watching_task: Option<gpui::Task<()>>,
    /// Whether the JSON that lands in the file is shown under the form. The files
    /// are read and edited by hand, so what the form writes has to be checkable.
    showing_json: bool,
    /// Said when the file changed under an edit, rather than quietly throwing the
    /// edit away.
    changed_underneath: bool,
    label: Entity<Editor>,
    command: Entity<Editor>,
    args: Entity<Editor>,
    cwd: Entity<Editor>,
    env_file: Entity<Editor>,
    /// The environment, a row to a variable, as the model holds it. Each row is
    /// two little editors; an empty name is a row the reader has not filled in
    /// yet and is left out of what is written.
    env_rows: Vec<EnvRow>,
    adapter: Entity<Editor>,
    adapter_config: Entity<Editor>,
    builds: Entity<Editor>,
    trouble: Option<SharedString>,
    list_scroll: ScrollHandle,
    form_scroll: ScrollHandle,
    /// The bar the window is dragged by, and which carries its buttons. macOS
    /// draws its own, so there is nothing to put there.
    title_bar: Option<Entity<PlatformTitleBar>>,
    /// A drag delivers a bounds change a frame; the last one is what is kept.
    remembering_bounds: Option<gpui::Task<()>>,
    /// The ways of starting this project that its own files describe, offered
    /// when a configuration is added so there is nothing to type.
    found: Vec<crate::entry_points::EntryPoint>,
    /// The files of variables the project holds, offered for the field that
    /// names one.
    env_files: Vec<std::path::PathBuf>,
    _looking: Option<gpui::Task<()>>,
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
        let store = crate::configurations_store::store_for(&project, cx);
        let mut subscriptions = vec![
            cx.subscribe_in(
                &store,
                window,
                |view: &mut Self, _, _: &ConfigurationsChanged, window, cx| {
                    view.the_files_changed(window, cx)
                },
            ),
            cx.observe_window_bounds(window, |view, window, cx| {
                view.remember_where_it_was_left(window, cx);
            }),
            // Only a window can say which appearance it was given, and the
            // application-wide guess can differ from it. Without this the window
            // opens light in front of a dark editor.
            cx.observe_window_appearance(window, |_, window, cx| {
                *theme::SystemAppearance::global_mut(cx) =
                    theme::SystemAppearance(window.appearance().into());
                theme_settings::reload_theme(cx);
                theme_settings::reload_icon_theme(cx);
            }),
            // The row's poll is not worth running while nobody can see it: this
            // stops it the moment the window loses focus and starts it again the
            // moment focus comes back.
            cx.observe_window_activation(window, Self::window_activation_changed),
            // Turning the row off by setting should stop the poll right away,
            // not wait for the window to lose and regain focus first.
            cx.observe_global::<settings::SettingsStore>(|view, cx| view.watch_the_run(cx)),
        ];
        // The form writes one project's files. When that project's window goes,
        // so does this one -- a form left behind writes to a project nobody has
        // open any more. A view put in a pane is not its window's root, and
        // removing the window there would close the editor itself.
        if let Some(alive) = workspace.upgrade() {
            subscriptions.push(cx.observe_release_in(&alive, window, |_, _, window, _| {
                if window.window_handle().downcast::<Self>().is_some() {
                    window.remove_window();
                }
            }));
        }

        let field = |placeholder: &'static str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_placeholder_text(placeholder, window, cx);
                editor
            })
        };
        // Grows with what is typed in it, between these two heights. A whole
        // editor in a box of a fixed height is what left the arguments in a
        // sliver a few pixels tall, with a line number in it and no room to
        // read the line beside it.
        let lines = |placeholder: &'static str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| {
                let mut editor = Editor::auto_height(3, 10, window, cx);
                editor.set_placeholder_text(placeholder, window, cx);
                editor
            })
        };

        let label = field("What to call it", window, cx);
        let command = field("The command to run", window, cx);
        let args = lines("One argument a line", window, cx);
        let cwd = field("Where to run it, blank for the project root", window, cx);
        let env_file = field("A file of variables, such as .env.local", window, cx);
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

        let mut view = Self {
            store,
            workspace,
            focus: cx.focus_handle(),
            chosen: None,
            as_read: None,
            use_new_terminal: false,
            several_at_once: false,
            metrics: None,
            watcher: crate::process_metrics::Watcher::default(),
            watching: true,
            window_active: window.is_window_active(),
            _watching_task: None,
            showing_json: false,
            unsaved: false,
            edited: false,
            changed_underneath: false,
            label,
            command,
            args,
            cwd,
            env_file,
            env_rows: Vec::new(),
            adapter,
            adapter_config,
            builds,
            trouble: None,
            list_scroll: ScrollHandle::new(),
            form_scroll: ScrollHandle::new(),
            title_bar: (!cfg!(target_os = "macos"))
                .then(|| cx.new(|cx| PlatformTitleBar::new("run-configurations-title-bar", cx))),
            remembering_bounds: None,
            found: Vec::new(),
            env_files: Vec::new(),
            _looking: None,
            _subscriptions: subscriptions,
        };
        view.look_for_ways_to_run(&project, cx);
        // Watching starts with the view: a run that is already going should be
        // reported the moment this is opened, not a second after somebody clicks.
        view.watch_the_run(cx);
        view
    }

    /// Reads the project for the ways it can be started, so the moment of adding
    /// a configuration offers them instead of an empty command field.
    fn look_for_ways_to_run(&mut self, project: &Entity<project::Project>, cx: &mut Context<Self>) {
        self.env_files = crate::entry_points::env_files(project, cx);
        let looking = crate::entry_points::look_through(project, cx);
        self._looking = Some(cx.spawn(async move |view, cx| {
            let found = looking.await;
            view.update(cx, |view, cx| {
                view.found = found;
                cx.notify();
            })
            .log_err();
        }));
    }

    /// Fills the form in from one of the ways the project describes: the command
    /// whole, with its arguments and the directory it runs in.
    fn fill_in_from_way(
        &mut self,
        point: &crate::entry_points::EntryPoint,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.label.update(cx, |editor, cx| {
            editor.set_text(format!("Run {}", point.name), window, cx)
        });
        self.command.update(cx, |editor, cx| {
            editor.set_text(point.how.command.clone(), window, cx)
        });
        self.args.update(cx, |editor, cx| {
            editor.set_text(point.how.args.join("\n"), window, cx)
        });
        if let Some(cwd) = point.how.cwd.clone() {
            self.cwd
                .update(cx, |editor, cx| editor.set_text(cwd, window, cx));
        }
        self.edited = true;
        cx.notify();
    }

    fn remember_where_it_was_left(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.remembering_bounds.is_some() {
            return;
        }
        self.remembering_bounds = Some(cx.spawn_in(window, async move |view, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(100))
                .await;
            view.update_in(cx, |view, window, cx| {
                view.remembering_bounds.take();
                // A maximized or fullscreen window has no placement worth
                // keeping: it would come back as a window the reader never sized.
                if let WindowBounds::Windowed(bounds) = window.inner_window_bounds()
                    && let Some(display) = window.display(cx).and_then(|it| it.uuid().ok())
                {
                    workspace::remembered_window::remember(
                        REMEMBERED_AS,
                        bounds,
                        display.to_string(),
                        cx,
                    );
                }
            })
            .log_err();
        }));
    }

    /// Closing is closing the window: it is the reader's own now, not a sheet
    /// over the editor.
    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // A view added to a pane by hand has no window of its own to remove, and
        // the tab is closed the way any tab is.
        match window.window_handle().downcast::<Self>() {
            Some(_) => window.remove_window(),
            None => cx.emit(gpui::DismissEvent),
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
        self.as_read = match configuration.as_written.is_null() {
            true => None,
            false => Some(configuration.as_written.clone()),
        };
        self.unsaved = false;
        self.edited = false;
        self.changed_underneath = false;
        self.trouble = None;
        self.use_new_terminal = configuration
            .task
            .as_ref()
            .is_some_and(|task| task.use_new_terminal);
        self.several_at_once = configuration
            .task
            .as_ref()
            .is_some_and(|task| task.allow_concurrent_runs);

        let (label, command, args, cwd, env_file) = match &configuration.task {
            Some(task) => (
                task.label.clone(),
                task.command.clone(),
                task.args.join("\n"),
                task.cwd.clone().unwrap_or_default(),
                task.env_file.clone().unwrap_or_default(),
            ),
            None => (
                configuration.label.clone(),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
            ),
        };
        // A row to a variable, in the order the reader will see them again next
        // time: a map has no order of its own, and rows that jump about between
        // two openings of the same configuration read as a bug.
        let mut variables: Vec<(String, String)> = configuration
            .task
            .as_ref()
            .map(|task| {
                task.env
                    .iter()
                    .map(|(name, value)| (name.clone(), value.clone()))
                    .collect()
            })
            .unwrap_or_default();
        variables.sort();
        self.env_rows = variables
            .into_iter()
            .map(|(name, value)| self.an_env_row(&name, &value, window, cx))
            .collect();
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

    /// Starts a configuration that is not in a file yet. An offer from the
    /// editor's gutter fills it in -- what that line runs, named as a reader
    /// reads it -- and without one the fields are left for the reader.
    fn start_from_an_offer(
        &mut self,
        offer: Option<zed_actions::run_configurations::EntryPointOffer>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.start_a_new_one(Kind::Task, window, cx);
        let Some(offer) = offer.filter(|offer| offer.file.is_some() || offer.command.is_some())
        else {
            return;
        };
        let filled_in = crate::templates::task_from(&offer);
        self.show(
            &Configuration {
                kind: Kind::Task,
                at: self.store.read(cx).of_kind(Kind::Task).configurations.len(),
                label: filled_in.label.clone(),
                task: Some(filled_in),
                scenario: None,
                as_written: Value::Null,
            },
            window,
            cx,
        );
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
        for row in &self.env_rows {
            let name = text(&row.name);
            if name.trim().is_empty() {
                continue;
            }
            task.env.insert(name, text(&row.value));
        }
        task.use_new_terminal = self.use_new_terminal;
        task.allow_concurrent_runs = self.several_at_once;
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
        let replacing = match self.unsaved {
            true => None,
            false => self.as_read.clone().map(|as_read| (at, as_read)),
        };
        let writing = self.store.read(cx).save(kind, replacing, entry, cx);
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

    fn remove(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((kind, at)) = self.chosen else {
            return;
        };
        if self.unsaved {
            // Nothing is in the file yet, so there is nothing to lose and nothing
            // to ask about.
            self.chosen = None;
            self.unsaved = false;
            cx.notify();
            return;
        }
        let Some(original) = self.as_read.clone() else {
            self.chosen = None;
            cx.notify();
            return;
        };
        let name = original
            .get("label")
            .or_else(|| original.get("name"))
            .and_then(|name| name.as_str())
            .unwrap_or_default()
            .to_string();
        let message = if name.is_empty() {
            "Take this configuration out of the file? This cannot be undone.".to_string()
        } else {
            format!("Take \"{name}\" out of the file? This cannot be undone.")
        };
        let answer = window.prompt(
            PromptLevel::Warning,
            &message,
            None,
            &["Cancel", "Remove"],
            cx,
        );
        cx.spawn(async move |view, cx| {
            // Cancel comes first, so removing is the second button.
            if answer.await != Ok(1) {
                return;
            }
            let removing = view
                .update(cx, |view, cx| {
                    view.chosen = None;
                    cx.notify();
                    view.store.read(cx).remove(kind, at, original, cx)
                })
                .ok();
            let Some(removing) = removing else {
                return;
            };
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
        self.as_read = None;
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
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |_, cx| {
            run_a_task(&workspace, task, cx).await;
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
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |_, cx| {
            start_a_debug_session(&workspace, scenario, cx).await;
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

    /// A small heading over a group in the list, as the mockup has them.
    fn group_heading(said: impl Into<SharedString>) -> AnyElement {
        h_flex()
            .w_full()
            .px_2()
            .pt_2()
            .pb_1()
            .child(Label::new(said).size(LabelSize::XSmall).color(Color::Muted))
            .into_any_element()
    }

    /// One configuration in the list: what it is, what it is called, and what it
    /// runs. No buttons of its own -- what to do with the chosen one is at the
    /// foot of the window, where a reader looks for it once rather than on every
    /// row.
    fn render_row(&self, configuration: &Configuration, cx: &mut Context<Self>) -> AnyElement {
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
            .debug_selector(move || format!("configuration-{}-{at}", kind.file_name()))
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
            .into_any_element()
    }

    /// One way run on the spot: kept in memory, offered here so it can be looked
    /// at and, if it is worth keeping, written into the project.
    fn render_temporary(
        &self,
        at: usize,
        task: &TaskTemplate,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let label = match task.label.trim().is_empty() {
            true => task.command.clone(),
            false => task.label.clone(),
        };
        let what_it_runs = format!("{} {}", task.command, task.args.join(" "))
            .trim()
            .to_string();
        h_flex()
            .id(("temporary", at))
            .debug_selector(move || format!("temporary-{at}"))
            .w_full()
            .px_2()
            .py_1()
            .gap_2()
            .items_center()
            .hover(|row| row.bg(ui::cyberpunk::row_hovered()))
            .child(
                Label::new("on the spot")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(Label::new(label).size(LabelSize::Small))
                    .child(
                        Label::new(what_it_runs)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .child(
                Button::new(("pin", at), "Keep")
                    .label_size(LabelSize::XSmall)
                    .tooltip(Tooltip::text("Write this into the project's own file"))
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.store
                            .update(cx, |store, cx| store.pin_temporary(at, cx))
                            .detach_and_log_err(cx);
                    })),
            )
            .into_any_element()
    }

    fn render_list(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let kept: Vec<AnyElement> = self
            .store
            .read(cx)
            .all()
            .cloned()
            .collect::<Vec<_>>()
            .iter()
            .map(|configuration| self.render_row(configuration, cx))
            .collect();
        let temporary: Vec<AnyElement> = self
            .store
            .read(cx)
            .temporary()
            .to_vec()
            .iter()
            .enumerate()
            .map(|(at, task)| self.render_temporary(at, task, cx))
            .collect();
        let how_many_temporary = temporary.len();
        let nothing_at_all = kept.is_empty() && temporary.is_empty();

        v_flex()
            .id("configurations-list")
            .flex_none()
            .w(px(300.))
            .h_full()
            .border_r_1()
            .border_color(ui::cyberpunk::border_dim())
            .overflow_y_scroll()
            .track_scroll(&self.list_scroll)
            .when(nothing_at_all, |list| {
                list.child(
                    div().p_3().child(
                        Label::new(
                            "Nothing here yet. Press + above and pick what you are \
                             running -- Go, Rust, Node and the rest fill the command \
                             in for you.",
                        )
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    ),
                )
            })
            .when(!kept.is_empty(), |list| {
                list.child(Self::group_heading("KEPT IN THE PROJECT"))
            })
            .children(kept)
            .when(how_many_temporary > 0, |list| {
                list.child(Self::group_heading(format!(
                    "RUN ON THE SPOT · {how_many_temporary} of {}",
                    crate::configurations_store::MOST_TEMPORARIES_KEPT
                )))
            })
            .children(temporary)
            .custom_scrollbars(
                ui::Scrollbars::always_visible(ui::ScrollAxes::Vertical)
                    .tracked_scroll_handle(&self.list_scroll),
                window,
                cx,
            )
            .into_any_element()
    }

    /// The process a run of this project is going on in, if one is. The terminal
    /// panel holds the runs; a task terminal is one that was started from a task,
    /// which is what a configuration is.
    fn process_of_a_run(&self, cx: &App) -> Option<u32> {
        let workspace = self.workspace.upgrade()?;
        let panel = workspace
            .read(cx)
            .panel::<terminal_view::terminal_panel::TerminalPanel>(cx)?;
        let panel = panel.read(cx);
        let mut newest = None;
        for pane in panel.panes() {
            for item in pane.read(cx).items() {
                let Some(view) = item.downcast::<terminal_view::TerminalView>() else {
                    continue;
                };
                let terminal = view.read(cx).terminal().read(cx);
                if terminal.task().is_some()
                    && let Some(pid) = terminal.pid()
                {
                    newest = Some(pid.as_u32());
                }
            }
        }
        newest
    }

    /// The window this row lives in gained or lost focus. Losing it is exactly
    /// the moment nobody can see the row, so the poll is stopped along with it;
    /// gaining it back starts the poll again.
    fn window_activation_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.window_active = window.is_window_active();
        self.watch_the_run(cx);
    }

    /// Reads what the run is using, once a second, for as long as this view is on
    /// screen, its window has focus, and the reader wants it. The reading itself
    /// happens off the drawing thread: `/proc` holds a few hundred files and none
    /// of that belongs in a frame.
    fn watch_the_run(&mut self, cx: &mut Context<Self>) {
        if !self.watching {
            self._watching_task = None;
            self.metrics = None;
            self.watcher.forget();
            return;
        }
        if !self.window_active || !RunConfigurationsSettings::get_global(cx).show_process_metrics {
            // Neither of these means the run itself stopped, so the last reading
            // stays on screen rather than being thrown away -- it is just that
            // nobody can see it right now, or the reader turned the row off.
            self._watching_task = None;
            return;
        }
        self._watching_task = Some(cx.spawn(async move |view, cx| {
            loop {
                let Ok(pid) = view.read_with(cx, |view, cx| view.process_of_a_run(cx)) else {
                    return;
                };
                let samples = match pid {
                    Some(_) => {
                        cx.background_spawn(
                            async move { crate::process_metrics::everything_running() },
                        )
                        .await
                    }
                    None => None,
                };
                let now = std::time::Instant::now();
                if view
                    .update(cx, |view, cx| {
                        if view.read_the_run(pid, samples.as_deref(), now) {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    return;
                }
                cx.background_executor()
                    .timer(crate::process_metrics::Watcher::HOW_OFTEN)
                    .await;
            }
        }));
    }

    /// One reading. `samples` is every process the machine talked about, or
    /// nothing when it did not answer; `pid` is the run to look for among them.
    /// Says whether the row changed.
    ///
    /// A run the machine has nothing to say about is over, and the row says so.
    /// A machine that did not answer at all leaves the row as it was, rather than
    /// reporting a running thing as gone.
    fn read_the_run(
        &mut self,
        pid: Option<u32>,
        samples: Option<&[crate::process_metrics::Sample]>,
        now: std::time::Instant,
    ) -> bool {
        let Some(pid) = pid else {
            self.watcher.forget();
            return self.metrics.take().is_some();
        };
        let Some(samples) = samples else {
            return false;
        };
        let read = self.watcher.metrics_of(pid, samples, now);
        let changed = read != self.metrics;
        self.metrics = read;
        changed
    }

    /// What the run is using, in one line. A number nobody can measure says why
    /// rather than showing a zero, which would read as "it is using none".
    fn render_metrics(&self, cx: &mut Context<Self>) -> AnyElement {
        let said = |label: &'static str, value: String| {
            h_flex()
                .gap_1()
                .child(
                    Label::new(label)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .child(Label::new(value).size(LabelSize::XSmall))
        };
        let toggle = h_flex()
            .id("metrics-toggle")
            .debug_selector(|| "metrics-toggle".to_string())
            .gap_1()
            .items_center()
            .cursor_pointer()
            .on_click(cx.listener(|view, _, _window, cx| {
                view.watching = !view.watching;
                view.watch_the_run(cx);
                cx.notify();
            }))
            .child(
                Icon::new(match self.watching {
                    true => IconName::Check,
                    false => IconName::Close,
                })
                .size(IconSize::XSmall)
                .color(match self.watching {
                    true => Color::Accent,
                    false => Color::Muted,
                }),
            )
            .child(
                Label::new("Watch the run")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            );

        let Some(metrics) = self.metrics.clone() else {
            return h_flex()
                .id("run-metrics")
                .debug_selector(|| "run-metrics".to_string())
                .w_full()
                .gap_3()
                .items_center()
                // No sentence when there is nothing to say. Absent numbers
                // already mean nothing is running, and the toggle beside them
                // says whether the run is being watched; a line repeating both
                // is one more thing to read and nothing more to learn.
                .child(div().flex_1())
                .child(toggle)
                .into_any_element();
        };

        h_flex()
            .id("run-metrics")
            .debug_selector(|| "run-metrics-reading".to_string())
            .w_full()
            .gap_3()
            .items_center()
            .child(said("PID", metrics.pid.to_string()))
            .child(said("processes", metrics.processes.to_string()))
            .child(said(
                "CPU",
                match metrics.cpu {
                    Some(cpu) => format!("{cpu:.1}%"),
                    None => "-- reading".to_string(),
                },
            ))
            .child(said(
                "RAM",
                crate::process_metrics::as_memory(metrics.memory),
            ))
            .child(said(
                "network",
                match metrics.network {
                    Ok(bytes) => crate::process_metrics::as_memory(bytes),
                    Err(why) => format!("-- {why}"),
                },
            ))
            .child(said(
                "video memory",
                match metrics.video_memory {
                    Ok(bytes) => crate::process_metrics::as_memory(bytes),
                    Err(why) => format!("-- {why}"),
                },
            ))
            .child(div().flex_1())
            .child(toggle)
            .into_any_element()
    }

    /// How a run meets the terminal. Both are the task file's own settings, shown
    /// here because they decide what happens on the second press of Run.
    /// One row of the environment: a name, a value, and the two little editors
    /// that hold them. Every edit marks the form as edited, the same as any other
    /// field, so Save knows there is something to write.
    fn an_env_row(
        &self,
        name: &str,
        value: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> EnvRow {
        let one = |placeholder: &str, said: &str, window: &mut Window, cx: &mut Context<Self>| {
            let editor = cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_placeholder_text(placeholder, window, cx);
                editor.set_text(said, window, cx);
                editor
            });
            cx.subscribe(&editor, |view: &mut Self, _, event, cx| {
                if matches!(
                    event,
                    editor::EditorEvent::Edited { .. } | editor::EditorEvent::BufferEdited { .. }
                ) {
                    view.edited = true;
                    cx.notify();
                }
            })
            .detach();
            editor
        };
        EnvRow {
            name: one("NAME", name, window, cx),
            value: one("value", value, window, cx),
        }
    }

    /// The environment, as the mockup draws it: a row to a variable, a name
    /// beside a value, one taken out from its own end and one added from the
    /// heading.
    /// The template a command comes from, and the debugger that follows from it.
    /// Picking one fills the command and its arguments in; the debugger beside it
    /// is not the reader's to type, since it is worked out from the command.
    /// Fills the command and its arguments in from a template, leaving every
    /// other field as the reader left it.
    fn fill_in_from(
        &mut self,
        template: &crate::templates::Template,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.command.update(cx, |editor, cx| {
            editor.set_text(template.command, window, cx)
        });
        self.args.update(cx, |editor, cx| {
            editor.set_text(template.args.join("\n"), window, cx)
        });
        self.edited = true;
        cx.notify();
    }

    fn render_env(&self, cx: &mut Context<Self>) -> AnyElement {
        let boxed = |editor: &Entity<Editor>, wide: bool| {
            div()
                // Same rule as every other field: a minimum height with the line
                // centred, never a fixed one.
                .flex()
                .items_center()
                .min_h(px(32.))
                .when(wide, |cell| cell.flex_1().min_w_0())
                .when(!wide, |cell| cell.w(px(200.)))
                .px_2()
                .py_1()
                .rounded_lg()
                .border_1()
                .border_color(ui::cyberpunk::border_dim())
                .child(editor.clone())
        };
        let rows: Vec<AnyElement> = self
            .env_rows
            .iter()
            .enumerate()
            .map(|(at, row)| {
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_center()
                    .child(boxed(&row.name, false))
                    .child(boxed(&row.value, true))
                    .child(
                        IconButton::new(("env-remove", at), IconName::Dash)
                            .icon_size(IconSize::XSmall)
                            .tooltip(Tooltip::text("Take this variable out"))
                            .on_click(cx.listener(move |view, _, _, cx| {
                                if at < view.env_rows.len() {
                                    view.env_rows.remove(at);
                                    view.edited = true;
                                    cx.notify();
                                }
                            })),
                    )
                    .into_any_element()
            })
            .collect();

        v_flex().w_full().gap_1().children(rows).into_any_element()
    }

    /// Asks for the file of variables rather than making the reader type a path.
    /// What comes back is put in the field as it is: a path relative to the
    /// project is what the file wants, and turning an absolute one into that is
    /// the reader's own business -- guessing it wrong would point the run at
    /// nothing.
    fn find_the_env_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let start = self
            .store
            .read(cx)
            .project_root()
            .cloned()
            .unwrap_or_else(|| paths::home_dir().clone());
        let chosen = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose the file of variables".into()),
        });
        let _ = start;
        cx.spawn_in(window, async move |view, cx| {
            let Ok(Ok(Some(paths))) = chosen.await else {
                return;
            };
            let Some(path) = paths.first().cloned() else {
                return;
            };
            view.update_in(cx, |view, window, cx| {
                let root = view.store.read(cx).project_root().cloned();
                let said = match root.and_then(|root| {
                    path.strip_prefix(&root)
                        .ok()
                        .map(|inside| inside.display().to_string())
                }) {
                    Some(inside) => inside,
                    None => path.display().to_string(),
                };
                view.env_file.update(cx, |editor, cx| {
                    editor.set_text(said, window, cx);
                });
                view.edited = true;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn render_run_toggles(&self, cx: &mut Context<Self>) -> AnyElement {
        let switch = |id: &'static str,
                      label: &'static str,
                      hint: &'static str,
                      on: bool,
                      cx: &mut Context<Self>,
                      toggle: fn(&mut Self, &mut Context<Self>)| {
            h_flex()
                .id(id)
                .debug_selector(move || id.to_string())
                .gap_2()
                .items_center()
                .cursor_pointer()
                .on_click(cx.listener(move |view, _, _window, cx| toggle(view, cx)))
                .child(ui::Checkbox::new(
                    id,
                    match on {
                        true => ui::ToggleState::Selected,
                        false => ui::ToggleState::Unselected,
                    },
                ))
                .child(Label::new(label).size(LabelSize::Small))
                .child(Label::new(hint).size(LabelSize::XSmall).color(Color::Muted))
        };
        v_flex()
            .gap_1()
            .child(switch(
                "configuration-new-terminal",
                "A terminal of its own",
                "every run opens one rather than reusing the last",
                self.use_new_terminal,
                cx,
                |view, cx| {
                    view.use_new_terminal = !view.use_new_terminal;
                    view.edited = true;
                    cx.notify();
                },
            ))
            .child(switch(
                "configuration-several-at-once",
                "Several at once",
                "a new run leaves the running one alone",
                self.several_at_once,
                cx,
                |view, cx| {
                    view.several_at_once = !view.several_at_once;
                    view.edited = true;
                    cx.notify();
                },
            ))
            .into_any_element()
    }

    /// The entry as it will stand in the file. These files are read and edited by
    /// hand and go into the project's history, so a form that hides what it writes
    /// leaves the reader guessing.
    /// The entry as JSON, exactly as the block below the form shows it.
    fn json_for_test(&self, kind: Kind, cx: &App) -> String {
        let written = match kind {
            Kind::Task => configurations_file::task_as_written(&self.task_in_the_form(cx)),
            Kind::Debug => configurations_file::scenario_as_written(&self.scenario_in_the_form(cx)),
        };
        match written {
            Ok(entry) => {
                serde_json::to_string_pretty(&entry).unwrap_or_else(|error| format!("// {error}"))
            }
            Err(error) => format!("// {error:#}"),
        }
    }

    fn render_as_json(&self, kind: Kind, cx: &mut Context<Self>) -> AnyElement {
        let text = self.json_for_test(kind, cx);
        v_flex()
            .w_full()
            .gap_1()
            .child(
                h_flex()
                    .id("configuration-json-toggle")
                    .debug_selector(|| "configuration-json-toggle".to_string())
                    .gap_1()
                    .items_center()
                    .cursor_pointer()
                    .on_click(cx.listener(|view, _, _window, cx| {
                        view.showing_json = !view.showing_json;
                        cx.notify();
                    }))
                    .child(
                        Icon::new(match self.showing_json {
                            true => IconName::ChevronDown,
                            false => IconName::ChevronRight,
                        })
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                    )
                    .child(
                        Label::new("Show as JSON")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .when(self.showing_json, |block| {
                block.child(
                    div()
                        .id("configuration-json")
                        .debug_selector(|| "configuration-json".to_string())
                        .w_full()
                        .px_2()
                        .py_1()
                        .border_1()
                        .border_color(ui::cyberpunk::border_dim())
                        .child(
                            Label::new(text)
                                .size(LabelSize::XSmall)
                                .buffer_font(cx)
                                .color(Color::Muted),
                        ),
                )
            })
            .into_any_element()
    }

    fn render_form(&self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
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

        let editor_background = cx.theme().colors().editor_background;
        let field = |name: &'static str, editor: &Entity<Editor>, tall: bool| {
            v_flex()
                .w_full()
                .gap_1()
                .child(
                    div()
                        .debug_selector(move || format!("configuration-label-{name}"))
                        .child(Label::new(name).size(LabelSize::XSmall).color(Color::Muted)),
                )
                .child(
                    div()
                        .w_full()
                        .debug_selector(move || format!("configuration-field-{name}"))
                        // A ground of its own under every field: a box drawn with
                        // a line alone reads as a rule across the form rather
                        // than as somewhere to type.
                        .bg(editor_background)
                        .rounded_lg()
                        // A minimum rather than a fixed height, and the line
                        // centred inside it. A fixed box stops fitting the moment
                        // the text scale moves, and the line then sits nearer the
                        // top than the bottom -- which is the whole reason a
                        // field can look subtly wrong without anything being
                        // obviously broken.
                        .when(!tall, |field| field.flex().items_center().min_h(px(34.)))
                        // Tall fields take the height their editor asks for, and
                        // no less than three lines of it.
                        .when(tall, |field| field.min_h(px(84.)))
                        .px_2()
                        .py_1()
                        .border_1()
                        .border_color(ui::cyberpunk::border_dim())
                        .child(editor.clone()),
                )
        };

        // A rule with a few words on it, to break a long column of fields into
        // the handful of things a reader is actually deciding.
        let section = |name: &'static str| {
            h_flex()
                .w_full()
                .pt_2()
                .gap_2()
                .items_center()
                .debug_selector(move || format!("configuration-section-{name}"))
                .child(
                    Label::new(name)
                        .size(LabelSize::XSmall)
                        .color(Color::Accent),
                )
                .child(div().flex_1().h(px(1.)).bg(ui::cyberpunk::border_dim()))
        };

        v_flex()
            .id("configuration-form")
            .flex_1()
            .min_w_0()
            .h_full()
            .p_4()
            // A reading column rather than the whole window. On a wide screen a
            // command field a metre long is harder to read, not easier, and the
            // labels drift away from the values they name.
            .max_w(px(880.))
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
            .child(field("NAME", &self.label, false))
            .when(kind == Kind::Task, |form| {
                form.child(section("WHAT TO RUN"))
                    .child(field("COMMAND", &self.command, false))
                    .child(field("ARGUMENTS", &self.args, true))
                    .child(field("WORKING DIRECTORY", &self.cwd, false))
                    .child(
                        // The plus rides the rule that names the section rather
                        // than sitting alone on a line of its own beneath it.
                        section("ENVIRONMENT").child(
                            IconButton::new("env-add", IconName::Plus)
                                .icon_size(IconSize::XSmall)
                                .tooltip(Tooltip::text("Add a variable"))
                                .on_click(cx.listener(|view, _, window, cx| {
                                    let row = view.an_env_row("", "", window, cx);
                                    view.env_rows.push(row);
                                    view.edited = true;
                                    cx.notify();
                                })),
                        ),
                    )
                    .child(self.render_env(cx))
                    .child(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .items_end()
                            .child(div().flex_1().child(field(
                                "ENVIRONMENT FILE",
                                &self.env_file,
                                false,
                            )))
                            .child({
                                let view = cx.entity();
                                PopoverMenu::new("env-file-offer")
                                    .trigger(
                                        Button::new("env-file-find", "…")
                                            .label_size(LabelSize::Small)
                                            .tooltip(Tooltip::text("Pick the file of variables")),
                                    )
                                    .menu(move |window, cx| {
                                        let view = view.clone();
                                        let offered = view.read(cx).env_files.clone();
                                        Some(ContextMenu::build(
                                            window,
                                            cx,
                                            move |mut menu, _, _| {
                                                for path in offered {
                                                    let said = path.to_string_lossy().into_owned();
                                                    let view = view.clone();
                                                    menu = menu.entry(
                                                        SharedString::from(said.clone()),
                                                        None,
                                                        move |window, cx| {
                                                            let said = said.clone();
                                                            view.update(cx, |view, cx| {
                                                                view.env_file.update(
                                                                    cx,
                                                                    |editor, cx| {
                                                                        editor.set_text(
                                                                            said, window, cx,
                                                                        )
                                                                    },
                                                                );
                                                                view.edited = true;
                                                                cx.notify();
                                                            });
                                                        },
                                                    );
                                                }
                                                menu.separator().entry(
                                                    "Choose a file…",
                                                    None,
                                                    move |window, cx| {
                                                        view.update(cx, |view, cx| {
                                                            view.find_the_env_file(window, cx)
                                                        });
                                                    },
                                                )
                                            },
                                        ))
                                    })
                            }),
                    )
            })
            .when(kind == Kind::Debug, |form| {
                form.child(section("WHAT DEBUGS IT"))
                    .child(field("DEBUGGER", &self.adapter, false))
                    .child(field("BUILDS FIRST", &self.builds, false))
                    .child(field("WHAT THE DEBUGGER NEEDS", &self.adapter_config, true))
            })
            .when(kind == Kind::Task, |form| {
                form.child(section("HOW TO RUN IT"))
                    .child(self.render_run_toggles(cx))
            })
            .child(self.render_as_json(kind, cx))
            .into_any_element()
    }
}

impl EventEmitter<gpui::DismissEvent> for RunConfigurationsView {}
impl Focusable for RunConfigurationsView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl RunConfigurationsView {
    /// The window's own row of controls, as the mockup has it: add, duplicate,
    /// remove, and move the chosen one up or down the file.
    fn render_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let has_one = self.chosen.is_some();
        h_flex()
            .gap_px()
            .child({
                // The templates belong to the moment of adding: this is where a
                // reader decides what kind of thing they are running, and a
                // dropdown sitting in the form afterwards only asks them to
                // decide again about something already decided.
                let view = cx.entity();
                PopoverMenu::new("configuration-add-task")
                    .trigger(
                        IconButton::new("configuration-add-task", IconName::Plus)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Add a way of running")),
                    )
                    .menu(move |window, cx| {
                        let view = view.clone();
                        // What the project itself says can be run comes first and
                        // whole: a command with its package, its arguments and the
                        // directory it runs in, so nothing is left to type. The
                        // bare templates stay underneath for a project this found
                        // nothing in.
                        let found = view.read(cx).found.clone();
                        Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                            let mut family = None;
                            for point in found {
                                if family != Some(point.family) {
                                    family = Some(point.family);
                                    menu = menu.header(point.family.shown());
                                }
                                let view = view.clone();
                                menu = menu.entry(
                                    SharedString::from(point.name.clone()),
                                    None,
                                    move |window, cx| {
                                        let point = point.clone();
                                        view.update(cx, |view, cx| {
                                            view.start_a_new_one(Kind::Task, window, cx);
                                            view.fill_in_from_way(&point, window, cx);
                                        });
                                    },
                                );
                            }
                            if family.is_some() {
                                menu = menu.separator().header("Fill in by hand");
                            }
                            for template in crate::templates::TEMPLATES {
                                let view = view.clone();
                                menu = menu.entry(template.name, None, move |window, cx| {
                                    view.update(cx, |view, cx| {
                                        view.start_a_new_one(Kind::Task, window, cx);
                                        view.fill_in_from(template, window, cx);
                                    });
                                });
                            }
                            menu.separator()
                                .entry("Something else", None, move |window, cx| {
                                    view.update(cx, |view, cx| {
                                        view.start_a_new_one(Kind::Task, window, cx)
                                    });
                                })
                        }))
                    })
            })
            .child(
                IconButton::new("configuration-add-debug", IconName::Debug)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Add a way of debugging"))
                    .on_click(cx.listener(|view, _, window, cx| {
                        view.start_a_new_one(Kind::Debug, window, cx)
                    })),
            )
            .child(
                IconButton::new("configuration-duplicate", IconName::Copy)
                    .icon_size(IconSize::Small)
                    .disabled(!has_one)
                    .tooltip(Tooltip::text("Make a copy of this one"))
                    .on_click(cx.listener(|view, _, window, cx| view.duplicate(window, cx))),
            )
            .child(
                IconButton::new("configuration-remove", IconName::Dash)
                    .icon_size(IconSize::Small)
                    .disabled(!has_one)
                    .tooltip(Tooltip::text("Take this one out of the file"))
                    .on_click(cx.listener(|view, _, window, cx| view.remove(window, cx))),
            )
            .child(
                IconButton::new("configuration-earlier", IconName::ChevronUp)
                    .icon_size(IconSize::Small)
                    .disabled(!has_one)
                    .tooltip(Tooltip::text("Move it earlier in the file"))
                    .on_click(cx.listener(|view, _, _, cx| view.move_it(false, cx))),
            )
            .child(
                IconButton::new("configuration-later", IconName::ChevronDown)
                    .icon_size(IconSize::Small)
                    .disabled(!has_one)
                    .tooltip(Tooltip::text("Move it later in the file"))
                    .on_click(cx.listener(|view, _, _, cx| view.move_it(true, cx))),
            )
            .into_any_element()
    }

    /// Moves the chosen configuration one place in its file, which is the order
    /// everything that lists them shows.
    fn move_it(&mut self, later: bool, cx: &mut Context<Self>) {
        let Some((kind, at)) = self.chosen else {
            return;
        };
        let Some(original) = self
            .store
            .read(cx)
            .get(kind, at)
            .map(|configuration| configuration.as_written.clone())
        else {
            return;
        };
        let writing = self.store.read(cx).move_it(kind, at, original, later, cx);
        // Which one is chosen follows it, so pressing again moves the same entry
        // rather than whatever has taken its place.
        self.chosen = Some((
            kind,
            match later {
                true => at + 1,
                false => at.saturating_sub(1),
            },
        ));
        cx.spawn(async move |view, cx| {
            let said = writing.await;
            view.update(cx, |view, cx| {
                view.trouble = said
                    .err()
                    .map(|error| SharedString::from(format!("{error:#}")));
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Why the Debug button on the footer has to be withheld for the chosen
    /// configuration, or `None` when it may be pressed. Only a task's own
    /// command is judged: a debug configuration already names its debugger, so
    /// deriving one from a command never comes up for it.
    fn why_the_debug_button_is_withheld(&self, cx: &App) -> Option<&'static str> {
        match self.chosen {
            Some((Kind::Task, _)) => {
                crate::debugging::why_it_cannot_be_debugged(&self.command.read(cx).text(cx))
            }
            _ => None,
        }
    }

    /// What the window is for, at the foot of it: where this is written down, and
    /// what can be done with the one in front of the reader.
    fn render_footer(&self, cx: &mut Context<Self>) -> AnyElement {
        let has_one = self.chosen.is_some();
        let kind_being_edited = self.chosen.map(|(kind, _)| kind).unwrap_or(Kind::Task);
        let cannot_debug = self.why_the_debug_button_is_withheld(cx);
        h_flex()
            .flex_none()
            .w_full()
            .px_3()
            .py_2()
            .gap_2()
            .items_center()
            .border_t_1()
            .border_color(ui::cyberpunk::border_dim())
            // One quiet control instead of a sentence plus a button beside it:
            // the file's own name says where the configurations are kept, and a
            // name that opens the file needs no verb next to it.
            .child(
                div()
                    .id("configuration-open-file-hit-area")
                    .debug_selector(|| "configuration-open-file".to_string())
                    .child(
                        Button::new(
                            "configuration-open-file-button",
                            format!(".zed/{}", kind_being_edited.file_name()),
                        )
                        .label_size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .tooltip(Tooltip::text("Open the file these are kept in"))
                        .disabled(!has_one)
                        .on_click(cx.listener(|view, _, window, cx| {
                            let kind = view.chosen.map(|(kind, _)| kind).unwrap_or(Kind::Task);
                            view.open_the_file(kind, window, cx)
                        })),
                    ),
            )
            // What the run is using, and the way to stop watching it: this says
            // plainly when nothing is running, so the line is always here and the
            // switch is always reachable -- unless the row was turned off by
            // setting, in which case none of it is painted at all.
            .when(
                RunConfigurationsSettings::get_global(cx).show_process_metrics,
                |footer| {
                    footer
                        .child(div().w(px(12.)))
                        .child(self.render_metrics(cx))
                },
            )
            .children(self.trouble.clone().map(|trouble| {
                Label::new(trouble)
                    .size(LabelSize::XSmall)
                    .color(Color::Error)
                    .into_any_element()
            }))
            .child(
                Button::new("configuration-cancel", "Close")
                    .label_size(LabelSize::Small)
                    .on_click(cx.listener(|view, _, window, cx| view.close(window, cx))),
            )
            .child(
                Button::new("configuration-save", "Save")
                    .label_size(LabelSize::Small)
                    .disabled(!has_one)
                    .on_click(cx.listener(|view, _, _, cx| view.save(cx))),
            )
            .child(
                div()
                    .id("configuration-debug-hit-area")
                    .debug_selector(|| "configuration-debug".to_string())
                    .child(
                        Button::new("configuration-debug-button", "Debug")
                            .label_size(LabelSize::Small)
                            .disabled(!has_one || cannot_debug.is_some())
                            .when_some(cannot_debug, |button, reason| {
                                button.tooltip(Tooltip::text(reason))
                            })
                            .on_click(cx.listener(|view, _, window, cx| view.debug(window, cx))),
                    ),
            )
            .child(
                div()
                    .id("configuration-run-hit-area")
                    .debug_selector(|| "configuration-run".to_string())
                    .child(
                        Button::new("configuration-run-button", "Run")
                            .label_size(LabelSize::Small)
                            .style(ButtonStyle::Tinted(ui::TintColor::Accent))
                            .disabled(!has_one)
                            .on_click(cx.listener(|view, _, window, cx| view.run(window, cx))),
                    ),
            )
            .into_any_element()
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

        let body = v_flex()
            .key_context("RunConfigurations")
            .track_focus(&self.focus)
            .debug_selector(|| "run-configurations".to_string())
            .on_action(
                cx.listener(|view, _: &RunThisConfiguration, window, cx| view.run(window, cx)),
            )
            .on_action(
                cx.listener(|view, _: &DebugThisConfiguration, window, cx| view.debug(window, cx)),
            )
            .on_action(cx.listener(|view, _: &SaveThisConfiguration, _, cx| view.save(cx)))
            .on_action(cx.listener(|view, _: &menu::Cancel, window, cx| view.close(window, cx)))
            .flex_1()
            .min_h_0()
            .w_full()
            .bg(cx.theme().colors().background)
            .text_color(cx.theme().colors().text)
            .overflow_hidden()
            .child(
                h_flex()
                    .flex_none()
                    .w_full()
                    .px_3()
                    .py_2()
                    .gap_2()
                    .items_center()
                    .border_b_1()
                    .border_color(ui::cyberpunk::border_dim())
                    .child(
                        Label::new("RUN CONFIGURATIONS")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1())
                    .children(trouble_in_files),
            )
            .child(
                h_flex()
                    .flex_none()
                    .w_full()
                    .px_2()
                    .py_1()
                    .child(self.render_toolbar(cx)),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(self.render_list(window, cx))
                    .child(self.render_form(window, cx)),
            )
            .child(self.render_footer(cx));

        // A window of its own gets a window's shell: a bar to drag it by and
        // borders to pull. The same view can also be put in a pane as a tab,
        // which has both already and must not grow a second set.
        if window.window_handle().downcast::<Self>().is_none() {
            return body.into_any_element();
        }

        client_side_decorations(
            div()
                .size_full()
                .relative()
                .bg(cx.theme().colors().background)
                .child(
                    // A tenth larger than the editor's own text: this is a form
                    // to read and type in, not a wall of code, and it is read at
                    // arm's length from a window that floats over everything.
                    ui::utils::WithRemSize::new(window.rem_size() * 1.1)
                        .size_full()
                        .child(
                            v_flex()
                                .size_full()
                                .child(
                                    div()
                                        .debug_selector(|| {
                                            "run-configurations-titlebar".to_string()
                                        })
                                        .w_full()
                                        .flex_none()
                                        .children(self.title_bar.clone()),
                                )
                                .child(body),
                        ),
                )
                .children(what_the_window_is_pulled_by()),
            window,
            cx,
        )
        .into_any_element()
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
    use gpui::{TestAppContext, VisualTestContext, size};
    use project::{FakeFs, Project};
    use serde_json::json;
    use util::path;

    fn draw(cx: &mut VisualTestContext) {
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        cx.run_until_parked();
    }

    fn debug_center(
        cx: &mut VisualTestContext,
        selector: &'static str,
    ) -> gpui::Point<gpui::Pixels> {
        cx.debug_bounds(selector)
            .unwrap_or_else(|| panic!("expected debug bounds for {selector}"))
            .center()
    }

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            crate::init(cx);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
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

    /// The same view, in a window of its own -- which is what puts its own shell
    /// around it: the bar to drag it by, the bands to pull it by, and the larger
    async fn a_window_of(
        tasks: &str,
        cx: &mut TestAppContext,
    ) -> (Entity<RunConfigurationsView>, VisualTestContext) {
        let (pane_view, _fs, mut pane_cx) = a_view_of(Some(tasks), cx).await;
        a_window_beside(&pane_view, &mut pane_cx)
    }

    /// A second window over the same app, with the view as its root -- which is
    /// what gives it the shell a window of its own has.
    fn a_window_beside(
        beside: &Entity<RunConfigurationsView>,
        cx: &mut VisualTestContext,
    ) -> (Entity<RunConfigurationsView>, VisualTestContext) {
        let workspace = beside.read_with(cx, |view, _| view.workspace.clone());
        let project = workspace
            .read_with(cx, |workspace, _| workspace.project().clone())
            .expect("the workspace the view belongs to");
        let mut app = cx.cx.clone();
        let opened =
            app.add_window(|window, cx| RunConfigurationsView::new(project, workspace, window, cx));
        let mut window_cx = VisualTestContext::from_window(opened.into(), &app);
        let view = opened.root(&mut window_cx).expect("the window's view");
        window_cx.run_until_parked();
        (view, window_cx)
    }

    /// Shows the first configuration the store holds, which is what fills the
    /// form in.
    fn show_the_first_configuration(
        view: &Entity<RunConfigurationsView>,
        cx: &mut VisualTestContext,
    ) {
        view.update_in(cx, |view, window, cx| {
            let first = view
                .store
                .read(cx)
                .get(Kind::Task, 0)
                .cloned()
                .expect("the configuration");
            view.show(&first, window, cx);
        });
        cx.run_until_parked();
        draw(cx);
    }

    /// The configurations open in a window of the reader's own: one they can drag
    /// anywhere and pull to any size, not a sheet pinned to the middle of the
    /// editor. Measured on the window itself and on what it paints, since a
    /// fixed-size view would keep its size however the window is pulled.
    #[gpui::test]
    async fn the_configurations_open_in_a_window_that_can_be_moved_and_sized(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({ "src": { "main.rs": "" } }))
            .await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        // Through a whole window, because the action that opens it is registered
        // by the workspace and only listened for there.
        let (_multi_workspace, editor_cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        editor_cx.run_until_parked();
        let windows_before = editor_cx.update(|_, cx| cx.windows().len());

        editor_cx.dispatch_action(OpenRunConfigurations);
        editor_cx.run_until_parked();

        let opened = editor_cx
            .update(|_, cx| {
                cx.windows()
                    .into_iter()
                    .find_map(|window| window.downcast::<RunConfigurationsView>())
            })
            .expect("the configurations opened a window of their own");
        assert_eq!(
            editor_cx.update(|_, cx| cx.windows().len()),
            windows_before + 1,
            "the editor's window is still there, with the configurations beside it"
        );

        let mut window_cx = VisualTestContext::from_window(opened.into(), &editor_cx.cx);
        window_cx.run_until_parked();
        let narrow = window_cx
            .debug_bounds("run-configurations")
            .expect("the configurations are painted in their window");

        // Pulled wider by 240 pixels: what is inside has to follow the window,
        // which a view laid out at a fixed width would not.
        let was = window_cx.update(|window, _| window.bounds().size);
        window_cx.simulate_resize(size(was.width + px(240.), was.height + px(120.)));
        window_cx.run_until_parked();
        let wide = window_cx
            .debug_bounds("run-configurations")
            .expect("the configurations are still painted after the window was pulled");
        assert!(
            wide.size.width > narrow.size.width + px(200.),
            "the form had to follow the window: {:?} against {:?}",
            narrow.size.width,
            wide.size.width
        );
        assert!(
            wide.size.height > narrow.size.height + px(80.),
            "and follow it downwards too: {:?} against {:?}",
            narrow.size.height,
            wide.size.height
        );

        // Dragging the window itself is the window manager's to do -- the test
        // platform refuses to be asked -- so what is checked here is that there
        // is a bar to drag it by, across the top of the window.
        let bar = window_cx
            .debug_bounds("run-configurations-titlebar")
            .expect("the window has a bar to drag it by");
        assert!(
            bar.size.height > px(8.),
            "the bar has to be tall enough to grab: {:?}",
            bar.size
        );
        assert!(
            bar.size.width > wide.size.width - px(4.),
            "the bar spans the window: {:?} against {:?}",
            bar.size.width,
            wide.size.width
        );
        assert!(
            bar.origin.y < wide.origin.y,
            "and sits above the form, not under it: {:?} against {:?}",
            bar.origin.y,
            wide.origin.y
        );

        // Closing it is closing the window, and the editor is left alone.
        window_cx.dispatch_action(menu::Cancel);
        window_cx.run_until_parked();
        // Counted from the editor's window, since the one that was closed can no
        // longer be asked anything.
        assert_eq!(
            editor_cx.update(|_, cx| cx.windows().len()),
            windows_before,
            "closing the configurations left only the editor's own window"
        );
    }

    /// Two projects are two sets of files, so each gets its own form. Asking
    /// from the second must not bring the first project's window forward, which
    /// would show -- and then write -- somebody else's configurations.
    #[gpui::test]
    async fn a_second_project_gets_a_window_of_its_own(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/one"), json!({ "src": { "main.rs": "" } }))
            .await;
        fs.insert_tree(path!("/two"), json!({ "src": { "main.rs": "" } }))
            .await;
        let first = Project::test(fs.clone(), [path!("/one").as_ref()], cx).await;
        let second = Project::test(fs.clone(), [path!("/two").as_ref()], cx).await;

        let one = cx.add_window(|window, cx| {
            workspace::MultiWorkspace::test_new(first.clone(), window, cx)
        });
        let two = cx.add_window(|window, cx| {
            workspace::MultiWorkspace::test_new(second.clone(), window, cx)
        });
        let mut one_cx = VisualTestContext::from_window(one.into(), cx);
        let mut two_cx = VisualTestContext::from_window(two.into(), cx);
        one_cx.run_until_parked();
        two_cx.run_until_parked();

        one_cx.dispatch_action(OpenRunConfigurations);
        one_cx.run_until_parked();
        two_cx.dispatch_action(OpenRunConfigurations);
        two_cx.run_until_parked();
        assert_eq!(
            forms_open(&mut two_cx),
            2,
            "each project has to get a form of its own"
        );

        // And asking the first project again brings its own window forward
        // rather than opening a third.
        one_cx.dispatch_action(OpenRunConfigurations);
        one_cx.run_until_parked();
        assert_eq!(
            forms_open(&mut one_cx),
            2,
            "asking again had to reach that project's own open window"
        );
    }

    /// Two asks in the same frame are two asks for the same window. Both are
    /// answered after the frame, so neither may decide on its own that there is
    /// no window yet.
    #[gpui::test]
    async fn asking_twice_in_one_frame_opens_one_window(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({ "src": { "main.rs": "" } }))
            .await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (_multi_workspace, editor_cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        editor_cx.run_until_parked();

        editor_cx.dispatch_action(OpenRunConfigurations);
        editor_cx.dispatch_action(OpenRunConfigurations);
        editor_cx.run_until_parked();
        assert_eq!(
            forms_open(editor_cx),
            1,
            "the two asks had to end up at the same window"
        );
    }

    /// The form writes one project's files. When that project's window closes
    /// there is nothing left for it to write to, so it goes with it rather than
    /// being left behind over a project nobody has open.
    #[gpui::test]
    async fn closing_the_editor_takes_the_form_with_it(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({ "src": { "main.rs": "" } }))
            .await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let editor = cx.add_window(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let mut editor_cx = VisualTestContext::from_window(editor.into(), cx);
        editor_cx.run_until_parked();
        editor_cx.dispatch_action(OpenRunConfigurations);
        editor_cx.run_until_parked();
        assert_eq!(forms_open(&mut editor_cx), 1, "the form opened");

        editor_cx.update(|window, _| window.remove_window());
        // Counted through the application, since by then neither window is left
        // to be asked anything.
        cx.run_until_parked();
        assert_eq!(
            cx.update(|cx| {
                cx.windows()
                    .into_iter()
                    .filter(|window| window.downcast::<RunConfigurationsView>().is_some())
                    .count()
            }),
            0,
            "the form had to close with the project's own window"
        );
    }

    fn forms_open(cx: &mut VisualTestContext) -> usize {
        cx.update(|_, cx| {
            cx.windows()
                .into_iter()
                .filter(|window| window.downcast::<RunConfigurationsView>().is_some())
                .count()
        })
    }

    /// Asking for the configurations while they are already open brings that
    /// window forward instead of opening a second form over the same files.
    #[gpui::test]
    async fn asking_again_brings_the_open_window_forward(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({ "src": { "main.rs": "" } }))
            .await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (_multi_workspace, editor_cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        editor_cx.run_until_parked();

        editor_cx.dispatch_action(OpenRunConfigurations);
        editor_cx.run_until_parked();
        let after_first = editor_cx.update(|_, cx| cx.windows().len());

        editor_cx.dispatch_action(OpenRunConfigurations);
        editor_cx.run_until_parked();
        assert_eq!(
            editor_cx.update(|_, cx| cx.windows().len()),
            after_first,
            "the second ask had to reach the open window, not open another one"
        );
    }

    /// The whole chain the gutter starts: what the editor found becomes a window
    /// with the fields already filled in, and saving it puts it in the project's
    /// own file.
    #[gpui::test]
    async fn an_entry_point_the_editor_found_becomes_a_configuration(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({ "cmd": { "api": { "main.go": "package main\n\nfunc main() {}\n" } } }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        // Through a whole window, because the actions a workspace registers are only
        // listened for there -- which is where the editor's gutter dispatches this
        // one too.
        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let _workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
        cx.run_until_parked();

        // What the editor leaves behind when the reader asks its gutter.
        cx.update(|_, cx| {
            cx.set_global(zed_actions::run_configurations::EntryPointOffer {
                language: Some("Go".to_string()),
                file: Some(std::path::PathBuf::from(path!("/project/cmd/api/main.go"))),
                line: 3,
                label: Some("go run ./cmd/api".to_string()),
                command: Some("go".to_string()),
                args: vec!["run".to_string(), ".".to_string()],
                cwd: Some("${ZED_DIRNAME}".to_string()),
                env: Default::default(),
            });
        });
        let handled = cx.update(|window, cx| {
            window
                .available_actions(cx)
                .iter()
                .any(|action| action.as_any().is::<CreateFromEntryPoint>())
        });
        assert!(
            handled,
            "the workspace has to answer the action the editor's gutter dispatches"
        );
        cx.dispatch_action(CreateFromEntryPoint);
        cx.run_until_parked();

        let form = cx
            .update(|_, cx| {
                cx.windows()
                    .into_iter()
                    .find_map(|window| window.downcast::<RunConfigurationsView>())
            })
            .expect("asking the gutter opens the window");
        let view = form.root(cx).expect("the window holds the form");

        let filled_in = view.read_with(cx, |view, cx| view.task_in_the_form(cx));
        assert_eq!(
            filled_in.label, "go run ./cmd/api",
            "the fields come filled in from what the editor already runs"
        );
        assert_eq!(filled_in.command, "go");
        assert_eq!(filled_in.args, vec!["run".to_string(), ".".to_string()]);

        view.update_in(cx, |view, _window, cx| view.save(cx));
        cx.run_until_parked();

        let written = fs
            .load(path!("/project/.zed/tasks.json").as_ref())
            .await
            .expect("saving writes the project's own file");
        let read_back = crate::configurations_file::read(Kind::Task, &written);
        assert_eq!(read_back.configurations.len(), 1);
        assert_eq!(
            read_back.configurations[0]
                .task
                .as_ref()
                .expect("a task the editor can read back")
                .command,
            "go"
        );
    }

    /// The row that says what a run is using is on screen from the moment the
    /// window opens, and says plainly when there is nothing to watch rather than
    /// showing zeroes.
    #[gpui::test]
    async fn the_metrics_row_says_when_there_is_nothing_to_watch(cx: &mut TestAppContext) {
        let (view, _fs, mut cx) = a_view_of(None, cx).await;
        draw(&mut cx);

        assert!(
            cx.debug_bounds("run-metrics").is_some(),
            "the row is there whether or not anything is running"
        );
        assert!(
            view.read_with(&cx, |view, _| view.metrics.is_none()),
            "with nothing running there is nothing measured"
        );
        assert!(
            view.read_with(&cx, |view, _| view.watching),
            "and it is watching, ready for a run"
        );

        // Turned off, it stops watching and says so.
        let toggle = debug_center(&mut cx, "metrics-toggle");
        cx.simulate_click(toggle, gpui::Modifiers::none());
        cx.run_until_parked();
        draw(&mut cx);
        assert!(
            !view.read_with(&cx, |view, _| view.watching),
            "the reader can turn the watching off"
        );
        assert!(
            cx.debug_bounds("run-metrics").is_some(),
            "and the row still says what it is doing"
        );
    }

    /// A poll costs a reading a second, and that is only worth paying while
    /// somebody can actually see the row. The window losing focus stops it, and
    /// the window getting focus back starts it again -- underneath the reader's
    /// own toggle, which still means what it always did.
    #[gpui::test]
    async fn the_poll_stops_while_the_window_is_not_looked_at(cx: &mut TestAppContext) {
        let (view, _fs, mut cx) = a_view_of(None, cx).await;

        // The window this row lives in already has focus, the same as the real
        // window does the moment the reader opens it.
        assert!(
            view.read_with(&cx, |view, _| view._watching_task.is_some()),
            "the poll runs while the window has focus"
        );

        cx.deactivate_window();
        assert!(
            view.read_with(&cx, |view, _| view._watching_task.is_none()),
            "losing focus stops it -- nobody left to read the row"
        );

        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        assert!(
            view.read_with(&cx, |view, _| view._watching_task.is_some()),
            "and getting focus back starts it again"
        );

        // The reader's own toggle still works underneath all of this.
        draw(&mut cx);
        let toggle = debug_center(&mut cx, "metrics-toggle");
        cx.simulate_click(toggle, gpui::Modifiers::none());
        cx.run_until_parked();
        assert!(
            view.read_with(&cx, |view, _| !view.watching
                && view._watching_task.is_none()),
            "and turning it off by hand still turns it off, focus or no focus"
        );
    }

    /// The per-window toggle only lasts as long as the window does. The setting
    /// is the one that keeps the row gone for good, and it takes the whole row
    /// with it -- the toggle included, since there is nothing left to toggle.
    #[gpui::test]
    async fn turning_the_row_off_by_setting_paints_none_of_it(cx: &mut TestAppContext) {
        let (view, _fs, mut cx) = a_view_of(None, cx).await;
        draw(&mut cx);
        assert!(
            cx.debug_bounds("run-metrics").is_some(),
            "the row is there by default"
        );
        assert!(
            view.read_with(&cx, |view, _| view._watching_task.is_some()),
            "and its poll is running while the window has focus"
        );

        cx.update(|_, cx| {
            RunConfigurationsSettings::override_global(
                RunConfigurationsSettings {
                    show_process_metrics: false,
                    ..RunConfigurationsSettings::get_global(cx).clone()
                },
                cx,
            );
        });
        cx.run_until_parked();
        draw(&mut cx);

        assert!(
            cx.debug_bounds("run-metrics").is_none(),
            "turned off by setting, none of the row is painted"
        );
        assert!(
            cx.debug_bounds("metrics-toggle").is_none(),
            "not even the toggle that would otherwise turn it back on"
        );
        assert!(
            view.read_with(&cx, |view, _| view._watching_task.is_none()),
            "and the poll behind an invisible row is not worth running either, \
             even though the window still has focus"
        );
    }

    /// A run that has ended leaves its terminal, and its pid, behind. The row
    /// must go back to saying nothing is running rather than keep the pid of a
    /// process that is gone.
    #[gpui::test]
    async fn the_row_lets_go_of_a_run_that_has_ended(cx: &mut TestAppContext) {
        use crate::process_metrics::Sample;

        let (view, _fs, mut cx) = a_view_of(None, cx).await;
        let watched = 4242;
        let running = [Sample {
            pid: watched,
            parent: 1,
            ticks: 10,
            memory: 8 * 1024 * 1024,
            started: 5_000,
        }];
        let at = std::time::Instant::now();

        view.update(&mut cx, |view, _| {
            assert!(view.read_the_run(Some(watched), Some(&running), at));
        });
        draw(&mut cx);
        assert!(
            cx.debug_bounds("run-metrics-reading").is_some(),
            "while it runs, the row shows the reading"
        );

        // The machine is asked again and says nothing about that process.
        let gone = [Sample {
            pid: 1,
            parent: 0,
            ticks: 3,
            memory: 1024,
            started: 1,
        }];
        view.update(&mut cx, |view, _| {
            assert!(
                view.read_the_run(
                    Some(watched),
                    Some(&gone),
                    at + crate::process_metrics::Watcher::HOW_OFTEN
                ),
                "the row changed"
            );
        });
        draw(&mut cx);
        assert!(
            cx.debug_bounds("run-metrics-reading").is_none(),
            "the reading is gone with the run"
        );
        assert!(
            cx.debug_bounds("run-metrics").is_some(),
            "and the row says there is nothing running"
        );
    }

    /// A machine that does not answer is not a run that has ended: an answer with
    /// no processes in it at all leaves the row as it was.
    #[gpui::test]
    async fn a_machine_that_says_nothing_does_not_end_the_run(cx: &mut TestAppContext) {
        use crate::process_metrics::Sample;

        let (view, _fs, mut cx) = a_view_of(None, cx).await;
        let watched = 4242;
        let running = [Sample {
            pid: watched,
            parent: 1,
            ticks: 10,
            memory: 8 * 1024 * 1024,
            started: 5_000,
        }];
        let at = std::time::Instant::now();
        view.update(&mut cx, |view, _| {
            view.read_the_run(Some(watched), Some(&running), at);
            assert!(
                !view.read_the_run(
                    Some(watched),
                    None,
                    at + crate::process_metrics::Watcher::HOW_OFTEN
                ),
                "nothing to report from a reading that did not happen"
            );
        });
        draw(&mut cx);
        assert!(
            cx.debug_bounds("run-metrics-reading").is_some(),
            "the row still shows the run it was showing"
        );
    }

    /// The two settings that decide what the second press of Run does are the
    /// reader's to change, and they land in the file.
    #[gpui::test]
    async fn the_run_toggles_are_written_into_the_file(cx: &mut TestAppContext) {
        let (view, fs, mut cx) = a_view_of(
            Some(r#"[{ "label": "api server", "command": "go run ./cmd/api" }]"#),
            cx,
        )
        .await;
        view.update_in(&mut cx, |view, window, cx| {
            let first = view
                .store
                .read(cx)
                .get(Kind::Task, 0)
                .cloned()
                .expect("the configuration");
            view.show(&first, window, cx);
        });
        cx.run_until_parked();
        draw(&mut cx);

        assert!(
            !view.read_with(&cx, |view, _| view.use_new_terminal),
            "a task says nothing about terminals until somebody says so"
        );

        for switch in [
            "configuration-new-terminal",
            "configuration-several-at-once",
        ] {
            let at = debug_center(&mut cx, switch);
            cx.simulate_click(at, gpui::Modifiers::none());
            cx.run_until_parked();
            draw(&mut cx);
        }
        view.update_in(&mut cx, |view, _window, cx| view.save(cx));
        cx.run_until_parked();

        let written = fs
            .load(path!("/project/.zed/tasks.json").as_ref())
            .await
            .expect("the file is written");
        let read_back = crate::configurations_file::read(Kind::Task, &written);
        let task = read_back.configurations[0]
            .task
            .as_ref()
            .expect("a task the editor reads back");
        assert!(
            task.use_new_terminal,
            "a terminal of its own is in the file"
        );
        assert!(
            task.allow_concurrent_runs,
            "and so is running several at once"
        );
        assert!(
            written.contains("use_new_terminal") && written.contains("allow_concurrent_runs"),
            "written under the names the file uses:\n{written}"
        );
    }

    /// The same view in a window of its own: it can be pulled by every edge and
    /// corner, not only by the one corner the shell's own transparent ring makes
    /// easy to hit, and it is read a size larger than the editor around it.
    #[gpui::test]
    async fn the_window_can_be_pulled_by_every_edge(cx: &mut TestAppContext) {
        let tasks = r#"[{ "label": "api server", "command": "go run ./cmd/api" }]"#;
        let (view, mut window_cx) = a_window_of(tasks, cx).await;
        show_the_first_configuration(&view, &mut window_cx);

        let whole = window_cx
            .debug_bounds("run-configurations-titlebar")
            .expect("the window's own bar is painted");
        for edge in [
            "top",
            "bottom",
            "left",
            "right",
            "top-left",
            "top-right",
            "bottom-left",
            "bottom-right",
        ] {
            let selector: &'static str = match edge {
                "top" => "run-configurations-pull-top",
                "bottom" => "run-configurations-pull-bottom",
                "left" => "run-configurations-pull-left",
                "right" => "run-configurations-pull-right",
                "top-left" => "run-configurations-pull-top-left",
                "top-right" => "run-configurations-pull-top-right",
                "bottom-left" => "run-configurations-pull-bottom-left",
                _ => "run-configurations-pull-bottom-right",
            };
            let band = window_cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("{edge} has a band to pull the window by"));
            assert!(
                band.size.width > px(0.) && band.size.height > px(0.),
                "{edge}'s band has to occupy real screen area: {band:?}"
            );
            assert!(
                band.size.width >= whole.size.width - px(1.)
                    || band.size.height > px(0.) && band.size.width <= px(20.),
                "{edge}'s band is either the width of the window or a narrow strip: \
                 {band:?} against a window {:?} wide",
                whole.size.width
            );
        }
    }

    /// The arguments are typed into a box with room in it. They were typed into a
    /// strip a few pixels tall, with a line number in it and no room for the line
    /// beside it.
    #[gpui::test]
    async fn the_arguments_are_typed_into_a_box_with_room_in_it(cx: &mut TestAppContext) {
        let tasks = r#"[{ "label": "api server", "command": "go run ./cmd/api" }]"#;
        let (view, mut window_cx) = a_window_of(tasks, cx).await;
        show_the_first_configuration(&view, &mut window_cx);

        let arguments = window_cx
            .debug_bounds("configuration-field-ARGUMENTS")
            .expect("the arguments are a field of the form");
        let command = window_cx
            .debug_bounds("configuration-field-COMMAND")
            .expect("and so is the command");
        assert!(
            arguments.size.height >= px(60.),
            "one argument a line needs the room for a few lines: {arguments:?}"
        );
        assert!(
            arguments.size.height > command.size.height * 1.5,
            "and more room than a field that holds one line: {:?} against {:?}",
            arguments.size.height,
            command.size.height
        );
    }

    #[gpui::test]
    async fn the_form_shows_what_it_writes(cx: &mut TestAppContext) {
        let (view, _fs, mut cx) = a_view_of(
            Some(r#"[{ "label": "api server", "command": "go run ./cmd/api" }]"#),
            cx,
        )
        .await;
        view.update_in(&mut cx, |view, window, cx| {
            let first = view
                .store
                .read(cx)
                .get(Kind::Task, 0)
                .cloned()
                .expect("the configuration");
            view.show(&first, window, cx);
        });
        cx.run_until_parked();
        draw(&mut cx);

        assert!(
            cx.debug_bounds("configuration-json").is_none(),
            "hidden until it is asked for"
        );
        let toggle = debug_center(&mut cx, "configuration-json-toggle");
        cx.simulate_click(toggle, gpui::Modifiers::none());
        cx.run_until_parked();
        draw(&mut cx);
        assert!(
            cx.debug_bounds("configuration-json").is_some(),
            "and shown when it is"
        );

        let shown = view.read_with(&cx, |view, cx| view.json_for_test(Kind::Task, cx));
        assert!(
            shown.contains("\"label\": \"api server\"")
                && shown.contains("\"command\": \"go run ./cmd/api\""),
            "what it shows is what lands in the file:\n{shown}"
        );
        assert!(
            !shown.contains("use_new_terminal"),
            "and nothing the entry never set:\n{shown}"
        );
    }

    /// A way run on the spot is nobody else's business until it is pinned, and then
    /// it is in the project's own file like any other.
    #[gpui::test]
    async fn pinning_a_way_run_on_the_spot_writes_it_into_the_file(cx: &mut TestAppContext) {
        let (view, fs, mut cx) = a_view_of(None, cx).await;
        let store = view.read_with(&cx, |view, _| view.store.clone());

        store.update_in(&mut cx, |store, _window, cx| {
            store.remember_temporary(
                task::TaskTemplate {
                    label: "go run ./cmd/api".to_string(),
                    command: "go".to_string(),
                    args: vec!["run".to_string(), "./cmd/api".to_string()],
                    ..task::TaskTemplate::default()
                },
                cx,
            );
        });
        cx.run_until_parked();
        assert_eq!(
            store.read_with(&cx, |store, _| store.temporary().len()),
            1,
            "it is remembered while it is nobody else's business"
        );
        assert!(
            fs.load(path!("/project/.zed/tasks.json").as_ref())
                .await
                .is_err(),
            "and nothing is written to the project for it"
        );

        let pinning = store.update_in(&mut cx, |store, _window, cx| store.pin_temporary(0, cx));
        pinning.await.expect("pinning writes the file");
        cx.run_until_parked();

        let written = fs
            .load(path!("/project/.zed/tasks.json").as_ref())
            .await
            .expect("pinning writes the project's own file");
        let read_back = crate::configurations_file::read(Kind::Task, &written);
        assert_eq!(read_back.configurations.len(), 1);
        assert_eq!(read_back.configurations[0].label, "go run ./cmd/api");
        assert_eq!(
            store.read_with(&cx, |store, _| store.temporary().len()),
            0,
            "and it is no longer one of the temporary ones"
        );
    }

    /// The gutter's run button asks rather than guesses: the window lists the way
    /// the editor found for the line first, then everything the project keeps.
    #[gpui::test]
    async fn the_gutter_offers_every_way_of_running(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".zed": {
                    "tasks.json": r#"[
                      { "label": "unit tests", "command": "go test ./..." }
                    ]"#,
                },
                "cmd": { "api": { "main.go": "package main\n\nfunc main() {}\n" } },
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
        cx.run_until_parked();

        cx.update(|_, cx| {
            cx.set_global(zed_actions::run_configurations::EntryPointOffer {
                language: Some("Go".to_string()),
                file: Some(std::path::PathBuf::from(path!("/project/cmd/api/main.go"))),
                line: 3,
                label: Some("go run ./cmd/api".to_string()),
                command: Some("go".to_string()),
                args: vec!["run".to_string(), ".".to_string()],
                cwd: None,
                env: Default::default(),
            });
        });
        cx.dispatch_action(RunFromEntryPoint);
        cx.run_until_parked();

        let modal = workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_modal::<crate::ways_to_run_modal::WaysToRunModal>(cx)
            })
            .expect("the run button opens the window of ways");
        let ways = modal.read_with(cx, |modal, _| modal.shown_ways());
        assert_eq!(
            ways,
            vec![
                ("go run ./cmd/api".to_string(), true),
                ("unit tests".to_string(), false),
            ],
            "the line's own way comes first, marked as the one that is not in a file yet"
        );

        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(px(1200.), px(800.)),
            |_, _| gpui::div(),
        );
        cx.run_until_parked();
    }

    /// With nothing to choose between, there is nothing to ask: the window that
    /// writes a configuration opens instead.
    #[gpui::test]
    async fn with_no_way_to_run_the_writing_window_opens(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({ "src": { "main.rs": "" } }))
            .await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
        cx.run_until_parked();

        // An offer for a language nothing is known about, so there is no way to run
        // it either.
        cx.update(|_, cx| {
            cx.set_global(zed_actions::run_configurations::EntryPointOffer {
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

        assert!(
            workspace
                .read_with(cx, |workspace, cx| workspace
                    .active_modal::<crate::ways_to_run_modal::WaysToRunModal>(
                    cx
                ))
                .is_none(),
            "there is nothing to choose between"
        );
        assert!(
            cx.update(|_, cx| {
                cx.windows()
                    .into_iter()
                    .any(|window| window.downcast::<RunConfigurationsView>().is_some())
            }),
            "so the window that writes one opens"
        );
    }

    /// The templates the document names are offered where a configuration is
    /// added -- on the plus button -- and picking one both starts the
    /// configuration and fills its command in. They are deliberately not a row
    /// inside the form: by then the reader has already decided what they are
    /// running, and a dropdown there only asks them to decide again.
    #[gpui::test]
    async fn adding_offers_the_templates_and_picking_one_fills_the_command_in(
        cx: &mut TestAppContext,
    ) {
        let (view, _fs, mut cx) = a_view_of(None, cx).await;
        draw(&mut cx);

        let add = cx
            .debug_bounds("ICON-Plus")
            .expect("the toolbar offers a way to add one");
        cx.simulate_click(add.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        draw(&mut cx);

        let go = cx
            .debug_bounds("MENU_ITEM-Go")
            .expect("every template the document names is offered when adding");
        for named in ["MENU_ITEM-Rust", "MENU_ITEM-Makefile", "MENU_ITEM-Mise"] {
            assert!(
                cx.debug_bounds(named).is_some(),
                "{named} is one of them too"
            );
        }
        assert!(
            cx.debug_bounds("MENU_ITEM-Something else").is_some(),
            "and a way to start without one"
        );

        cx.simulate_click(go.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        draw(&mut cx);

        let filled_in = view.read_with(&cx, |view, cx| view.task_in_the_form(cx));
        assert_eq!(filled_in.command, "go", "picking one fills the command in");
        assert_eq!(filled_in.args, vec!["run".to_string(), ".".to_string()]);

        assert!(
            cx.debug_bounds("configuration-template-row").is_none(),
            "and the form itself no longer carries a template row"
        );
    }

    /// An offer is answered once. A later ask that comes from somewhere else -- the
    /// keyboard, say -- must not be filled in from a line the reader looked at long
    /// ago.
    #[gpui::test]
    async fn an_offer_is_only_answered_once(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({ "src": { "main.rs": "" } }))
            .await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        // Through a whole window, because the actions a workspace registers are only
        // listened for there -- which is where the editor's gutter dispatches this
        // one too.
        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let _workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
        cx.run_until_parked();

        cx.update(|_, cx| {
            cx.set_global(zed_actions::run_configurations::EntryPointOffer {
                language: Some("Go".to_string()),
                file: Some(std::path::PathBuf::from(path!("/project/cmd/api/main.go"))),
                line: 3,
                label: Some("go run ./cmd/api".to_string()),
                command: Some("go".to_string()),
                args: vec!["run".to_string(), ".".to_string()],
                cwd: None,
                env: Default::default(),
            });
        });
        cx.dispatch_action(CreateFromEntryPoint);
        cx.run_until_parked();
        let first = cx
            .update(|_, cx| {
                cx.windows()
                    .into_iter()
                    .find_map(|window| window.downcast::<RunConfigurationsView>())
            })
            .expect("the first ask opens the window");
        first
            .update(cx, |_, window, _| window.remove_window())
            .expect("closing it again");
        cx.run_until_parked();

        cx.dispatch_action(CreateFromEntryPoint);
        cx.run_until_parked();
        let asked_again = cx
            .update(|_, cx| {
                cx.windows()
                    .into_iter()
                    .find_map(|window| window.downcast::<RunConfigurationsView>())
            })
            .expect("asking again opens the window")
            .root(cx)
            .expect("the window holds the form");
        let filled_in = asked_again.read_with(cx, |view, cx| view.task_in_the_form(cx));
        assert_eq!(
            filled_in.command, "",
            "with nothing found, the window opens empty rather than repeating what \
             was found before"
        );
    }

    /// The list is written by index, and the file changes underneath it. An entry
    /// that moved is still edited where it is now; one that is gone is not written
    /// over whatever took its place.
    #[gpui::test]
    async fn an_entry_that_moved_in_the_file_is_still_the_one_edited(cx: &mut TestAppContext) {
        let (view, fs, mut cx) = a_view_of(
            Some(
                r#"[
                  { "label": "api server", "command": "go run ./cmd/api" },
                  { "label": "unit tests", "command": "go test ./..." }
                ]"#,
            ),
            cx,
        )
        .await;

        view.update_in(&mut cx, |view, window, cx| {
            let tests = view
                .store
                .read(cx)
                .get(Kind::Task, 1)
                .cloned()
                .expect("the second configuration");
            view.show(&tests, window, cx);
        });
        cx.run_until_parked();

        // Somebody puts another configuration first, so what the view chose is now
        // one further down.
        fs.save(
            path!("/project/.zed/tasks.json").as_ref(),
            &r#"[
                  { "label": "migrations", "command": "make migrate" },
                  { "label": "api server", "command": "go run ./cmd/api" },
                  { "label": "unit tests", "command": "go test ./..." }
                ]"#
            .into(),
            Default::default(),
        )
        .await
        .expect("the file can be written by hand");
        cx.run_until_parked();

        view.update_in(&mut cx, |view, window, cx| {
            view.command.update(cx, |editor, cx| {
                editor.set_text("go test -race ./...", window, cx);
            });
            view.save(cx);
        });
        cx.run_until_parked();

        let written = fs
            .load(path!("/project/.zed/tasks.json").as_ref())
            .await
            .expect("the file is still there");
        let read_back = crate::configurations_file::read(Kind::Task, &written);
        let commands: Vec<_> = read_back
            .configurations
            .iter()
            .map(|configuration| {
                configuration
                    .task
                    .as_ref()
                    .map(|task| task.command.clone())
                    .unwrap_or_default()
            })
            .collect();
        assert_eq!(
            commands,
            vec![
                "make migrate".to_string(),
                "go run ./cmd/api".to_string(),
                "go test -race ./...".to_string(),
            ],
            "the edit lands on the configuration that was chosen, wherever it sits now"
        );
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
            let row = view.an_env_row("PORT", "8080", window, cx);
            view.env_rows.push(row);
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

    /// A Makefile target is opaque: no locator can say what it builds, so the
    /// mockup calls for a Debug button that stays on screen but does nothing --
    /// painted disabled, and unmoved by a click where it sits.
    #[gpui::test]
    async fn the_debug_button_is_withheld_with_a_reason_for_an_opaque_command(
        cx: &mut TestAppContext,
    ) {
        let (view, _fs, mut cx) = a_view_of(
            Some(r#"[{ "label": "build", "command": "make build" }]"#),
            cx,
        )
        .await;
        view.update_in(&mut cx, |view, window, cx| {
            let first = view
                .store
                .read(cx)
                .get(Kind::Task, 0)
                .cloned()
                .expect("the configuration");
            view.show(&first, window, cx);
        });
        cx.run_until_parked();
        draw(&mut cx);

        let reason = view.read_with(&cx, |view, cx| view.why_the_debug_button_is_withheld(cx));
        assert!(
            reason.is_some_and(|reason| !reason.is_empty()),
            "a Makefile target's artifact cannot be worked out, so the button has to say why"
        );

        let before = view.read_with(&cx, |view, cx| {
            (
                view.chosen,
                view.store
                    .read(cx)
                    .of_kind(Kind::Debug)
                    .configurations
                    .len(),
            )
        });
        let at = debug_center(&mut cx, "configuration-debug");
        cx.simulate_click(at, gpui::Modifiers::none());
        cx.run_until_parked();
        draw(&mut cx);
        let after = view.read_with(&cx, |view, cx| {
            (
                view.chosen,
                view.store
                    .read(cx)
                    .of_kind(Kind::Debug)
                    .configurations
                    .len(),
            )
        });
        assert_eq!(
            before, after,
            "the button is disabled, so a click where it is painted must do nothing"
        );
    }

    /// The counterpart of the test above: a command a locator was written for
    /// keeps offering a guess, exactly as it did before the button could be
    /// withheld.
    #[gpui::test]
    async fn the_debug_button_still_offers_a_guess_for_a_command_a_locator_understands(
        cx: &mut TestAppContext,
    ) {
        let (view, _fs, mut cx) = a_view_of(
            Some(r#"[{ "label": "tests", "command": "cargo test" }]"#),
            cx,
        )
        .await;
        view.update_in(&mut cx, |view, window, cx| {
            let first = view
                .store
                .read(cx)
                .get(Kind::Task, 0)
                .cloned()
                .expect("the configuration");
            view.show(&first, window, cx);
        });
        cx.run_until_parked();
        draw(&mut cx);

        assert_eq!(
            view.read_with(&cx, |view, cx| view.why_the_debug_button_is_withheld(cx)),
            None,
            "cargo has a locator, so nothing withholds the button"
        );

        let at = debug_center(&mut cx, "configuration-debug");
        cx.simulate_click(at, gpui::Modifiers::none());
        cx.run_until_parked();
        draw(&mut cx);

        assert_eq!(
            view.read_with(&cx, |view, _| view.chosen.map(|(kind, _)| kind)),
            Some(Kind::Debug),
            "the button is enabled, so pressing it where it is painted offers a debug \
             configuration to save"
        );
    }

    /// The footer's Open button, with a task shown, has to reach for tasks.json
    /// alone -- not the form-embedded pair the old block offered for both files
    /// at once, one of which was never the one being edited.
    #[gpui::test]
    async fn the_footer_open_button_reaches_only_the_task_file_for_a_task(cx: &mut TestAppContext) {
        let (view, fs, mut cx) =
            a_view_of(Some(r#"[{ "label": "one", "command": "true" }]"#), cx).await;
        view.update_in(&mut cx, |view, window, cx| {
            let first = view
                .store
                .read(cx)
                .get(Kind::Task, 0)
                .cloned()
                .expect("the configuration");
            view.show(&first, window, cx);
        });
        cx.run_until_parked();
        draw(&mut cx);

        let at = debug_center(&mut cx, "configuration-open-file");
        cx.simulate_click(at, gpui::Modifiers::none());
        cx.run_until_parked();

        assert!(
            !fs.is_file(path!("/project/.zed/debug.json").as_ref()).await,
            "editing a task must not reach for the debug file"
        );
        let tasks_after = fs
            .load(path!("/project/.zed/tasks.json").as_ref())
            .await
            .expect("opening the task file must not have removed it");
        assert!(
            tasks_after.contains("\"command\": \"true\""),
            "opening the task file must not have rewritten it:\n{tasks_after}"
        );
    }

    /// The counterpart: with a debug configuration shown, the same button has
    /// to reach for debug.json, made if it was not there yet, and leave the
    /// task file alone.
    #[gpui::test]
    async fn the_footer_open_button_reaches_only_the_debug_file_for_a_debug_configuration(
        cx: &mut TestAppContext,
    ) {
        let (view, fs, mut cx) =
            a_view_of(Some(r#"[{ "label": "one", "command": "true" }]"#), cx).await;
        view.update_in(&mut cx, |view, window, cx| {
            view.start_a_new_one(Kind::Debug, window, cx)
        });
        cx.run_until_parked();
        draw(&mut cx);
        assert_eq!(
            view.read_with(&cx, |view, _| view.chosen.map(|(kind, _)| kind)),
            Some(Kind::Debug),
            "a debug configuration is the one shown"
        );

        let at = debug_center(&mut cx, "configuration-open-file");
        cx.simulate_click(at, gpui::Modifiers::none());
        cx.run_until_parked();

        assert!(
            fs.is_file(path!("/project/.zed/debug.json").as_ref()).await,
            "editing a debug configuration must open debug.json, made if it was not there"
        );
        let tasks_after = fs
            .load(path!("/project/.zed/tasks.json").as_ref())
            .await
            .expect("the task file is still there");
        assert!(
            tasks_after.contains("\"command\": \"true\""),
            "opening the debug file must not have touched the task file:\n{tasks_after}"
        );
    }

    /// The window's own Run button used to resolve the task and throw the
    /// result away in silence whenever a variable in it -- like an
    /// `$ZED_...` one nothing supplies -- could not be resolved, the same
    /// silent path the title bar's plaque took. A press that does nothing at
    /// all reads as the press never having landed; the reader has to be told.
    #[gpui::test]
    async fn pressing_run_says_when_the_command_cannot_be_resolved(cx: &mut TestAppContext) {
        let (view, _fs, mut cx) = a_view_of(
            Some(r#"[{ "label": "Run API", "command": "$ZED_CUSTOM_UNKNOWN_VARIABLE" }]"#),
            cx,
        )
        .await;
        show_the_first_configuration(&view, &mut cx);

        let workspace = view
            .read_with(&cx, |view, _| view.workspace.clone())
            .upgrade()
            .expect("the workspace is still open");
        assert!(
            workspace
                .read_with(&cx, |workspace, _| workspace.notification_ids())
                .is_empty(),
            "nothing has been said yet"
        );

        let at = debug_center(&mut cx, "configuration-run");
        cx.simulate_click(at, gpui::Modifiers::none());
        cx.run_until_parked();

        assert!(
            !workspace
                .read_with(&cx, |workspace, _| workspace.notification_ids())
                .is_empty(),
            "pressing Run on a command that could not be resolved has to say \
             so -- doing nothing looks exactly like the press never landed"
        );
    }

    /// A configuration kept in the project names `$ZED_WORKTREE_ROOT`, and the
    /// item the reader happens to have open often knows nothing about the
    /// project -- an unsaved buffer, a request in the API client, this window
    /// itself. Resolving against that item's context alone fails, and a failed
    /// resolution is a press that does nothing.
    #[test]
    fn a_configuration_of_the_project_resolves_whatever_is_open() {
        let mut contexts = project::TaskContexts::default();

        let mut what_is_open = task::TaskContext::default();
        what_is_open
            .task_variables
            .insert(task::VariableName::File, "/nowhere/scratch".to_string());
        contexts.active_item_context = Some((None, None, what_is_open));

        let mut the_project = task::TaskContext::default();
        the_project.task_variables.insert(
            task::VariableName::WorktreeRoot,
            "/projects/thing".to_string(),
        );
        contexts.active_worktree_context = Some((project::WorktreeId::from_usize(0), the_project));

        let configuration = TaskTemplate {
            label: "Run API".to_string(),
            command: "/bin/echo".to_string(),
            cwd: Some("$ZED_WORKTREE_ROOT".to_string()),
            ..TaskTemplate::default()
        };

        // The control: without this, the test would pass whether or not the
        // project's variables are filled in, and prove nothing.
        assert!(
            configuration
                .resolve_task(
                    "test",
                    &contexts.active_context().cloned().unwrap_or_default()
                )
                .is_none(),
            "the open item alone was expected to be missing the project's root"
        );

        let resolved = configuration
            .resolve_task("test", &what_to_resolve_against(&contexts))
            .expect("a configuration of the project resolves against the project");
        assert_eq!(
            resolved.resolved.cwd.as_deref(),
            Some(std::path::Path::new("/projects/thing"))
        );
    }

    /// What the open item knows is the more specific answer and has to win; the
    /// project only fills in what the item leaves unsaid.
    #[test]
    fn what_is_open_wins_over_the_project() {
        let mut contexts = project::TaskContexts::default();

        let mut what_is_open = task::TaskContext::default();
        what_is_open.task_variables.insert(
            task::VariableName::WorktreeRoot,
            "/projects/the-one-in-front".to_string(),
        );
        contexts.active_item_context = Some((None, None, what_is_open));

        let mut the_project = task::TaskContext::default();
        the_project.task_variables.insert(
            task::VariableName::WorktreeRoot,
            "/projects/some-other".to_string(),
        );
        contexts.active_worktree_context = Some((project::WorktreeId::from_usize(0), the_project));

        let context = what_to_resolve_against(&contexts);
        assert_eq!(
            context
                .task_variables
                .get(&task::VariableName::WorktreeRoot),
            Some("/projects/the-one-in-front")
        );
    }

    /// The point of the moment of adding: what the project itself says can be
    /// run is offered whole, so a reader who has never seen the command types
    /// nothing at all.
    #[gpui::test]
    async fn adding_offers_what_the_project_itself_says_can_be_run(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".zed": { "tasks.json": "[]" },
                ".env.local": "TOKEN=1",
                "cmd": { "api": { "main.go": "package main\n\nfunc main() {}\n" } },
            }),
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

        let (found, env_files) = view.read_with(cx, |view, _| {
            (
                view.found
                    .iter()
                    .map(|point| point.name.clone())
                    .collect::<Vec<_>>(),
                view.env_files
                    .iter()
                    .map(|path| path.to_string_lossy().into_owned())
                    .collect::<Vec<_>>(),
            )
        });
        assert_eq!(
            found,
            vec!["./cmd/api".to_string()],
            "the Go program in the project is a way of running it"
        );
        assert_eq!(env_files, vec![".env.local".to_string()]);

        // Picking it fills the command in whole, which is what "nothing to type"
        // means: the command, its package and the directory it runs in.
        let point = view.read_with(cx, |view, _| view.found[0].clone());
        view.update_in(cx, |view, window, cx| {
            view.start_a_new_one(Kind::Task, window, cx);
            view.fill_in_from_way(&point, window, cx);
        });
        cx.run_until_parked();

        let (command, args, cwd) = view.read_with(cx, |view, cx| {
            (
                view.command.read(cx).text(cx),
                view.args.read(cx).text(cx),
                view.cwd.read(cx).text(cx),
            )
        });
        assert_eq!(command, "go");
        assert_eq!(args, "run\n./cmd/api");
        assert_eq!(cwd, "$ZED_WORKTREE_ROOT");
    }
}
