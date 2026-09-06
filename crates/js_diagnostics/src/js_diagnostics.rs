use std::path::{Path, PathBuf};

use collections::HashMap;
use serde::Deserialize;

mod watching;

pub use watching::{Lint, init};

/// One finding the linter reported, at a place the editor can put it.
///
/// The range is in the units the protocol means by a character -- UTF-16 code
/// units -- because that is what this editor's own conversion reads. oxlint
/// counts bytes; see [`utf16_position_at`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reported {
    /// Absolute, so a caller does not have to remember where oxlint ran.
    pub path: PathBuf,
    pub diagnostic: lsp::Diagnostic,
}

/// What `oxlint --format json` writes: one object holding the findings and a few
/// counts about the run, which nothing here needs.
#[derive(Deserialize)]
struct Report {
    diagnostics: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
struct Finding {
    message: String,
    /// The rule that fired, plugin and all -- `eslint(no-debugger)`. A parse
    /// error has none: nothing fired, the file simply would not read.
    #[serde(default)]
    code: Option<String>,
    /// `error` for a parse error, `warning` for a rule. Absent output is
    /// treated as a rule firing, which is the milder reading.
    #[serde(default)]
    severity: Option<String>,
    filename: String,
    #[serde(default)]
    help: Option<String>,
    /// Where the finding is. The first is the primary place; a rule that
    /// needs to point at two things, like a duplicate key, adds the others.
    labels: Vec<Label>,
}

#[derive(Deserialize)]
struct Label {
    span: Span,
}

/// A stretch of a file, counted in bytes from the start of the file.
#[derive(Deserialize)]
struct Span {
    offset: usize,
    length: usize,
}

/// The severity oxlint gives a file it could not parse, as opposed to a rule
/// that fired.
const A_PARSE_ERROR: &str = "error";

/// Reads what the linter reported into diagnostics the editor can show, with
/// no language server anywhere.
///
/// `ran_in` is the directory oxlint was run in; it writes paths relative to
/// that. `read` supplies a file's text, which is needed and not optional:
/// oxlint gives byte offsets, the protocol wants UTF-16 code units, and only
/// the text itself can convert one to the other.
///
/// A file `read` cannot supply is skipped rather than guessed at. Putting a
/// diagnostic on the wrong column is worse than not showing it.
///
/// Nothing at all is returned for output that is not oxlint's report. That is
/// a different answer from an empty report, and the caller must keep it
/// different: oxlint writes its own failures -- an unparseable `.oxlintrc.json`
/// -- as plain text on the same stream, and reading that as "the project is
/// clean" would wipe every finding the reader still has.
pub fn what_oxlint_reported(
    output: &str,
    ran_in: &Path,
    read: impl Fn(&Path) -> Option<String>,
) -> Option<Vec<Reported>> {
    let report = serde_json::from_str::<Report>(output).ok()?;
    let mut reported = Vec::new();
    let mut texts: HashMap<PathBuf, Option<String>> = HashMap::default();
    for finding in report.diagnostics {
        let Ok(finding) = serde_json::from_value::<Finding>(finding) else {
            continue;
        };
        let Some(span) = finding.labels.first().map(|label| &label.span) else {
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
                    start: utf16_position_at(text, span.offset),
                    end: utf16_position_at(text, span.offset.saturating_add(span.length)),
                },
                severity: Some(severity_of(finding.severity.as_deref())),
                code: finding.code.clone().map(lsp::NumberOrString::String),
                source: Some("oxlint".to_string()),
                message: what_it_said(&finding),
                ..Default::default()
            },
            path,
        });
    }
    Some(reported)
}

/// The whole of what one finding says: its own text, and the advice it came
/// with. The advice is the reader's next question after what is wrong, and
/// oxlint gives it in a separate field the editor would otherwise drop.
fn what_it_said(finding: &Finding) -> String {
    let mut said = finding.message.clone();
    if let Some(help) = &finding.help {
        said.push_str("\nhelp: ");
        said.push_str(help);
    }
    said
}

fn severity_of(severity: Option<&str>) -> lsp::DiagnosticSeverity {
    match severity {
        Some(A_PARSE_ERROR) => lsp::DiagnosticSeverity::ERROR,
        _ => lsp::DiagnosticSeverity::WARNING,
    }
}

/// The position a byte offset falls at, counted the way the protocol counts:
/// lines from zero, and characters as UTF-16 code units.
///
/// Three units are in play and they disagree. On the line
/// `    const café = "🦀🔥"; debugger;` the `d` of `debugger` sits at byte 30,
/// at character 23, and at UTF-16 unit 25, all counted from zero within the
/// line. oxlint reports the byte; the protocol asks for the UTF-16 unit. Only
/// the text can convert between them, which is why this takes the text.
///
/// An offset past the end, or inside a character, lands on the nearest
/// boundary at or before it, rather than panicking on a file oxlint saw and
/// the editor has since changed.
fn utf16_position_at(text: &str, offset: usize) -> lsp::Position {
    let mut at = offset.min(text.len());
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    let before = text.get(..at).unwrap_or("");
    let line_starts_at = before.rfind('\n').map_or(0, |newline| newline + 1);
    lsp::Position {
        line: before.matches('\n').count() as u32,
        character: before
            .get(line_starts_at..)
            .unwrap_or("")
            .encode_utf16()
            .count() as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// Real output, captured from `oxlint --format json
    /// --no-error-on-unmatched-pattern .` over [`LINTED`]. Kept verbatim
    /// rather than written by hand: every field this reader depends on is one
    /// oxlint actually emits, in the shape it actually emits it.
    const REAL_OUTPUT: &str = include_str!("../test_data/oxlint-check.json");

    /// Real output from the same command over [`BROKEN`], a file that will
    /// not parse. It is the one case oxlint calls an error rather than a
    /// warning, and it carries no rule code at all.
    const SYNTAX_ERROR_OUTPUT: &str = include_str!("../test_data/oxlint-syntax-error.json");

    /// Real output from the same command over a project with nothing wrong
    /// with it. An empty report is what a file the reader has just fixed
    /// produces, and it has to stay distinguishable from output that is not a
    /// report at all.
    const CLEAN_OUTPUT: &str = include_str!("../test_data/oxlint-clean.json");

    /// The files the fixtures were captured over, byte for byte.
    const LINTED: &str = include_str!("../test_data/lint_me.ts");
    const BROKEN: &str = include_str!("../test_data/bad.ts");

    fn over_the_real_files(output: &str) -> Vec<Reported> {
        what_oxlint_reported(output, Path::new("/project"), |path| {
            match path.to_str().unwrap_or_default() {
                "/project/lint_me.ts" => Some(LINTED.to_string()),
                "/project/bad.ts" => Some(BROKEN.to_string()),
                _ => None,
            }
        })
        .expect("the captured output is oxlint's own report")
    }

    /// A plain finding: the unused import on the first line, placed on the
    /// name and not the whole statement, and carrying its rule code so the
    /// reader can look it up or silence it.
    #[test]
    fn a_finding_is_read_with_its_place_its_code_and_a_warning_severity() {
        let reported = over_the_real_files(REAL_OUTPUT);
        assert_eq!(reported.len(), 3, "{reported:?}");

        let unused_import = &reported[0];
        assert_eq!(unused_import.path, Path::new("/project/lint_me.ts"));
        assert_eq!(
            unused_import.diagnostic.code,
            Some(lsp::NumberOrString::String(
                "eslint(no-unused-vars)".to_string()
            ))
        );
        assert_eq!(unused_import.diagnostic.source.as_deref(), Some("oxlint"));
        assert_eq!(
            unused_import.diagnostic.range,
            lsp::Range {
                start: lsp::Position::new(0, 9),
                end: lsp::Position::new(0, 17),
            },
            "`readFile` on the first line, counted from zero"
        );
        assert!(
            unused_import
                .diagnostic
                .message
                .starts_with("Identifier 'readFile' is imported but never used."),
            "{}",
            unused_import.diagnostic.message
        );
        assert_eq!(
            unused_import.diagnostic.severity,
            Some(lsp::DiagnosticSeverity::WARNING),
            "a rule that fired is not an error"
        );
    }

    /// oxlint counts a column in bytes, the protocol counts it in UTF-16 code
    /// units, and a reader looking at the line counts characters. All three
    /// disagree on the same place, and reading one as another puts every
    /// diagnostic on a line with an emoji in the wrong column.
    #[test]
    fn a_byte_column_becomes_a_utf16_one_and_not_a_byte_or_character_one() {
        let line = LINTED.lines().nth(3).expect("the café line");
        let inside = line.find("debugger").expect("the debugger statement");

        // Three counts of the same place, all different.
        assert_eq!(line[..inside].len(), 30, "bytes");
        assert_eq!(line[..inside].chars().count(), 23, "characters");
        assert_eq!(line[..inside].encode_utf16().count(), 25, "UTF-16 units");

        // What oxlint itself put in the captured output, unread by the code
        // under test: a byte column counted from one, and nothing else.
        let raw: serde_json::Value = serde_json::from_str(REAL_OUTPUT).expect("the fixture parses");
        assert_eq!(
            raw["diagnostics"][2]["labels"][0]["span"]["column"],
            serde_json::json!(31),
            "31 is the byte column from one -- not 24 characters and not 26 UTF-16 units"
        );

        let debugger = &over_the_real_files(REAL_OUTPUT)[2];
        assert_eq!(
            debugger.diagnostic.code,
            Some(lsp::NumberOrString::String(
                "eslint(no-debugger)".to_string()
            ))
        );
        assert_eq!(
            debugger.diagnostic.range.start,
            lsp::Position::new(3, 25),
            "the protocol's own unit -- not 30, which is what oxlint reports"
        );
    }

    /// A finding oxlint has advice about says so. The advice is a separate
    /// field, and an editor that showed only the message would drop the half
    /// that says what to do.
    #[test]
    fn the_advice_is_part_of_what_the_finding_said() {
        let reported = over_the_real_files(REAL_OUTPUT);

        assert_eq!(
            reported[0].diagnostic.message,
            "Identifier 'readFile' is imported but never used.\nhelp: Consider removing this import."
        );
        assert_eq!(
            reported[1].diagnostic.message,
            "Variable 'café' is declared but never used. Unused variables should start with a '_'.\nhelp: Consider removing this declaration."
        );
    }

    /// A parse error is the one finding that really is an error: past it
    /// nothing else can be said about the file, and oxlint stops reporting
    /// rules entirely. It also arrives with no rule code, because no rule
    /// fired.
    #[test]
    fn a_parse_error_is_an_error_and_a_rule_that_fired_is_not() {
        let reported = over_the_real_files(SYNTAX_ERROR_OUTPUT);
        assert_eq!(reported.len(), 1, "{reported:?}");
        assert_eq!(reported[0].path, Path::new("/project/bad.ts"));
        assert_eq!(
            reported[0].diagnostic.severity,
            Some(lsp::DiagnosticSeverity::ERROR)
        );
        assert_eq!(reported[0].diagnostic.code, None);
        assert_eq!(reported[0].diagnostic.message, "Unexpected token");
    }

    /// A file `read` cannot supply is skipped. The offsets are only
    /// meaningful against the text they were measured on, and a diagnostic in
    /// the wrong column is worse than one not shown.
    #[test]
    fn a_file_that_cannot_be_read_is_skipped_rather_than_placed_by_guess() {
        let reported = what_oxlint_reported(REAL_OUTPUT, Path::new("/project"), |_| None)
            .expect("still oxlint's report");
        assert!(reported.is_empty(), "{reported:?}");
    }

    /// Output that is not oxlint's report is nothing at all, which is a
    /// different answer from an empty report. oxlint writes its own failures
    /// -- an unparseable `.oxlintrc.json` -- as plain text on the same stream
    /// it writes findings to, and reading that as a clean project would wipe
    /// every finding the reader still has.
    #[test]
    fn output_that_is_not_the_report_is_nothing_rather_than_a_clean_project() {
        // What oxlint wrote on the same stream when its config file held
        // `{ not json`, with the colour codes taken out.
        const CONFIG_FAILED: &str = "Failed to parse oxlint configuration file.\n\n  × Failed to parse oxlint config /tmp/bad.json.\n  │ key must be a string at line 1 column 3\n";

        for output in [
            CONFIG_FAILED,
            "",
            "oxlint failed",
            "null",
            "{}",
            "[1, 2, 3]",
        ] {
            assert_eq!(
                what_oxlint_reported(output, Path::new("/project"), |_| Some(LINTED.to_string())),
                None,
                "{output:?} was read as a report"
            );
        }
    }

    /// A project with nothing wrong with it reports an empty list, not
    /// nothing at all. The difference is what tells the editor to take down
    /// the findings the reader has just fixed.
    #[test]
    fn a_project_with_nothing_wrong_reports_an_empty_list_rather_than_nothing() {
        assert_eq!(
            what_oxlint_reported(CLEAN_OUTPUT, Path::new("/project"), |_| Some(
                LINTED.to_string()
            )),
            Some(Vec::new())
        );
    }

    /// One entry that will not parse does not cost the ones around it: a
    /// field oxlint renames in some later version should lose that finding,
    /// not every finding.
    #[test]
    fn one_unreadable_entry_does_not_cost_the_rest() {
        let mut report: serde_json::Value =
            serde_json::from_str(REAL_OUTPUT).expect("the fixture parses");
        let Some(diagnostics) = report["diagnostics"].as_array_mut() else {
            panic!("the fixture holds an array of findings");
        };
        diagnostics.insert(1, serde_json::json!({"filename": "lint_me.ts"}));
        let stream = serde_json::to_string(&report).expect("re-serialised");

        assert_eq!(over_the_real_files(&stream).len(), 3, "the three real ones");
    }

    /// A finding with nowhere to put it is skipped rather than landed at the
    /// top of the file, where it would point at code that is not the problem.
    #[test]
    fn a_finding_with_no_place_is_skipped_rather_than_put_at_the_start() {
        let unplaced = serde_json::json!({
            "diagnostics": [{
                "message": "something is wrong somewhere",
                "severity": "warning",
                "filename": "lint_me.ts",
                "labels": []
            }]
        })
        .to_string();

        assert!(over_the_real_files(&unplaced).is_empty());
    }

    /// An offset past the end of the text, or inside a character, lands on
    /// the nearest boundary at or before it. oxlint measured them against a
    /// file the editor may have changed since.
    #[test]
    fn a_place_past_the_end_or_inside_a_character_lands_on_a_boundary() {
        assert_eq!(
            utf16_position_at("x = 1\n", 9_000),
            lsp::Position::new(1, 0)
        );
        assert_eq!(utf16_position_at("", 0), lsp::Position::new(0, 0));
        assert_eq!(utf16_position_at("", 7), lsp::Position::new(0, 0));
        // Inside the four bytes of the crab, which begins at byte 2.
        assert_eq!(utf16_position_at("a 🦀 b", 4), lsp::Position::new(0, 2));
        // And just past it, where its two UTF-16 units have been counted.
        assert_eq!(utf16_position_at("a 🦀 b", 6), lsp::Position::new(0, 4));
    }
}
