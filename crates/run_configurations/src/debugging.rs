use task::TaskTemplate;

/// The first word of a command, which is the program being run.
fn program(command: &str) -> &str {
    command.split_whitespace().next().unwrap_or("")
}

/// Every program a locator in this editor knows how to derive a debug session
/// from.
const DERIVED_FROM: [&str; 5] = ["cargo", "go", "npm", "pnpm", "yarn"];

/// Whether a debugger can be derived from this command on its own.
///
/// Mirrors the locators in `crates/project/src/debugger/locators/`, which are
/// what actually work out the artifact a command produces: cargo takes `cargo`,
/// go takes `go`, the JavaScript one takes `npm`, `pnpm` and `yarn`, and python
/// takes any command whose own name starts with `python`. Anything else -- a
/// Makefile target, a mise task -- could build anything, or nothing, and there
/// is no honest way to guess.
pub fn can_be_derived_from(command: &str) -> bool {
    let program = program(command);
    if program.is_empty() {
        return false;
    }
    // Still a variable at this point -- it stands for a runner the reader's
    // settings choose, and every runner it stands for has a locator.
    if program.starts_with("$ZED_") || program.starts_with("${ZED_") {
        return true;
    }
    let name = program.rsplit('/').next().unwrap_or(program);
    name.starts_with("python") || DERIVED_FROM.contains(&name)
}

/// What to tell the reader whose configuration cannot be debugged, or `None`
/// when it can. Said in full rather than by hiding the offer: a control that is
/// simply absent reads as a bug, and one that guesses runs the wrong thing.
pub fn why_it_cannot_be_debugged(command: &str) -> Option<&'static str> {
    if can_be_derived_from(command) {
        return None;
    }
    if program(command).is_empty() {
        return Some("Give this configuration a command first.");
    }
    Some(concat!(
        "Debugging is not worked out automatically for this command: ",
        "what it builds is not known from the command itself. ",
        "Write a debug configuration naming the artifact to debug it."
    ))
}

/// The debugger that goes with a program, when one does. The same mapping the
/// locators use, said once here so the window that offers a debugger and the
/// list of ways to run a project cannot drift apart.
pub fn adapter_for(command: &str) -> Option<&'static str> {
    let program = program(command);
    let name = program.rsplit('/').next().unwrap_or(program);
    if name.starts_with("python") {
        return Some("Debugpy");
    }
    match name {
        "go" => Some("Delve"),
        "cargo" => Some("CodeLLDB"),
        "npm" | "pnpm" | "yarn" | "node" => Some("JavaScript"),
        "cc" | "c++" | "gcc" | "g++" | "clang" | "clang++" => Some("CodeLLDB"),
        _ => None,
    }
}

/// The task as a debugger can read it, when the one the reader wrote hides the
/// real program behind a wrapper.
///
/// A project run through a script that loads an environment and then execs the
/// compiler -- `with-env go run ./cmd/api` -- tells a locator nothing: locators
/// read the command, and the command is the script. The program is in the
/// arguments, so the first argument that is a program a debugger knows becomes
/// the command and the rest of the arguments follow it. Everything else about
/// the task is kept, because the wrapper's whole job -- the working directory,
/// the variables, the file they come from -- is what makes the run work.
///
/// `None` when the task needs no unwrapping or when nothing in it is a program
/// a debugger knows, which is the honest answer for a Makefile target.
pub fn unwrapped(task: &TaskTemplate) -> Option<TaskTemplate> {
    if adapter_for(&task.command).is_some() {
        return None;
    }
    let at = task
        .args
        .iter()
        .position(|argument| adapter_for(argument).is_some())?;
    let mut unwrapped = task.clone();
    unwrapped.command = task.args[at].clone();
    unwrapped.args = task.args[at + 1..].to_vec();
    Some(unwrapped)
}

/// Whether a debugger can be worked out for this task, looking past a wrapper.
pub fn a_debugger_can_be_worked_out(task: &TaskTemplate) -> bool {
    can_be_derived_from(&task.command)
        || unwrapped(task).is_some_and(|unwrapped| can_be_derived_from(&unwrapped.command))
}

/// The label with its leading word dropped, underscores turned to spaces, and
/// lowercased -- "Run" in "Run API" and "Debug" in "Debug API" is the verb, and
/// what is left is what the label is about.
///
/// A one-word label has nothing to drop a verb from, so it is kept whole
/// instead: dropping its only word would compare every such label as the same
/// empty subject.
fn subject(label: &str) -> String {
    let mut words = label.split_whitespace();
    words.next();
    let after_the_verb: String = words.collect::<Vec<_>>().join(" ");
    let subject = match after_the_verb.is_empty() {
        true => label.trim(),
        false => after_the_verb.as_str(),
    };
    subject.replace('_', " ").to_lowercase()
}

/// Whether a task's label and a debug configuration's label are about the same
/// thing, once the leading verb and the choice between underscores and spaces
/// are looked past -- a project's tasks and its debug configurations are
/// written in separate files, and do not always spell a shared name the same
/// way (`Run industry_ratios_cron` and `Debug industry_ratios cron` are meant
/// as the same pairing).
pub fn name_the_same_thing(task_label: &str, debug_label: &str) -> bool {
    !task_label.trim().is_empty()
        && !debug_label.trim().is_empty()
        && subject(task_label) == subject(debug_label)
}

#[cfg(test)]
mod tests {

    /// A project run through a script that loads an environment and then execs
    /// the compiler is the shape every one of these configurations has, and it
    /// tells a locator nothing: locators read the command, and the command is the
    /// script. The program is one argument along.
    #[test]
    fn a_wrapper_script_is_taken_off_so_the_program_can_be_seen() {
        let wrapped = TaskTemplate {
            label: "Run API".to_string(),
            command: "$HOME/.envs/.zed/with-env".to_string(),
            args: vec!["go".into(), "run".into(), "./cmd/api".into()],
            cwd: Some("$ZED_WORKTREE_ROOT".to_string()),
            env_file: Some(".env.local".to_string()),
            ..TaskTemplate::default()
        };
        assert!(
            !can_be_derived_from(&wrapped.command),
            "the wrapper is what no locator can read anything from"
        );

        let unwrapped = unwrapped(&wrapped).expect("the program is in the arguments");
        assert_eq!(unwrapped.command, "go");
        assert_eq!(unwrapped.args, vec!["run", "./cmd/api"]);
        assert!(a_debugger_can_be_worked_out(&wrapped));
        assert_eq!(adapter_for(&unwrapped.command), Some("Delve"));

        // Everything the wrapper was there for survives: without the working
        // directory and the variables the run works for nobody.
        assert_eq!(unwrapped.cwd.as_deref(), Some("$ZED_WORKTREE_ROOT"));
        assert_eq!(unwrapped.env_file.as_deref(), Some(".env.local"));
        assert_eq!(unwrapped.label, "Run API");
    }

    #[test]
    fn a_task_that_needs_no_unwrapping_is_left_alone() {
        let plain = TaskTemplate {
            label: "Run API".to_string(),
            command: "go".to_string(),
            args: vec!["run".into(), "./cmd/api".into()],
            ..TaskTemplate::default()
        };
        assert!(unwrapped(&plain).is_none());
        assert!(a_debugger_can_be_worked_out(&plain));
    }

    /// A Makefile target could build anything, or nothing, and there is nothing
    /// in it to unwrap either.
    #[test]
    fn a_target_that_says_nothing_is_not_pretended_to_be_debuggable() {
        let opaque = TaskTemplate {
            label: "Run everything".to_string(),
            command: "make".to_string(),
            args: vec!["all".into()],
            ..TaskTemplate::default()
        };
        assert!(unwrapped(&opaque).is_none());
        assert!(!a_debugger_can_be_worked_out(&opaque));
        assert!(why_it_cannot_be_debugged(&opaque.command).is_some());
    }

    #[test]
    fn every_program_a_locator_knows_has_a_debugger_named_for_it() {
        assert_eq!(adapter_for("go"), Some("Delve"));
        assert_eq!(adapter_for("cargo"), Some("CodeLLDB"));
        assert_eq!(adapter_for("npm"), Some("JavaScript"));
        assert_eq!(adapter_for("/usr/bin/python3.12"), Some("Debugpy"));
        assert_eq!(adapter_for("make"), None);
        assert_eq!(adapter_for(""), None);
    }
    use super::*;

    #[test]
    fn the_commands_the_locators_were_written_for_can_be_debugged() {
        for command in [
            "cargo test",
            "cargo run --bin api",
            "go test ./...",
            "npm run dev",
            "pnpm test",
            "yarn build",
            "python -m pytest",
            "python3 manage.py runserver",
            "/usr/bin/python3.12 -m app",
            "$ZED_TYPESCRIPT_RUNNER test",
        ] {
            assert!(
                can_be_derived_from(command),
                "a locator exists for {command:?}"
            );
            assert_eq!(why_it_cannot_be_debugged(command), None);
        }
    }

    /// The commands the mockup names as opaque, plus the ones nobody wrote a
    /// locator for. Guessing a debugger for these is the mistake this replaces.
    #[test]
    fn an_opaque_command_says_why_instead_of_guessing() {
        for command in [
            "make build",
            "mise run test",
            "sh ./run.sh",
            "php artisan serve",
            "./my-own-binary",
            "docker compose up",
        ] {
            assert!(
                !can_be_derived_from(command),
                "no locator exists for {command:?}"
            );
            let said = why_it_cannot_be_debugged(command)
                .unwrap_or_else(|| panic!("{command:?} has to say why"));
            assert!(
                said.len() > 40,
                "the reason has to be a sentence, not a word: {said:?}"
            );
        }
    }

    #[test]
    fn a_configuration_with_no_command_yet_is_told_to_write_one() {
        for command in ["", "   "] {
            assert!(!can_be_derived_from(command));
            assert_eq!(
                why_it_cannot_be_debugged(command),
                Some("Give this configuration a command first.")
            );
        }
    }

    /// `python` is matched on the program's own name, so a path to it counts and
    /// a command that merely contains the word does not.
    #[test]
    fn only_the_program_is_looked_at() {
        assert!(can_be_derived_from("/opt/homebrew/bin/python3 -m app"));
        assert!(!can_be_derived_from("make python-tests"));
        assert!(!can_be_derived_from("env python app.py"));
    }

    /// The pairing a wrapper-script project actually relies on: a command an
    /// automatic locator cannot read anything from is still pairable with a
    /// debug configuration whose label names the same thing.
    #[test]
    fn a_task_pairs_with_the_debug_configuration_named_for_the_same_thing() {
        assert!(name_the_same_thing("Run API", "Debug API"));
        assert!(!can_be_derived_from("$HOME/.envs/.zed/with-env"));
    }

    /// The two files spell a shared name differently -- one keeps the
    /// underscores throughout, the other breaks the last one into a space --
    /// and the pairing has to look past that rather than treat them as two
    /// unrelated things.
    #[test]
    fn underscores_and_spaces_do_not_break_the_pairing() {
        assert!(name_the_same_thing(
            "Run industry_ratios_cron",
            "Debug industry_ratios cron"
        ));
        assert!(name_the_same_thing(
            "Run ratios_financials_cron",
            "Debug ratios_financials cron"
        ));
    }

    /// A task with no debug configuration written for it pairs with nothing --
    /// two labels about different things must not be said to be the same.
    #[test]
    fn unrelated_labels_do_not_pair() {
        assert!(!name_the_same_thing("Run unit tests", "Debug API"));
        assert!(!name_the_same_thing(
            "Run tests",
            "Debug ratios_financials cron"
        ));
    }

    /// Neither side has to carry a verb at all: a label that is only the
    /// subject still pairs with one that names the same subject after its own
    /// verb is dropped.
    #[test]
    fn a_label_with_no_verb_still_pairs() {
        assert!(name_the_same_thing("API", "Debug API"));
    }

    /// Two blank labels are not "the same thing" -- an empty subject must
    /// never be treated as matching another empty subject.
    #[test]
    fn blank_labels_never_pair() {
        assert!(!name_the_same_thing("", ""));
        assert!(!name_the_same_thing("   ", "Debug API"));
    }
}
