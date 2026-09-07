use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::sync::Arc;

use gpui::{App, Global};
use ruff_db::diagnostic::{Diagnostic, Severity, UnifiedFile};
use ruff_db::files::{File, system_path_to_file};
use ruff_db::system::{SystemPath, SystemPathBuf};

use crate::{PythonProject, TypesFromTy};

/// What these diagnostics say they came from, so a reader can tell a type
/// error from a lint finding in the same file at a glance.
const SOURCE: &str = "ty";

/// Asks `ty` what is wrong with a file's types, out of the same project
/// databases that answer hover and completion.
///
/// Cheap to clone: a handle on the one source object, not a database.
#[derive(Clone)]
pub struct TypeChecker(Arc<TypesFromTy>);

struct Registered(TypeChecker);

impl Global for Registered {}

pub(crate) fn register(types: Arc<TypesFromTy>, cx: &mut App) {
    cx.set_global(Registered(TypeChecker(types)));
}

/// The type checker this process holds, where one has been registered.
pub fn type_checker(cx: &App) -> Option<TypeChecker> {
    cx.try_global::<Registered>()
        .map(|registered| registered.0.clone())
}

impl TypeChecker {
    /// Everything `ty` finds wrong with the types in one file, as diagnostics
    /// in the units the protocol means by a character.
    ///
    /// `text` is what the buffer says rather than what is on disk, so a file
    /// saved by a formatter that has not been reread is still described by the
    /// text the reader is looking at.
    ///
    /// A file `ty` cannot place in a project -- no project at `root`, a path
    /// that is not UTF-8, a file outside the checked set -- yields no
    /// diagnostics rather than a complaint about itself. The reader asked for
    /// type errors in their code, not for this source's own difficulties.
    ///
    /// Blocking, and it holds the database lock for the length of the check.
    /// Call it off the foreground thread.
    pub fn check(&self, root: &Path, path: &Path, text: &str) -> Vec<lsp::Diagnostic> {
        let Some(project) = self.0.project_for(root) else {
            return Vec::new();
        };
        let Ok(path) = SystemPathBuf::from_path_buf(path.to_path_buf()) else {
            return Vec::new();
        };
        check(&project, &path, text)
    }
}

/// Asks `ty` to check one file and reads what it said.
///
/// Returns nothing rather than propagating a panic, for the same reason the
/// hover side does: a type-inference query that falls over is a file whose
/// errors go unreported, and must not be the end of the editor being typed
/// into.
pub(crate) fn check(
    project: &PythonProject,
    path: &SystemPath,
    text: &str,
) -> Vec<lsp::Diagnostic> {
    let Ok(mut database) = project.database.lock() else {
        return Vec::new();
    };
    if project.open.record(path, text.to_string()) {
        File::sync_path(&mut *database, path);
    }
    let database = &*database;

    std::panic::catch_unwind(AssertUnwindSafe(|| {
        let Ok(file) = system_path_to_file(database, path) else {
            return Vec::new();
        };
        database
            .check_file(file)
            .iter()
            .filter(|found| !is_also_ruffs_to_report(found))
            .filter_map(|found| read_one(found, file, text))
            .collect()
    }))
    .unwrap_or_default()
}

/// Whether the linter already reports this, so that a reader is not shown the
/// same problem twice.
///
/// The two sources overlap in exactly one place. `ty` grades type rules --
/// `invalid-assignment`, `unresolved-attribute` -- and ruff's own rule set is
/// pyflakes and pycodestyle, which name nothing `ty` names. A parse error is
/// the exception: both report it, both call it `invalid-syntax`, and both put
/// it at the same offset. It is dropped here rather than on ruff's side
/// because a parse error is the one finding `ty` adds nothing to -- past it
/// there are no types to infer, so `ty` reports the parse error and stops,
/// while ruff reports it for every file in the project in one run.
///
/// The cost of choosing this side is that a machine with no ruff installed
/// sees no parse errors from either source.
fn is_also_ruffs_to_report(found: &Diagnostic) -> bool {
    found.is_invalid_syntax()
}

/// One of `ty`'s findings as a diagnostic, or nothing where it is not about
/// the file that was checked.
fn read_one(found: &Diagnostic, checked: File, text: &str) -> Option<lsp::Diagnostic> {
    let span = found.primary_span()?;
    // A finding whose span lands in another file cannot be shown against this
    // one, and its offsets would silently point at the wrong text if it were
    // tried.
    if !matches!(span.file(), UnifiedFile::Ty(file) if *file == checked) {
        return None;
    }
    // No range at all is `ty`'s way of saying the finding is about the file as
    // a whole, which the protocol has no way to express; the start of the file
    // is the closest thing to it.
    let about = span.range().unwrap_or_default();
    Some(lsp::Diagnostic {
        range: lsp::Range {
            start: utf16_position_of(text, usize::from(about.start())),
            end: utf16_position_of(text, usize::from(about.end())),
        },
        severity: Some(severity_of(found.severity())),
        // The rule's own name, so two different rules are distinguishable and
        // the reader can look one up or silence it.
        code: Some(lsp::NumberOrString::String(
            found.secondary_code_or_id().to_string(),
        )),
        source: Some(SOURCE.to_string()),
        message: found.concise_message().to_string(),
        ..Default::default()
    })
}

/// `ty`'s own grade for a finding, carried across rather than flattened.
///
/// A rule the project has configured as a warning must not paint the file red,
/// and one `ty` only mentions must not either. `Fatal` becomes an error
/// because the protocol has nothing above one.
fn severity_of(grade: Severity) -> lsp::DiagnosticSeverity {
    match grade {
        Severity::Info => lsp::DiagnosticSeverity::INFORMATION,
        Severity::Warning => lsp::DiagnosticSeverity::WARNING,
        Severity::Error | Severity::Fatal => lsp::DiagnosticSeverity::ERROR,
    }
}

/// The position a byte offset falls at, counted the way the protocol counts:
/// lines from zero, and characters as UTF-16 code units.
///
/// `ty` measures in UTF-8 bytes -- its `TextSize` indexes the source string
/// directly -- and three units are in play that disagree. On the line
/// `café = "🦀🔥"; total: int = "not a number"` the string assigned to
/// `total` starts at byte 33, at character 26 and at UTF-16 unit 28. Reading
/// one as another puts every diagnostic on a line with an emoji in the wrong
/// column.
///
/// An offset past the end, or inside a character, lands on the nearest
/// boundary at or before it rather than panicking: `ty` measured against text
/// the editor may have changed since.
fn utf16_position_of(text: &str, offset: usize) -> lsp::Position {
    let mut offset = offset.min(text.len());
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    let before = &text[..offset];
    let start_of_line = before.rfind('\n').map_or(0, |newline| newline + 1);
    lsp::Position {
        line: before.matches('\n').count() as u32,
        character: before[start_of_line..].encode_utf16().count() as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruff_text_size::TextSize;
    use std::time::Instant;

    /// A genuine type error: a function that takes an `int`, handed a string.
    const WRONG: &str = "def double(value: int) -> int:\n    return value * 2\n\n\nanswer = double(\"twenty-one\")\n";

    /// The same file with the mistake taken out.
    const RIGHT: &str =
        "def double(value: int) -> int:\n    return value * 2\n\n\nanswer = double(21)\n";

    /// Drives the same path a check takes, without an editor in front of it.
    /// The file is written to a real directory because `ty` reads the disk to
    /// find a project at all, and the text is handed over separately because
    /// that is what a buffer would supply.
    fn what_ty_says(on_disk: &str, text: &str) -> (tempfile::TempDir, Vec<lsp::Diagnostic>) {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join("checked.py");
        std::fs::write(&path, on_disk).expect("a file to check");
        let project = PythonProject::discover(directory.path()).expect("a project");
        let path = SystemPathBuf::from_path_buf(path).expect("a UTF-8 path");
        let found = check(&project, &path, text);
        (directory, found)
    }

    fn only_one(found: &[lsp::Diagnostic]) -> &lsp::Diagnostic {
        assert_eq!(found.len(), 1, "{found:?}");
        &found[0]
    }

    /// The central case: `ty` finds a type error, and it arrives at the place
    /// it is about, with the grade `ty` gave it and the name of the rule that
    /// fired.
    #[test]
    fn a_type_error_is_reported_where_it_is_with_tys_grade_and_rule_name() {
        let (_directory, found) = what_ty_says(WRONG, WRONG);
        let error = only_one(&found);

        assert_eq!(
            error.severity,
            Some(lsp::DiagnosticSeverity::ERROR),
            "ty grades a wrong argument type an error: {error:?}"
        );
        assert_eq!(
            error.code,
            Some(lsp::NumberOrString::String(
                "invalid-argument-type".to_string()
            )),
            "the rule's own name, so two rules are distinguishable: {error:?}"
        );
        assert_eq!(error.source.as_deref(), Some("ty"));
        assert!(
            error.message.contains("int"),
            "the message should say what was expected: {}",
            error.message
        );

        // The last line is `answer = double("twenty-one")`, and the argument
        // is what is wrong with it.
        let line = WRONG.lines().nth(4).expect("the call");
        let argument = line.find('"').expect("the string argument");
        assert_eq!(
            error.range.start,
            lsp::Position::new(4, argument as u32),
            "on the argument, not on the whole statement: {error:?}"
        );
        assert!(error.range.end > error.range.start, "{error:?}");
    }

    /// A file with nothing wrong with it reports nothing, so a clean project
    /// stays clean rather than collecting notes about itself.
    #[test]
    fn a_clean_file_reports_nothing() {
        let (_directory, found) = what_ty_says(RIGHT, RIGHT);
        assert!(found.is_empty(), "{found:?}");
    }

    /// A file the reader has just fixed reports an empty list rather than
    /// keeping what it said before. The editor keeps the last thing it was
    /// given, so an empty report is what takes the old underlining off.
    #[test]
    fn a_fixed_file_reports_an_empty_list_rather_than_what_it_said_before() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join("checked.py");
        std::fs::write(&path, WRONG).expect("a file to check");
        let project = PythonProject::discover(directory.path()).expect("a project");
        let path = SystemPathBuf::from_path_buf(path).expect("a UTF-8 path");

        assert_eq!(check(&project, &path, WRONG).len(), 1);
        assert!(
            check(&project, &path, RIGHT).is_empty(),
            "the same file, with the mistake taken out"
        );
    }

    /// A file `ty` cannot place in a project reports nothing rather than an
    /// error about itself. A path that does not exist is the plain case: there
    /// is no file for `ty` to check, and the reader asked about their types.
    #[test]
    fn a_file_ty_cannot_place_reports_nothing_rather_than_an_error_about_itself() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let project = PythonProject::discover(directory.path()).expect("a project");
        let missing = SystemPathBuf::from_path_buf(directory.path().join("never-written.py"))
            .expect("a UTF-8 path");
        assert!(check(&project, &missing, WRONG).is_empty());
    }

    /// `ty` counts in UTF-8 bytes, the protocol counts in UTF-16 code units,
    /// and a person counts characters. All three disagree on a line holding an
    /// accent and two emoji, and reading one as another puts the diagnostic in
    /// the wrong column.
    #[test]
    fn a_byte_offset_becomes_a_utf16_one_and_not_a_byte_or_character_one() {
        const EMOJI: &str = "label = \"start\"\ncaf\u{e9} = \"\u{1f980}\u{1f525}\"; total: int = \"not a number\"\n";

        let line = EMOJI.lines().nth(1).expect("the café line");
        let wrong_value = line.find("\"not").expect("the string assigned to an int");

        // Three counts of the same place, all different.
        assert_eq!(line[..wrong_value].len(), 33, "bytes");
        assert_eq!(line[..wrong_value].chars().count(), 26, "characters");
        assert_eq!(
            line[..wrong_value].encode_utf16().count(),
            28,
            "UTF-16 units"
        );

        let (_directory, found) = what_ty_says(EMOJI, EMOJI);
        let error = only_one(&found);
        assert_eq!(
            error.range.start,
            lsp::Position::new(1, 28),
            "the protocol's own unit -- not 33, which is what ty reports, and not 26: {error:?}"
        );
    }

    /// An offset past the end of the text, or inside a character, lands on the
    /// nearest boundary at or before it. `ty` measured against text the editor
    /// may have changed since.
    #[test]
    fn an_offset_past_the_end_or_inside_a_character_lands_on_a_boundary() {
        let text = "caf\u{e9}\nx = 1\n";
        assert_eq!(utf16_position_of(text, 0), lsp::Position::new(0, 0));
        assert_eq!(
            utf16_position_of(text, 4),
            lsp::Position::new(0, 3),
            "inside the é, so back to before it"
        );
        // Past the end is the end of the text, which for a file that ends in
        // a newline is the empty line after it.
        assert_eq!(utf16_position_of(text, 9_000), lsp::Position::new(2, 0));
        assert_eq!(utf16_position_of("", 3), lsp::Position::new(0, 0));
    }

    /// What a check costs: how long one takes, and what it adds to the
    /// database hover and completion already keep warm. Printed rather than
    /// asserted -- the numbers are for a person deciding when a check should
    /// run, and a threshold here would fail on an unrelated allocator change
    /// or a slower machine.
    #[test]
    fn time_and_memory_of_checking_a_project() {
        let modules = [
            (
                "shapes.py",
                "import dataclasses\n\n\n@dataclasses.dataclass\nclass Point:\n    x: int\n    y: float\n",
            ),
            (
                "loading.py",
                "import json\nimport pathlib\n\n\ndef read(path: pathlib.Path) -> dict:\n    return json.loads(path.read_text())\n",
            ),
            (
                "waiting.py",
                "import asyncio\nimport typing\n\n\nasync def gather(items: typing.Sequence[int]) -> list[int]:\n    await asyncio.sleep(0)\n    return sorted(items)\n",
            ),
            (
                "main.py",
                "import loading\nimport pathlib\nimport shapes\n\n\nplace = shapes.Point(1, 2.0)\nwrong: int = place\n",
            ),
        ];
        let directory = tempfile::tempdir().expect("a temporary directory");
        for (name, source) in modules {
            std::fs::write(directory.path().join(name), source).expect("a module");
        }
        let asked_about = |name: &str, source: &str, offset: usize| crate::Asked {
            root: directory.path().to_path_buf(),
            path: SystemPathBuf::from_path_buf(directory.path().join(name)).expect("a UTF-8 path"),
            text: source.to_string(),
            offset: TextSize::try_from(offset).expect("an offset that fits"),
        };

        let before = resident_bytes();
        let project = PythonProject::discover(directory.path()).expect("a project");
        // Hovers across the last line of each module first: that is the state
        // a database is in while a reader is working, and it is the cost the
        // checker shares rather than pays.
        let mut answered = 0;
        for (name, source) in modules {
            let last_line = source
                .trim_end_matches('\n')
                .rfind('\n')
                .map_or(0, |newline| newline + 1);
            for offset in last_line..source.trim_end().len() {
                if crate::answer(&project, &asked_about(name, source, offset)).is_some() {
                    answered += 1;
                }
            }
        }
        assert!(answered > 0, "the measurement needs a warm database");
        let warm = resident_bytes();

        let paths: Vec<(SystemPathBuf, &str)> = modules
            .iter()
            .map(|(name, source)| {
                (
                    SystemPathBuf::from_path_buf(directory.path().join(name))
                        .expect("a UTF-8 path"),
                    *source,
                )
            })
            .collect();
        let (main_path, main_source) = paths.last().expect("main.py");

        let first = Instant::now();
        let found = check(&project, main_path, main_source);
        let first = first.elapsed();
        assert!(
            !found.is_empty(),
            "the measurement needs a file with something wrong with it"
        );

        // The same text again: what a save costs when nothing has changed
        // under it, which is the floor.
        let again = Instant::now();
        check(&project, main_path, main_source);
        let again = again.elapsed();

        // Every file the project has, each checked once, which is the worst a
        // whole-project pass would cost.
        let all = Instant::now();
        let mut reported = 0;
        for (path, source) in &paths {
            reported += check(&project, path, source).len();
        }
        let all = all.elapsed();
        let after = resident_bytes();

        println!(
            "checking {} modules: first check {first:?}, the same text again {again:?}, \
             every file once {all:?} for {reported} findings; \
             resident memory {:.1} MB before, {:.1} MB with a database warm from {answered} hovers, \
             {:.1} MB after the checks -- {:.1} MB for the database, {:.1} MB added by checking",
            modules.len(),
            before as f64 / 1e6,
            warm as f64 / 1e6,
            after as f64 / 1e6,
            warm.saturating_sub(before) as f64 / 1e6,
            after.saturating_sub(warm) as f64 / 1e6
        );
    }

    fn resident_bytes() -> u64 {
        let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
        let pages: u64 = statm
            .split_whitespace()
            .nth(1)
            .and_then(|pages| pages.parse().ok())
            .unwrap_or_default();
        pages * 4096
    }
}
