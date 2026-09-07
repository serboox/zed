use std::path::{Path, PathBuf};

use collections::HashMap;
use serde::Deserialize;

mod type_watching;
mod watching;

pub use type_watching::CheckTypes;
pub use watching::Lint;

/// Registers both sources of Python diagnostics this editor has with no
/// language server running: the linter, and `ty`'s type checker.
pub fn init(cx: &mut gpui::App) {
    watching::init(cx);
    type_watching::init(cx);
}

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
    fix: Option<Fix>,
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
struct Fix {
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    applicability: Option<String>,
}

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
        reported.push(Reported {
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
}
