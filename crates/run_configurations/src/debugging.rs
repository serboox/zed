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

#[cfg(test)]
mod tests {
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
}
