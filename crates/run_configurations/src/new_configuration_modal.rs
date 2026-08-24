use std::path::Path;

use collections::HashMap;
use editor::Editor;
use gpui::{
    AnyElement, App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    SharedString, WeakEntity, Window,
};
use task::TaskTemplate;
use ui::{ElevationIndex, prelude::*};
use workspace::{ModalView, Workspace};
use zed_actions::run_configurations::EntryPointOffer;

use crate::configurations_file::{self, Kind};
use crate::configurations_store::ConfigurationsStore;

/// What an entry point is run with.
#[derive(Debug, Default, PartialEq)]
pub struct HowToRun {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    /// What the command needs in its environment. Only a compiled one-off has
    /// anything here: it reads the source file's path from a variable rather than
    /// from the command, so no file name can end the command and start another.
    pub env: HashMap<String, String>,
}

/// What an entry point of this language is usually run with.
///
/// Only a first offer. The editor's own task for the line wins over this when
/// there is one, and whatever the reader saves into the file wins over both.
pub fn defaults_for(language: Option<&str>, file: Option<&Path>) -> HowToRun {
    let name = file
        .and_then(|file| file.file_name())
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    // The directory the file sits in, which is the package for languages that
    // have packages.
    let here = Some("${ZED_DIRNAME}".to_string());

    let plain = |command: &str, args: Vec<String>, cwd: Option<String>| HowToRun {
        command: command.to_string(),
        args,
        cwd,
        env: HashMap::default(),
    };

    match language.unwrap_or_default() {
        "Go" => plain("go", vec!["run".into(), ".".into()], here),
        "Rust" => plain("cargo", vec!["run".into()], None),
        "Python" => plain("python3", vec![name], here),
        "TypeScript" | "TSX" => plain("npx", vec!["tsx".into(), name], here),
        "JavaScript" | "JSX" => plain("node", vec![name], here),
        "PHP" => plain("php", vec![name], here),
        "Ruby" => plain("ruby", vec![name], here),
        "C" => compiled_with("cc", here),
        "C++" => compiled_with("c++", here),
        // Nothing known: the reader fills it in, and the fields say so rather than
        // pretending to a command that would fail.
        _ => HowToRun::default(),
    }
}

/// A one-off build and run, for a language that has to be compiled first. Built
/// where `task::compiled_one_off` puts one, which is where the gutter's own play
/// button builds it too.
fn compiled_with(compiler: &str, here: Option<String>) -> HowToRun {
    let (command, args, env) = task::compiled_one_off::build_and_run(compiler);
    HowToRun {
        command,
        args,
        cwd: here,
        env,
    }
}

/// The configuration an offer becomes: the editor's own task when it has one,
/// otherwise what the language is usually run with.
pub fn task_from(offer: &EntryPointOffer) -> TaskTemplate {
    let how = match offer.command.as_deref() {
        Some(command) if !command.trim().is_empty() => HowToRun {
            command: command.to_string(),
            args: offer.args.clone(),
            cwd: offer.cwd.clone(),
            env: what_the_file_settles(&offer.env, offer.file.as_deref()),
        },
        _ => defaults_for(offer.language.as_deref(), offer.file.as_deref()),
    };
    let named_after = offer
        .file
        .as_deref()
        .and_then(|file| file.file_stem())
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_else(|| "program".to_string());
    TaskTemplate {
        label: offer
            .label
            .as_deref()
            .and_then(|label| as_a_reader_reads_it(label, offer.file.as_deref()))
            .unwrap_or_else(|| format!("run {named_after}")),
        command: how.command,
        args: how.args,
        cwd: how.cwd,
        env: how.env,
        ..TaskTemplate::default()
    }
}

/// The task's environment with what this file settles already filled in.
///
/// A run started from here is a one-off: it is not the editor's own task any
/// more, so nothing is going to fill in `$ZED_FILE` for it later. What a file
/// alone cannot settle is left as it stands, for whoever runs it.
fn what_the_file_settles(
    env: &std::collections::HashMap<String, String>,
    file: Option<&Path>,
) -> HashMap<String, String> {
    let asked: HashMap<String, String> = env
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    task::substitute_variables_in_map(&asked, &what_is_known_of(file)).unwrap_or(asked)
}

/// What a file alone says, for filling in a label or a value with no project to
/// ask.
fn what_is_known_of(file: Option<&Path>) -> task::TaskContext {
    let mut variables = task::TaskVariables::default();
    if let Some(file) = file {
        variables.insert(task::VariableName::File, file.to_string_lossy().to_string());
        if let Some(name) = file.file_name() {
            variables.insert(
                task::VariableName::Filename,
                name.to_string_lossy().to_string(),
            );
        }
        if let Some(stem) = file.file_stem() {
            variables.insert(task::VariableName::Stem, stem.to_string_lossy().to_string());
        }
        if let Some(folder) = file.parent() {
            variables.insert(
                task::VariableName::Dirname,
                folder.to_string_lossy().to_string(),
            );
        }
    }
    task::TaskContext {
        cwd: None,
        task_variables: variables,
        project_env: std::collections::HashMap::default(),
    }
}

/// A label as a reader reads it. The label a language gives a runnable names
/// variables -- `run $ZED_STEM` -- and a window that shows that says less than one
/// that shows `run hello`, so what the file settles is put in.
///
/// Nothing comes back when a variable is left that only the language itself could
/// settle, such as which Go package the line is in: the caller has a name made
/// from the file for that, which is at least something a reader can read.
fn as_a_reader_reads_it(label: &str, file: Option<&Path>) -> Option<String> {
    if !label.contains('$') {
        return Some(label.to_string());
    }
    let said = task::substitute_variables_in_str(label, &what_is_known_of(file))?;
    (!said.contains('$')).then_some(said)
}

/// The window that opens when the reader asks the gutter for a run configuration:
/// the fields already filled in, for looking over rather than typing out.
pub struct NewConfigurationModal {
    focus: FocusHandle,
    store: Entity<ConfigurationsStore>,
    workspace: WeakEntity<Workspace>,
    pub label: Entity<Editor>,
    pub command: Entity<Editor>,
    pub args: Entity<Editor>,
    pub cwd: Entity<Editor>,
    pub env_file: Entity<Editor>,
    /// A terminal of its own for every run, rather than the last one reused.
    use_new_terminal: bool,
    /// Several runs of this configuration at once, rather than the running one
    /// being replaced.
    several_at_once: bool,
    found_at: Option<String>,
    trouble: Option<SharedString>,
}

impl EventEmitter<DismissEvent> for NewConfigurationModal {}
impl ModalView for NewConfigurationModal {}

impl Focusable for NewConfigurationModal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl NewConfigurationModal {
    pub fn new(
        offer: EntryPointOffer,
        store: Entity<ConfigurationsStore>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let task = task_from(&offer);
        let field = |text: String,
                     placeholder: &'static str,
                     window: &mut Window,
                     cx: &mut Context<Self>| {
            cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_placeholder_text(placeholder, window, cx);
                editor.set_text(text, window, cx);
                editor
            })
        };
        let lines = |text: String,
                     placeholder: &'static str,
                     window: &mut Window,
                     cx: &mut Context<Self>| {
            cx.new(|cx| {
                let mut editor = Editor::multi_line(window, cx);
                editor.set_placeholder_text(placeholder, window, cx);
                editor.set_text(text, window, cx);
                editor
            })
        };

        let found_at = offer.file.as_deref().map(|file| {
            format!(
                "{} line {}",
                file.file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_else(|| file.display().to_string()),
                offer.line
            )
        });

        Self {
            focus: cx.focus_handle(),
            store,
            workspace,
            label: field(task.label.clone(), "What to call it", window, cx),
            command: field(task.command.clone(), "The command to run", window, cx),
            args: lines(task.args.join("\n"), "One argument a line", window, cx),
            cwd: field(
                task.cwd.clone().unwrap_or_default(),
                "Where to run it, blank for the project root",
                window,
                cx,
            ),
            env_file: field(
                String::new(),
                "A file of variables, such as .env.local",
                window,
                cx,
            ),
            use_new_terminal: task.use_new_terminal,
            several_at_once: task.allow_concurrent_runs,
            found_at,
            trouble: None,
        }
    }

    /// What the fields say.
    pub fn task(&self, cx: &App) -> TaskTemplate {
        let text = |editor: &Entity<Editor>| editor.read(cx).text(cx).trim().to_string();
        let cwd = text(&self.cwd);
        let env_file = text(&self.env_file);
        TaskTemplate {
            label: text(&self.label),
            command: text(&self.command),
            args: text(&self.args)
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect(),
            cwd: (!cwd.is_empty()).then_some(cwd),
            env_file: (!env_file.is_empty()).then_some(env_file),
            use_new_terminal: self.use_new_terminal,
            allow_concurrent_runs: self.several_at_once,
            ..TaskTemplate::default()
        }
    }

    /// Writes it into the project's `tasks.json`. Nothing is remembered anywhere
    /// else: the file is where configurations live.
    pub fn save(&mut self, and_run: bool, window: &mut Window, cx: &mut Context<Self>) {
        let task = self.task(cx);
        if task.label.trim().is_empty() || task.command.trim().is_empty() {
            self.trouble = Some("It needs a name and a command before it can be saved.".into());
            cx.notify();
            return;
        }
        let entry = match configurations_file::task_as_written(&task) {
            Ok(entry) => entry,
            Err(error) => {
                self.trouble = Some(format!("{error:#}").into());
                cx.notify();
                return;
            }
        };
        let writing = self.store.read(cx).save(Kind::Task, None, entry, cx);
        let workspace = self.workspace.clone();
        let run_it = and_run.then(|| task.clone());
        cx.spawn_in(window, async move |modal, cx| match writing.await {
            Ok(()) => {
                if let Some(task) = run_it {
                    crate::configurations_view::run_a_task(&workspace, task, cx).await;
                }
                modal.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
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

    fn where_it_goes(&self, cx: &App) -> String {
        self.store
            .read(cx)
            .file_path(Kind::Task)
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "no folder is open".to_string())
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
                .on_click(cx.listener(move |modal, _, _window, cx| toggle(modal, cx)))
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
                "new-configuration-new-terminal",
                "A terminal of its own",
                "every run opens one rather than reusing the last",
                self.use_new_terminal,
                cx,
                |modal, cx| {
                    modal.use_new_terminal = !modal.use_new_terminal;
                    cx.notify();
                },
            ))
            .child(switch(
                "new-configuration-several-at-once",
                "Several at once",
                "a new run leaves the running one alone",
                self.several_at_once,
                cx,
                |modal, cx| {
                    modal.several_at_once = !modal.several_at_once;
                    cx.notify();
                },
            ))
            .into_any_element()
    }
}

impl Render for NewConfigurationModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let field = |name: &'static str, editor: &Entity<Editor>, tall: bool| {
            v_flex()
                .w_full()
                .gap_1()
                .child(Label::new(name).size(LabelSize::XSmall).color(Color::Muted))
                .child(
                    div()
                        .w_full()
                        .when(!tall, |field| field.h(px(28.)))
                        .when(tall, |field| field.h(px(64.)))
                        .px_2()
                        .py_1()
                        .border_1()
                        .border_color(ui::cyberpunk::border_dim())
                        .child(editor.clone()),
                )
        };

        v_flex()
            .key_context("NewRunConfiguration")
            .track_focus(&self.focus)
            .w(px(560.))
            .p_3()
            .gap_3()
            .elevation_3(cx)
            .shadow(ElevationIndex::ModalSurface.shadow(cx))
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .on_action(
                cx.listener(|modal, _: &menu::Confirm, window, cx| modal.save(false, window, cx)),
            )
            .child(
                v_flex()
                    .gap_0p5()
                    .child(Label::new("A run configuration for this entry point"))
                    .children(self.found_at.clone().map(|found_at| {
                        Label::new(format!("Found at {found_at}"))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                    })),
            )
            .children(self.trouble.clone().map(|trouble| {
                Label::new(trouble)
                    .size(LabelSize::Small)
                    .color(Color::Error)
            }))
            .child(field("Name", &self.label, false))
            .child(field("Command", &self.command, false))
            .child(field("Arguments", &self.args, true))
            .child(field("Working directory", &self.cwd, false))
            .child(field("Environment file", &self.env_file, false))
            .child(self.render_run_toggles(cx))
            .child(
                Label::new(format!("It will be written to {}", self.where_it_goes(cx)))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("new-configuration-cancel", "Cancel")
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        Button::new("new-configuration-save", "Save")
                            .label_size(LabelSize::Small)
                            .on_click(
                                cx.listener(|modal, _, window, cx| modal.save(false, window, cx)),
                            ),
                    )
                    .child(
                        Button::new("new-configuration-save-and-run", "Save and run")
                            .label_size(LabelSize::Small)
                            .style(ButtonStyle::Filled)
                            .on_click(
                                cx.listener(|modal, _, window, cx| modal.save(true, window, cx)),
                            ),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::Fs as _;
    use gpui::{TestAppContext, VisualTestContext};
    use std::path::PathBuf;
    use util::path;

    #[test]
    fn each_language_is_offered_the_way_it_is_usually_run() {
        for (language, file, expected_command, expected_first_argument) in [
            ("Go", "cmd/api/main.go", "go", Some("run")),
            ("Rust", "src/main.rs", "cargo", Some("run")),
            ("Python", "tools/report.py", "python3", Some("report.py")),
            ("JavaScript", "server.js", "node", Some("server.js")),
            ("TypeScript", "server.ts", "npx", Some("tsx")),
            ("PHP", "index.php", "php", Some("index.php")),
            ("C", "main.c", "sh", Some("-c")),
            ("C++", "main.cpp", "sh", Some("-c")),
            ("Brainfuck", "thing.bf", "", None),
        ] {
            let how = defaults_for(Some(language), Some(Path::new(file)));

            assert_eq!(
                how.command, expected_command,
                "{language} is usually run with {expected_command}"
            );
            assert_eq!(
                how.args.first().map(String::as_str),
                expected_first_argument,
                "{language} arguments came out as {:?}",
                how.args
            );
        }
    }

    /// A compiled one-off is built outside every worktree, and reads the file it
    /// builds from the environment. A build directory in the project would show up
    /// in the tree, in search and in go-to-file.
    #[test]
    fn a_compiled_one_off_is_built_outside_the_project() {
        let how = defaults_for(Some("C"), Some(Path::new("/projects/thing/main.c")));
        let line = how.args.join(" ");

        assert!(
            line.contains("$RUN_CONFIGURATION_CACHE_ROOT"),
            "the binary goes to the editor's own cache, read by name so no path is \
             spliced into the command: {line}"
        );
        assert!(
            !line.contains("target/"),
            "nothing is written into the project: {line}"
        );
        assert!(
            !line.contains("projects/thing"),
            "and no name reaches the command: {line}"
        );
        assert_eq!(
            how.env
                .get(task::compiled_one_off::SOURCE)
                .map(String::as_str),
            Some("$ZED_FILE"),
            "the file it builds is asked for by name, and read from the environment"
        );
    }

    /// What the editor already runs for the line beats any guess: it is known to
    /// work.
    /// A task may keep in its environment something its command cannot do
    /// without -- the file it is to build, for one -- and a run started from here
    /// is a one-off, so nothing will fill that in later.
    #[test]
    fn what_a_task_needs_in_its_environment_comes_with_it() {
        let mut env = std::collections::HashMap::default();
        env.insert(
            task::compiled_one_off::SOURCE.to_string(),
            "$ZED_FILE".to_string(),
        );
        env.insert("KEPT".to_string(), "as it is".to_string());
        let offer = EntryPointOffer {
            language: Some("C".to_string()),
            file: Some(PathBuf::from("/projects/thing/src/hello.c")),
            line: 3,
            label: Some("run $ZED_STEM".to_string()),
            command: Some("sh".to_string()),
            args: vec!["-c".to_string(), "true".to_string()],
            cwd: None,
            env,
        };

        let task = task_from(&offer);

        assert_eq!(
            task.env
                .get(task::compiled_one_off::SOURCE)
                .map(String::as_str),
            Some("/projects/thing/src/hello.c"),
            "the file is settled here, since nothing else is going to settle it"
        );
        assert_eq!(
            task.env.get("KEPT").map(String::as_str),
            Some("as it is"),
            "and everything else comes through as it stands"
        );
    }

    /// The label a language gives a runnable names variables. A window showing
    /// `run $ZED_STEM` says less than one showing the file's own name.
    #[test]
    fn a_label_naming_the_file_is_read_as_the_file() {
        let offer = EntryPointOffer {
            language: Some("C".to_string()),
            file: Some(PathBuf::from("/projects/thing/src/hello.c")),
            line: 3,
            label: Some("run $ZED_STEM".to_string()),
            command: Some("sh".to_string()),
            args: vec!["-c".to_string(), "true".to_string()],
            cwd: None,
            env: Default::default(),
        };

        assert_eq!(task_from(&offer).label, "run hello");
    }

    /// And a variable only the language itself could settle is not left in the
    /// label for the reader to puzzle over.
    #[test]
    fn a_label_nothing_here_can_settle_is_named_after_the_file() {
        let offer = EntryPointOffer {
            language: Some("Go".to_string()),
            file: Some(PathBuf::from("/projects/thing/cmd/api/main.go")),
            line: 9,
            label: Some("go run $ZED_CUSTOM_GO_PACKAGE".to_string()),
            command: Some("go".to_string()),
            args: vec!["run".to_string(), ".".to_string()],
            cwd: None,
            env: Default::default(),
        };

        assert_eq!(task_from(&offer).label, "run main");
    }

    #[test]
    fn the_editors_own_task_is_preferred_to_a_guess() {
        let offer = EntryPointOffer {
            language: Some("Go".to_string()),
            file: Some(PathBuf::from("/projects/thing/cmd/api/main.go")),
            line: 35,
            label: Some("go run ./cmd/api".to_string()),
            command: Some("go".to_string()),
            args: vec!["run".to_string(), "./cmd/api".to_string()],
            cwd: Some("/projects/thing".to_string()),
            env: Default::default(),
        };

        let task = task_from(&offer);

        assert_eq!(task.label, "go run ./cmd/api");
        assert_eq!(task.args, vec!["run".to_string(), "./cmd/api".to_string()]);
        assert_eq!(task.cwd.as_deref(), Some("/projects/thing"));
    }

    #[test]
    fn an_offer_with_no_task_of_its_own_is_filled_in_from_the_language() {
        let offer = EntryPointOffer {
            language: Some("Python".to_string()),
            file: Some(PathBuf::from("/projects/thing/tools/report.py")),
            line: 12,
            ..EntryPointOffer::default()
        };

        let task = task_from(&offer);

        assert_eq!(task.command, "python3");
        assert_eq!(task.args, vec!["report.py".to_string()]);
        assert_eq!(
            task.label, "run report",
            "and it is named after the file, so the list reads as something"
        );
    }

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

    /// A window opened the way the gutter opens it: through the workspace action,
    /// with the offer left in the global the editor uses to hand one over.
    async fn a_modal_over(
        offer: EntryPointOffer,
        cx: &mut TestAppContext,
    ) -> (
        Entity<NewConfigurationModal>,
        std::sync::Arc<project::FakeFs>,
        VisualTestContext,
    ) {
        init_test(cx);
        let fs = project::FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            serde_json::json!({ "cmd": { "api": { "main.go": "package main\n\nfunc main() {}\n" } } }),
        )
        .await;
        let project = project::Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        // Through a whole window, because the action the gutter dispatches is only
        // listened for there.
        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
        cx.run_until_parked();

        cx.update(|_, cx| cx.set_global(offer));
        cx.dispatch_action(crate::CreateFromEntryPoint);
        cx.run_until_parked();

        let modal = workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_modal::<NewConfigurationModal>(cx)
            })
            .expect("asking the gutter opens the window");
        (modal, fs, cx.clone())
    }

    fn an_offer() -> EntryPointOffer {
        EntryPointOffer {
            language: Some("Go".to_string()),
            file: Some(PathBuf::from(path!("/project/cmd/api/main.go"))),
            line: 3,
            label: Some("go run ./cmd/api".to_string()),
            command: Some("go".to_string()),
            args: vec!["run".to_string(), ".".to_string()],
            cwd: Some("${ZED_DIRNAME}".to_string()),
            env: Default::default(),
        }
    }

    /// The window offers the same two toggles the "All configurations" window
    /// does, and clicking them here reaches the file exactly the same way.
    #[gpui::test]
    async fn the_run_toggles_are_written_into_the_file(cx: &mut TestAppContext) {
        let (modal, fs, mut cx) = a_modal_over(an_offer(), cx).await;
        draw(&mut cx);

        assert!(
            !modal.read_with(&cx, |modal, _| modal.use_new_terminal),
            "a fresh configuration says nothing about terminals until somebody says so"
        );
        assert!(
            !modal.read_with(&cx, |modal, _| modal.several_at_once),
            "nor about running several at once"
        );

        for switch in [
            "new-configuration-new-terminal",
            "new-configuration-several-at-once",
        ] {
            let at = debug_center(&mut cx, switch);
            cx.simulate_click(at, gpui::Modifiers::none());
            cx.run_until_parked();
            draw(&mut cx);
        }
        modal.update_in(&mut cx, |modal, window, cx| modal.save(false, window, cx));
        cx.run_until_parked();

        let written = fs
            .load(path!("/project/.zed/tasks.json").as_ref())
            .await
            .expect("saving writes the project's own file");
        let read_back = configurations_file::read(Kind::Task, &written);
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

    /// A configuration nobody touched the toggles on is written the same as any
    /// other task with nothing said about how it runs: neither key in the file.
    #[gpui::test]
    async fn leaving_the_toggles_untouched_keeps_the_defaults(cx: &mut TestAppContext) {
        let (modal, fs, mut cx) = a_modal_over(an_offer(), cx).await;
        draw(&mut cx);

        modal.update_in(&mut cx, |modal, window, cx| modal.save(false, window, cx));
        cx.run_until_parked();

        let written = fs
            .load(path!("/project/.zed/tasks.json").as_ref())
            .await
            .expect("saving writes the project's own file");
        assert!(
            !written.contains("use_new_terminal") && !written.contains("allow_concurrent_runs"),
            "an untouched configuration says nothing about either toggle:\n{written}"
        );
        let read_back = configurations_file::read(Kind::Task, &written);
        let task = read_back.configurations[0]
            .task
            .as_ref()
            .expect("a task the editor reads back");
        assert!(!task.use_new_terminal);
        assert!(!task.allow_concurrent_runs);
    }
}
