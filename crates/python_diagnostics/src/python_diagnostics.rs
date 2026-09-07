use std::path::{Path, PathBuf};

use collections::HashMap;
use serde::Deserialize;

mod fixing;
mod watching;

pub use watching::{Lint, init};

/// One finding the linter reported, at a place the editor can put it.
///
/// The range is in the units the protocol means by a character -- UTF-16 code
/// units -- because that is what this editor's own conversion reads. ruff
/// counts something else; see [`utf16_position_of`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reported {
    /// Absolute, so a caller does not have to remember where ruff ran.
    pub path: PathBuf,
    pub diagnostic: lsp::Diagnostic,
    /// The fix ruff computed for this finding and called safe, or nothing
    /// where it computed none or would not vouch for the one it computed.
    pub fixes: Vec<Fix>,
}

/// A fix ruff wrote itself, offered word for word as it gave it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fix {
    /// ruff's own words for it -- "Remove unused import: `os`".
    pub title: String,
    pub replacements: Vec<Replacement>,
}

/// One piece of text ruff asked to be put somewhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replacement {
    /// Absolute, like [`Reported::path`]. ruff keeps every edit of a fix in
    /// the file the finding is in, so this is always that file.
    pub path: PathBuf,
    /// In UTF-16 code units, like [`Reported`]'s range and for the same
    /// reason.
    pub range: lsp::Range,
    /// Empty where ruff asked for the range to be deleted, which is a fix
    /// like any other and not the absence of one.
    pub new_text: String,
    /// The text that was in `range` when ruff measured it. A caller compares
    /// it against what is there now: a file edited since the run has moved
    /// every offset in the report, and a replacement made against moved
    /// offsets overwrites something ruff never looked at.
    pub replaced: String,
}

/// What `ruff check --output-format json` writes: one array holding one
/// object per finding.
#[derive(Deserialize)]
struct Finding {
    filename: String,
    message: String,
    /// The rule that fired -- `F401`, or `invalid-syntax` for a parse error.
    /// Absent in ruff's older output, where a parse error had no code at all.
    #[serde(default)]
    code: Option<String>,
    location: Place,
    end_location: Place,
    #[serde(default)]
    fix: Option<SuggestedFix>,
}

/// A place in a file, counted from one. `column` counts Unicode characters --
/// not bytes and not UTF-16 code units, which the protocol wants; see
/// [`utf16_position_of`].
#[derive(Deserialize)]
struct Place {
    row: u32,
    column: u32,
}

#[derive(Deserialize)]
struct SuggestedFix {
    #[serde(default)]
    message: Option<String>,
    /// Read as text rather than as an enum: a grade this reader has never
    /// heard of must leave the rest of the finding readable, and an unknown
    /// variant would fail the whole entry instead.
    #[serde(default)]
    applicability: Option<String>,
    #[serde(default)]
    edits: Vec<Edit>,
}

/// One piece of text ruff asked to put over `location..end_location`. The
/// content is empty where it asked for a deletion.
#[derive(Deserialize)]
struct Edit {
    content: String,
    location: Place,
    end_location: Place,
}

/// The one grade of fix this reader offers.
///
/// ruff grades every fix it computes, and only this grade is one it will
/// apply without being asked twice. An `unsafe` fix can change what the code
/// means -- rewriting `x == None` to `x is None` alters the comparison for a
/// type that overloads `__eq__` -- and a `display` one is written to be read
/// rather than applied. Applying either at one click would put something in
/// the file that nobody asked for, and that is worse than offering nothing.
const ONLY_GRADE_OFFERED: &str = "safe";

/// The code ruff gives a parse error, as opposed to a lint rule firing.
const PARSE_ERROR: &str = "invalid-syntax";

/// Reads what the linter reported into diagnostics the editor can show, with
/// no language server anywhere.
///
/// `ran_in` is the directory ruff was run in; ruff writes absolute paths, but
/// joining onto the root costs nothing and keeps a relative one meaningful.
/// `read` supplies a file's text, which is needed and not optional: ruff
/// gives character columns, the protocol wants UTF-16 code units, and only
/// the text itself can convert one to the other.
///
/// A file `read` cannot supply is skipped rather than guessed at. Putting a
/// diagnostic on the wrong column is worse than not showing it.
///
/// Output that will not parse yields nothing rather than panicking, and one
/// unreadable entry does not cost the rest of them.
pub fn what_ruff_reported(
    output: &str,
    ran_in: &Path,
    read: impl Fn(&Path) -> Option<String>,
) -> Vec<Reported> {
    let Ok(findings) = serde_json::from_str::<Vec<serde_json::Value>>(output) else {
        return Vec::new();
    };
    let mut reported = Vec::new();
    let mut texts: HashMap<PathBuf, Option<String>> = HashMap::default();
    for finding in findings {
        let Ok(finding) = serde_json::from_value::<Finding>(finding) else {
            continue;
        };
        let path = ran_in.join(&finding.filename);
        let text = texts
            .entry(path.clone())
            .or_insert_with(|| read(&path))
            .as_deref();
        let Some(text) = text else {
            continue;
        };
        let fixes = fix_in(&finding, &path, text).into_iter().collect();
        reported.push(Reported {
            fixes,
            diagnostic: lsp::Diagnostic {
                range: lsp::Range {
                    start: utf16_position_of(text, finding.location.row, finding.location.column),
                    end: utf16_position_of(
                        text,
                        finding.end_location.row,
                        finding.end_location.column,
                    ),
                },
                severity: Some(severity_of(finding.code.as_deref())),
                code: finding.code.clone().map(lsp::NumberOrString::String),
                source: Some("ruff".to_string()),
                message: what_it_said(&finding),
                ..Default::default()
            },
            path,
        });
    }
    reported
}

/// The whole of what one finding says: its own text, and the fix it offers.
/// Whether a fix exists at all is the reader's next question after what is
/// wrong, and an unsafe one is worth saying so about -- ruff will not apply
/// it without being asked twice, because it can change what the code means.
fn what_it_said(finding: &Finding) -> String {
    let mut said = finding.message.clone();
    if let Some(fix) = &finding.fix {
        said.push_str("\nfix");
        if fix.applicability.as_deref() == Some("unsafe") {
            said.push_str(" (unsafe)");
        }
        said.push_str(": ");
        said.push_str(fix.message.as_deref().unwrap_or("available"));
    }
    said
}

/// The fix ruff offered for this finding, where it is one this reader will
/// pass on.
///
/// Only [`ONLY_GRADE_OFFERED`] is kept; see there for why. A fix with no
/// message is dropped as well: what the reader picks from a menu is the
/// title, and an unnamed entry says nothing about what it will do.
///
/// A whole fix is refused where any one of its edits cannot be placed
/// against the text ruff measured. Its edits are halves of one change --
/// ruff's rewrite of a comparison deletes the old expression and writes the
/// new one -- and applying some of them leaves the file worse than before.
fn fix_in(finding: &Finding, path: &Path, text: &str) -> Option<Fix> {
    let fix = finding.fix.as_ref()?;
    if fix.applicability.as_deref() != Some(ONLY_GRADE_OFFERED) {
        return None;
    }
    let title = fix.message.clone()?;
    if title.is_empty() || fix.edits.is_empty() {
        return None;
    }
    let mut replacements = Vec::with_capacity(fix.edits.len());
    for edit in &fix.edits {
        let start = byte_offset_of(text, edit.location.row, edit.location.column)?;
        let end = byte_offset_of(text, edit.end_location.row, edit.end_location.column)?;
        // A range the text cannot be sliced by has been measured against a
        // different file than the one being read, and an empty string here
        // would be indistinguishable from ruff asking for an insertion.
        let replaced = text.get(start..end)?.to_string();
        replacements.push(Replacement {
            path: path.to_path_buf(),
            range: lsp::Range {
                start: utf16_position_of(text, edit.location.row, edit.location.column),
                end: utf16_position_of(text, edit.end_location.row, edit.end_location.column),
            },
            new_text: edit.content.clone(),
            replaced,
        });
    }
    Some(Fix {
        title,
        replacements,
    })
}

/// The byte offset a row and column fall at, needed because the text has to
/// be sliced by it to learn what a fix expects to replace.
///
/// ruff counts a row from one and a column in Unicode characters from one,
/// which is a third unit again from the bytes the file is stored in and the
/// UTF-16 code units the protocol asks for.
///
/// A row past the end of the text yields nothing rather than a guess: the
/// report was measured against a file the editor may have changed since, and
/// a fix placed by guess would overwrite text ruff never looked at. A column
/// past the end of its line lands at the end of that line, which is where
/// ruff itself points at a line's trailing whitespace.
fn byte_offset_of(text: &str, row: u32, column: u32) -> Option<usize> {
    let rows_before = row.checked_sub(1)?;
    let mut line_starts_at = 0usize;
    for _ in 0..rows_before {
        line_starts_at += text.get(line_starts_at..)?.find('\n')? + 1;
    }
    let rest = text.get(line_starts_at..)?;
    let line = rest.split('\n').next().unwrap_or(rest);
    let within: usize = line
        .chars()
        .take(column.saturating_sub(1) as usize)
        .map(char::len_utf8)
        .sum();
    Some(line_starts_at + within)
}

/// ruff calls every finding an error, which would paint a file red over a
/// missing blank line. Only a parse error is one: past that point nothing
/// else can be said about the file at all. A rule that fired is a warning --
/// the code runs, and the reader gets to decide.
fn severity_of(code: Option<&str>) -> lsp::DiagnosticSeverity {
    match code {
        // Older ruff gave a parse error no code rather than this one.
        Some(PARSE_ERROR) | None => lsp::DiagnosticSeverity::ERROR,
        Some(_) => lsp::DiagnosticSeverity::WARNING,
    }
}

/// The position a row and column fall at, counted the way the protocol
/// counts: lines from zero, and characters as UTF-16 code units.
///
/// Three units are in play and they disagree. On the line
/// `    café = "🦀🔥"; total = 1` the semicolon sits at byte 22, at character
/// 15, and at UTF-16 unit 17, all counted from zero. ruff reports the
/// character, from one; the protocol asks for the UTF-16 unit, from zero.
/// Only the text can convert between them, which is why this takes the text.
///
/// A row or column past the end lands at the end of what is there, rather
/// than panicking on a file ruff saw and the editor has since changed.
fn utf16_position_of(text: &str, row: u32, column: u32) -> lsp::Position {
    let line = row.saturating_sub(1);
    let before = column.saturating_sub(1) as usize;
    let character = text
        .lines()
        .nth(line as usize)
        .unwrap_or("")
        .chars()
        .take(before)
        .map(char::len_utf16)
        .sum::<usize>();
    lsp::Position {
        line,
        character: character as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// Real output, captured from `ruff check --output-format json --quiet
    /// --select E,F` over the file [`linted`] holds. Kept verbatim rather
    /// than written by hand: every field this reader depends on is one ruff
    /// actually emits, in the shape it actually emits it.
    const REAL_OUTPUT: &str = include_str!("../test_data/ruff-check.json");

    /// Real output from the same command over a file whose safe fix lands
    /// after an emoji, captured together with the file itself so that every
    /// column in it can be checked against the text it was measured on.
    const WITH_FIXES: &str = include_str!("../test_data/ruff-check-fixes.json");
    const WITH_FIXES_SOURCE: &str = include_str!("../test_data/ruff-check-fixes.source");

    fn over_the_fixture_with_fixes() -> Vec<Reported> {
        what_ruff_reported(WITH_FIXES, Path::new("/project"), |path| {
            (path == Path::new("/project/lint_me.py")).then(|| WITH_FIXES_SOURCE.to_string())
        })
    }

    /// The file the fixture was captured over, byte for byte.
    fn linted() -> String {
        "import os\n\n\ndef add(first, second):\n    café = \"\u{1f980}\u{1f525}\"; total = first + second\n    return total\n"
            .to_string()
    }

    fn over_the_real_file(output: &str) -> Vec<Reported> {
        what_ruff_reported(output, Path::new("/project"), |path| {
            (path == Path::new("/project/lint_me.py")).then(linted)
        })
    }

    /// A plain finding: the unused import on the first line, placed on the
    /// name and not the whole statement, and carrying its rule code so the
    /// reader can look it up or silence it.
    #[test]
    fn a_finding_is_read_with_its_place_its_code_and_a_warning_severity() {
        let reported = over_the_real_file(REAL_OUTPUT);
        assert_eq!(reported.len(), 3, "{reported:?}");

        let unused_import = &reported[0];
        assert_eq!(unused_import.path, Path::new("/project/lint_me.py"));
        assert_eq!(
            unused_import.diagnostic.code,
            Some(lsp::NumberOrString::String("F401".to_string()))
        );
        assert_eq!(unused_import.diagnostic.source.as_deref(), Some("ruff"));
        assert_eq!(
            unused_import.diagnostic.range,
            lsp::Range {
                start: lsp::Position::new(0, 7),
                end: lsp::Position::new(0, 9),
            },
            "`os` on the first line, counted from zero"
        );
        assert!(
            unused_import
                .diagnostic
                .message
                .starts_with("`os` imported"),
            "{}",
            unused_import.diagnostic.message
        );
        assert_eq!(
            unused_import.diagnostic.severity,
            Some(lsp::DiagnosticSeverity::WARNING),
            "a rule that fired is not an error, whatever ruff calls it"
        );
    }

    /// ruff counts a column in Unicode characters, the protocol counts it in
    /// UTF-16 code units, and the file is stored in bytes. All three disagree
    /// on the same line, and reading one as another puts every diagnostic on
    /// a line with an emoji in the wrong column.
    #[test]
    fn a_character_column_becomes_a_utf16_one_and_not_a_byte_or_character_one() {
        let text = linted();
        let line = text.lines().nth(4).expect("the café line");
        let inside = line.find(';').expect("the semicolon");

        // Three counts of the same place, all different.
        assert_eq!(line[..inside].len(), 22, "bytes");
        assert_eq!(line[..inside].chars().count(), 15, "characters");
        assert_eq!(line[..inside].encode_utf16().count(), 17, "UTF-16 units");

        let semicolon = &over_the_real_file(REAL_OUTPUT)[2];
        assert_eq!(
            semicolon.diagnostic.code,
            Some(lsp::NumberOrString::String("E702".to_string()))
        );
        assert_eq!(
            semicolon.diagnostic.range.start,
            lsp::Position::new(4, 17),
            "the protocol's own unit -- not 15, which is what ruff reports"
        );
    }

    /// A finding ruff can fix says so, and says what the fix would do. An
    /// unsafe fix is called one: it can change what the code means, and the
    /// reader deciding whether to take it needs to know that first.
    #[test]
    fn a_fix_is_part_of_what_the_finding_said_and_an_unsafe_one_is_called_so() {
        let reported = over_the_real_file(REAL_OUTPUT);

        assert_eq!(
            reported[0].diagnostic.message,
            "`os` imported but unused\nfix: Remove unused import: `os`"
        );
        assert_eq!(
            reported[1].diagnostic.message,
            "Local variable `café` is assigned to but never used\nfix (unsafe): Remove assignment to unused variable `café`"
        );
    }

    /// A parse error is the one finding that really is an error: past it
    /// nothing else can be said about the file, and ruff stops reporting
    /// rules entirely. Captured from ruff over `def broken(:`.
    #[test]
    fn a_parse_error_is_an_error_and_a_rule_that_fired_is_not() {
        const BROKEN: &str = r#"[
          {
            "cell": null,
            "code": "invalid-syntax",
            "end_location": { "column": 13, "row": 1 },
            "filename": "/project/lint_me.py",
            "fix": null,
            "location": { "column": 12, "row": 1 },
            "message": "Expected a parameter or the end of the parameter list",
            "name": "invalid-syntax",
            "noqa_row": null,
            "severity": "error",
            "url": null
          }
        ]"#;
        let reported = over_the_real_file(BROKEN);
        assert_eq!(reported.len(), 1);
        assert_eq!(
            reported[0].diagnostic.severity,
            Some(lsp::DiagnosticSeverity::ERROR)
        );
    }

    /// A file `read` cannot supply is skipped. The columns are only
    /// meaningful against the text they were measured on, and a diagnostic in
    /// the wrong column is worse than one not shown.
    #[test]
    fn a_file_that_cannot_be_read_is_skipped_rather_than_placed_by_guess() {
        let reported = what_ruff_reported(REAL_OUTPUT, Path::new("/project"), |_| None);
        assert!(reported.is_empty(), "{reported:?}");
    }

    /// Output that is not what this reader expects yields nothing. ruff
    /// writes its own errors to the terminal and leaves stdout empty, and a
    /// diagnostics reader that panicked on that would take the editor down
    /// over a broken `ruff.toml`.
    #[test]
    fn output_that_will_not_parse_is_skipped_rather_than_panicked_on() {
        for output in ["", "ruff failed", "{}", "null", "[1, 2, 3]"] {
            assert!(
                over_the_real_file(output).is_empty(),
                "{output:?} yielded diagnostics"
            );
        }
    }

    /// One entry that will not parse does not cost the ones around it: a
    /// field ruff renames in some later version should lose that finding, not
    /// every finding.
    #[test]
    fn one_unreadable_entry_does_not_cost_the_rest() {
        let mut findings: Vec<serde_json::Value> =
            serde_json::from_str(REAL_OUTPUT).expect("the fixture parses");
        findings.insert(1, serde_json::json!({"filename": "/project/lint_me.py"}));
        let stream = serde_json::to_string(&findings).expect("re-serialised");

        assert_eq!(over_the_real_file(&stream).len(), 3, "the three real ones");
    }

    /// A row or column past the end of the text lands at the end of what is
    /// there. ruff measured them against a file the editor may have changed
    /// since.
    #[test]
    fn a_place_past_the_end_lands_at_the_end() {
        let text = "x = 1\n";
        assert_eq!(
            utf16_position_of(text, 9_000, 1),
            lsp::Position::new(8999, 0)
        );
        assert_eq!(utf16_position_of(text, 1, 9_000), lsp::Position::new(0, 5));
        assert_eq!(utf16_position_of("", 1, 1), lsp::Position::new(0, 0));
    }

    /// Only a fix ruff itself calls safe is passed on. It grades every fix it
    /// computes, and an `unsafe` one changes what the code means -- applying
    /// that at one click is worse than offering nothing.
    #[test]
    fn only_a_fix_ruff_calls_safe_is_kept() {
        let reported = over_the_fixture_with_fixes();

        let graded: Vec<(String, Vec<&str>)> = reported
            .iter()
            .map(|one| {
                let code = match &one.diagnostic.code {
                    Some(lsp::NumberOrString::String(code)) => code.clone(),
                    _ => String::new(),
                };
                (
                    code,
                    one.fixes.iter().map(|fix| fix.title.as_str()).collect(),
                )
            })
            .collect();
        assert_eq!(
            graded,
            vec![
                ("F401".to_string(), vec!["Remove unused import: `os`"]),
                ("E703".to_string(), vec!["Remove unnecessary semicolon"]),
                // Graded unsafe by ruff: `x == None` and `x is None` are not
                // the same comparison for a type that overloads `__eq__`.
                ("E711".to_string(), Vec::new()),
            ]
        );
    }

    /// A fix's edit is placed in the protocol's own unit and carries the text
    /// it expects to replace. ruff counts characters from one, the file is
    /// bytes, and the protocol wants UTF-16 code units -- three numbers that
    /// disagree on the line the safe fix lands on.
    #[test]
    fn a_fixs_edit_is_placed_in_utf16_units_and_carries_what_it_replaces() {
        let line = WITH_FIXES_SOURCE
            .lines()
            .nth(5)
            .expect("the line the semicolon is on");
        let semicolon = line.find(';').expect("the semicolon");

        // Three counts of the same place, all different.
        assert_eq!(line[..semicolon].len(), 33, "bytes");
        assert_eq!(line[..semicolon].chars().count(), 19, "characters");
        assert_eq!(line[..semicolon].encode_utf16().count(), 21, "UTF-16 units");

        let reported = over_the_fixture_with_fixes();
        let fix = reported[1].fixes.first().expect("the semicolon's fix");
        assert_eq!(fix.replacements.len(), 1);
        let replacement = &fix.replacements[0];
        assert_eq!(replacement.path, Path::new("/project/lint_me.py"));
        assert_eq!(
            replacement.range,
            lsp::Range {
                start: lsp::Position::new(5, 21),
                end: lsp::Position::new(5, 22),
            },
            "the protocol's own unit -- not 33, and not the 19 ruff reports"
        );
        assert_eq!(replacement.new_text, "", "ruff asked for a deletion");
        assert_eq!(replacement.replaced, ";");
    }

    /// A fix with no message is dropped even where ruff calls it safe: what
    /// the reader picks from a menu is the title, and an unnamed entry says
    /// nothing about what it will do.
    #[test]
    fn a_fix_with_no_name_is_not_kept() {
        let unnamed = WITH_FIXES.replace(
            "\"message\": \"Remove unnecessary semicolon\"",
            "\"message\": null",
        );
        let reported = what_ruff_reported(&unnamed, Path::new("/project"), |path| {
            (path == Path::new("/project/lint_me.py")).then(|| WITH_FIXES_SOURCE.to_string())
        });
        assert!(reported[1].fixes.is_empty(), "{:?}", reported[1].fixes);
    }

    /// A row past the end of the text yields no byte offset at all. The
    /// report was measured against a file the editor may have changed since,
    /// and a fix placed by guess would overwrite text ruff never looked at.
    #[test]
    fn a_row_past_the_end_has_no_byte_offset() {
        let text = "x = 1\ny = 2\n";
        assert_eq!(byte_offset_of(text, 2, 1), Some(6));
        assert_eq!(byte_offset_of(text, 9_000, 1), None);
        assert_eq!(byte_offset_of(text, 0, 1), None);
        // A column past the end of its line lands at that line's end.
        assert_eq!(byte_offset_of(text, 1, 9_000), Some(5));
    }
}
