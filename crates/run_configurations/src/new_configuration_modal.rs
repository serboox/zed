use std::path::Path;

use editor::Editor;
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, SharedString,
    WeakEntity, Window,
};
use sha2::{Digest as _, Sha256};
use task::TaskTemplate;
use ui::{ElevationIndex, prelude::*};
use workspace::{ModalView, Workspace};
use zed_actions::run_configurations::EntryPointOffer;

use crate::configurations_file::{self, Kind};
use crate::configurations_store::ConfigurationsStore;

/// The editor's own cache, where a compiled one-off is put.
///
/// Not inside the project. A build directory there shows up in the file tree, in
/// search and in go-to-file, for a file nobody asked to see; the cache is outside
/// every worktree, so none of that happens and no `.gitignore` is needed either.
///
/// The shell is the one that works the path out, when the run starts, so what is
/// written into the project's file is the same on every machine. This is the only
/// part of the line the shell is asked to work out, and it holds no name that came
/// from a file or a folder -- a path with a quote or a backtick in it would
/// otherwise end the command and start one nobody asked for.
pub const WHERE_A_BUILT_BINARY_GOES: &str = r#""${XDG_CACHE_HOME-$HOME/.cache}/zed/run"#;

/// What an entry point of this language is usually run with: the command, its
/// arguments, and where to run it from.
///
/// Only a first offer. The editor's own task for the line wins over this when
/// there is one, and whatever the reader saves into the file wins over both.
pub fn defaults_for(
    language: Option<&str>,
    file: Option<&Path>,
) -> (String, Vec<String>, Option<String>) {
    let name = file
        .and_then(|file| file.file_name())
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    let stem = file
        .and_then(|file| file.file_stem())
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_else(|| "program".to_string());
    // The directory the file sits in, which is the package for languages that
    // have packages.
    let here = Some("${ZED_DIRNAME}".to_string());

    match language.unwrap_or_default() {
        "Go" => ("go".into(), vec!["run".into(), ".".into()], here),
        "Rust" => ("cargo".into(), vec!["run".into()], None),
        "Python" => ("python3".into(), vec![name], here),
        "TypeScript" | "TSX" => ("npx".into(), vec!["tsx".into(), name], here),
        "JavaScript" | "JSX" => ("node".into(), vec![name], here),
        "PHP" => ("php".into(), vec![name], here),
        "Ruby" => ("ruby".into(), vec![name], here),
        "C" => compiled_with("cc", file, &name, &stem, here),
        "C++" => compiled_with("c++", file, &name, &stem, here),
        // Nothing known: the reader fills it in, and the fields say so rather than
        // pretending to a command that would fail.
        _ => (String::new(), Vec::new(), None),
    }
}

/// A one-off build and run, for a language that has to be compiled first.
///
/// This goes through a shell, so the file names are quoted: a space or a quote in
/// a path would otherwise split the command into pieces, or run something else
/// entirely.
fn compiled_with(
    compiler: &str,
    file: Option<&Path>,
    name: &str,
    stem: &str,
    here: Option<String>,
) -> (String, Vec<String>, Option<String>) {
    // `$out` is the shell's own variable: the editor leaves a name it does not
    // know alone, and the shell has it by the time the line runs. Two words with
    // nothing between them are one word, so the folder keeps its double quotes
    // and the file name its single ones.
    (
        "sh".into(),
        vec![
            "-c".into(),
            format!(
                r#"out={folder}; mkdir -p "$out" && {compiler} {name} -o "$out"{binary} && "$out"{binary}"#,
                folder = built_in(file),
                name = as_one_shell_word(name),
                binary = as_one_shell_word(&format!("/{stem}")),
            ),
        ],
        here,
    )
}

/// The folder a one-off is built into: the cache, and under it a name of this
/// source file alone.
///
/// The name is a digest of the path rather than the path itself, so two files
/// called `foo.c` in one project do not build over each other, and so nothing a
/// folder is called can reach the shell.
fn built_in(file: Option<&Path>) -> String {
    let of = file
        .map(|file| file.to_string_lossy().to_string())
        .unwrap_or_default();
    let digest = Sha256::digest(of.as_bytes());
    let named: String = digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{WHERE_A_BUILT_BINARY_GOES}/{named}\"")
}

/// `value` as one word for the shell, whatever it holds.
fn as_one_shell_word(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// The configuration an offer becomes: the editor's own task when it has one,
/// otherwise what the language is usually run with.
pub fn task_from(offer: &EntryPointOffer) -> TaskTemplate {
    let (command, args, cwd) = match offer.command.as_deref() {
        Some(command) if !command.trim().is_empty() => {
            (command.to_string(), offer.args.clone(), offer.cwd.clone())
        }
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
            .clone()
            .unwrap_or_else(|| format!("run {named_after}")),
        command,
        args,
        cwd,
        ..TaskTemplate::default()
    }
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
    use std::path::PathBuf;

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
            let (command, args, _) = defaults_for(Some(language), Some(Path::new(file)));

            assert_eq!(
                command, expected_command,
                "{language} is usually run with {expected_command}"
            );
            assert_eq!(
                args.first().map(String::as_str),
                expected_first_argument,
                "{language} arguments came out as {args:?}"
            );
        }
    }

    /// A compiled one-off is built outside every worktree. A build directory in
    /// the project would show up in the tree, in search and in go-to-file.
    #[test]
    fn a_compiled_one_off_is_built_outside_the_project() {
        let (_, args, _) = defaults_for(Some("C"), Some(Path::new("/projects/thing/main.c")));
        let line = args.join(" ");

        assert!(
            line.contains(r#""${XDG_CACHE_HOME-$HOME/.cache}/zed/run/"#),
            "the binary goes to the editor's cache, wherever the shell says that is: {line}"
        );
        assert!(
            !line.contains("target/"),
            "nothing is written into the project: {line}"
        );
        assert!(
            !line.contains("/tmp"),
            "nor into the system's temporary directory, which is emptied under us: {line}"
        );
        assert!(
            !line.contains("projects/thing"),
            "and no folder's name reaches the shell: {line}"
        );
    }

    /// Two files of one name in one project are two configurations, and one must
    /// not build over the other.
    #[test]
    fn two_files_of_one_name_are_built_apart() {
        let one = built_in(Some(Path::new("/projects/thing/src/foo.c")));
        let other = built_in(Some(Path::new("/projects/thing/tests/foo.c")));
        assert_ne!(one, other, "each source file gets its own folder");
        assert_eq!(
            one,
            built_in(Some(Path::new("/projects/thing/src/foo.c"))),
            "and the same file gets the same one every time, so a build can be reused"
        );
    }

    /// The line is written into the project's file as it stands, and the editor
    /// puts its own variables in before running. What the shell has to work out
    /// must come through that untouched, and nothing the editor fills in may end
    /// up inside the command.
    #[test]
    fn the_editor_leaves_the_shells_own_work_alone() {
        let offer = EntryPointOffer {
            language: Some("C".to_string()),
            file: Some(PathBuf::from("/projects/thing/main.c")),
            line: 1,
            label: Some("run main".to_string()),
            command: None,
            args: Vec::new(),
            cwd: None,
        };
        let mut variables = task::TaskVariables::default();
        variables.insert(
            task::VariableName::WorktreeRoot,
            "/projects/thing".to_string(),
        );
        variables.insert(task::VariableName::Dirname, "/projects/thing".to_string());
        let context = task::TaskContext {
            cwd: None,
            task_variables: variables,
            project_env: std::collections::HashMap::default(),
        };

        let resolved = task_from(&offer)
            .resolve_task("test", &context)
            .expect("the editor can make a run out of it");
        let line = resolved.resolved.args.join(" ");

        assert!(
            line.contains("${XDG_CACHE_HOME-$HOME/.cache}"),
            "the editor leaves the shell's own work alone: {line}"
        );
        assert!(
            !line.contains("projects/thing"),
            "and puts nothing of its own inside the command: {line}"
        );
    }

    /// The whole line, run for real, with a compiler of our own so nothing has to
    /// be installed: the program has to land in the cache, run from there, and
    /// leave the project as it was -- for a file name made of everything a shell
    /// reads.
    #[test]
    fn the_line_builds_into_the_cache_and_runs_from_there() {
        use std::os::unix::fs::PermissionsExt as _;

        let cache = tempfile::tempdir().expect("a directory to keep the cache in");
        let project = tempfile::tempdir().expect("a directory to be the project");
        let compilers = tempfile::tempdir().expect("a directory for the compiler");

        // A `cc` that writes a program saying it ran, wherever it is told to.
        let compiler = compilers.path().join("cc");
        std::fs::write(
            &compiler,
            "#!/bin/sh\nprintf '#!/bin/sh\\nprintf ran\\n' > \"$3\"\nchmod +x \"$3\"\n",
        )
        .expect("the compiler is written");
        std::fs::set_permissions(&compiler, std::fs::Permissions::from_mode(0o755))
            .expect("and can be run");

        let hostile = r#"my "program" $HOME `id` ; x.c"#;
        std::fs::write(project.path().join(hostile), "int main(void){return 0;}\n")
            .expect("a source file");

        let (command, args, _) = defaults_for(Some("C"), Some(&project.path().join(hostile)));
        let ran = smol::block_on(
            smol::process::Command::new(&command)
                .args(&args)
                .current_dir(project.path())
                .env("XDG_CACHE_HOME", cache.path())
                .env(
                    "PATH",
                    format!(
                        "{}:{}",
                        compilers.path().display(),
                        std::env::var("PATH").unwrap_or_default()
                    ),
                )
                .output(),
        )
        .expect("a machine that runs this editor has a shell");

        assert!(
            ran.status.success(),
            "the line did not run: {}",
            String::from_utf8_lossy(&ran.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&ran.stdout),
            "ran",
            "the program that was built is the one that ran, and nothing else"
        );

        let left_in_the_project: Vec<String> = std::fs::read_dir(project.path())
            .expect("the project can be read")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(
            left_in_the_project,
            vec![hostile.to_string()],
            "the project holds the source and nothing else"
        );

        let built = every_file_under(cache.path());
        assert_eq!(
            built
                .iter()
                .filter_map(|path| path.file_name())
                .map(|name| name.to_string_lossy().to_string())
                .collect::<Vec<_>>(),
            vec![r#"my "program" $HOME `id` ; x"#.to_string()],
            "the cache holds the program, under the name it was built from: {built:?}"
        );

        // And it sits in a folder named after nothing but a digest, one step under
        // the cache: a folder named after the file would put a quote, a `$` or a
        // backtick from that name into the command the shell reads.
        let folder = built[0].parent().expect("the program is in a folder");
        assert_eq!(
            folder.parent(),
            Some(cache.path().join("zed").join("run").as_path()),
            "one step under the cache: {folder:?}"
        );
        let named = folder
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        assert!(
            named.len() == 16 && named.chars().all(|letter| letter.is_ascii_hexdigit()),
            "the folder is named by a digest and nothing else: {named}"
        );
    }

    /// Every file under `at`, however deep.
    fn every_file_under(at: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let Ok(entries) = std::fs::read_dir(at) else {
            return found;
        };
        for entry in entries.flatten() {
            match entry.path().is_dir() {
                true => found.extend(every_file_under(&entry.path())),
                false => found.push(entry.path()),
            }
        }
        found
    }

    /// The compile goes through a shell, and a file name is not a shell word. A
    /// space would split the command in two; a quote or a semicolon would run
    /// something nobody asked for.
    #[test]
    fn a_file_name_the_shell_would_misread_is_still_one_word() {
        let (command, args, _) = defaults_for(
            Some("C++"),
            Some(Path::new("/projects/thing/my program'; rm -rf x.cpp")),
        );
        assert_eq!(command, "sh");
        let line = args.join(" ");
        assert!(
            line.contains(r"'my program'\''; rm -rf x.cpp'"),
            "the file has to reach the compiler as one word: {line}"
        );
        assert!(
            line.contains(r"'/my program'\''; rm -rf x'"),
            "and so does what it is built into: {line}"
        );
        assert!(
            !line.contains("&& rm -rf"),
            "nothing in a file name may become a command of its own: {line}"
        );
    }

    /// What the editor already runs for the line beats any guess: it is known to
    /// work.
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
}
