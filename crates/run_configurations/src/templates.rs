use std::path::Path;

use collections::HashMap;
use task::TaskTemplate;
use zed_actions::run_configurations::EntryPointOffer;

/// The ways of running a project this editor knows how to fill in, in the order
/// the design document lists them. Choosing one fills the command and its
/// arguments; what it says about debugging is what the locators can work out
/// from that command, which for a Makefile target or a mise task is nothing.
pub struct Template {
    /// What the reader picks it by.
    pub name: &'static str,
    /// The command it fills in, with the file's own directory as the working
    /// directory where a language has packages.
    pub command: &'static str,
    pub args: &'static [&'static str],
    /// The debugger a locator derives from that command, if one can.
    pub debugger: Option<&'static str>,
}

pub const TEMPLATES: &[Template] = &[
    Template {
        name: "Go",
        command: "go",
        args: &["run", "."],
        debugger: Some("Delve"),
    },
    Template {
        name: "Rust",
        command: "cargo",
        args: &["run"],
        debugger: Some("CodeLLDB"),
    },
    Template {
        name: "Node.js",
        command: "npm",
        args: &["start"],
        debugger: Some("JavaScript"),
    },
    Template {
        name: "Python",
        command: "python3",
        args: &["${ZED_FILENAME}"],
        debugger: Some("Debugpy"),
    },
    Template {
        name: "C/C++",
        command: "cc",
        args: &["${ZED_FILENAME}"],
        debugger: Some("CodeLLDB"),
    },
    Template {
        name: "PHP",
        command: "php",
        args: &["${ZED_FILENAME}"],
        debugger: None,
    },
    Template {
        name: "Makefile",
        command: "make",
        args: &[],
        debugger: None,
    },
    Template {
        name: "Mise",
        command: "mise",
        args: &["run"],
        debugger: None,
    },
];

/// The template a command was filled in from, if it reads like one of them. Only
/// the program is compared: the arguments are the reader's to change.
pub fn template_of(command: &str) -> Option<&'static Template> {
    let program = command.split_whitespace().next()?;
    let name = program.rsplit('/').next().unwrap_or(program);
    TEMPLATES.iter().find(|template| {
        template.command == name
            || (template.command == "python3" && name.starts_with("python"))
            || (template.command == "cc" && matches!(name, "c++" | "gcc" | "g++" | "clang"))
            || (template.command == "npm" && matches!(name, "pnpm" | "yarn" | "node"))
    })
}

/// What an entry point is run with.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
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
