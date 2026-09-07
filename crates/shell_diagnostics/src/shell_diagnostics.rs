use serde::Deserialize;

mod watching;

pub use watching::{Check, init};

/// What `shellcheck --format=json1` writes: one object holding one entry per
/// finding.
///
/// The entries are read one at a time rather than as a typed list: a field
/// shellcheck renames in some later version should lose that finding, not
/// every finding.
#[derive(Deserialize)]
struct Report {
    comments: Vec<serde_json::Value>,
}

/// One finding, in the shape shellcheck emits it.
#[derive(Deserialize)]
struct Comment {
    /// The path shellcheck was given, echoed back. A `source` directive plus
    /// `external-sources=true` lets shellcheck report about a file other than
    /// the one it was asked about, and such a finding belongs on that file.
    file: String,
    line: u32,
    #[serde(rename = "endLine")]
    end_line: u32,
    /// Counted in Unicode characters from one -- not bytes, and not the UTF-16
    /// code units the protocol wants; see [`utf16_position_of`].
    column: u32,
    /// Exclusive, and in the same unit as `column`.
    #[serde(rename = "endColumn")]
    end_column: u32,
    /// `error`, `warning`, `info` or `style`. Read as text rather than as an
    /// enum: a grade this reader has never heard of must leave the rest of the
    /// finding readable.
    level: String,
    /// The rule number, without the `SC` the reader knows it by.
    code: u32,
    message: String,
}

/// shellcheck's word for "I cannot lint this shell at all".
///
/// Zed calls one language Shell Script and maps `.zsh`, `.zshrc` and
/// `.zprofile` to it along with `.sh` and `.bash`, and shellcheck reads none
/// of the zsh ones. Its refusal is about the tool rather than about the
/// script, so a file it names is reported on not at all.
const SHELL_NOT_SUPPORTED: u32 = 1071;

/// shellcheck's complaint that it cannot tell which shell a file is for.
///
/// Dropped because Zed maps `.bashrc`, `.envrc`, `.env`, `PKGBUILD` and
/// `.bash_profile` to Shell Script, and a shebang in any of those would be
/// wrong. shellcheck still lints the file as bash once it has said this, so
/// dropping the complaint costs none of its other findings.
const TARGET_SHELL_UNKNOWN: u32 = 2148;

/// Reads what shellcheck reported into diagnostics the editor can show, with
/// no language server anywhere.
///
/// `named` is the argument shellcheck was given, and a finding about anything
/// else is dropped: it belongs on that file, and putting it on this one would
/// underline a line nobody wrote. `text` is that file's own text, and is
/// needed rather than optional -- shellcheck gives character columns, the
/// protocol wants UTF-16 code units, and only the text can convert one to the
/// other.
///
/// Output that will not parse yields nothing rather than panicking, and one
/// unreadable entry does not cost the rest of them.
pub fn what_shellcheck_reported(output: &str, named: &str, text: &str) -> Vec<lsp::Diagnostic> {
    let Ok(report) = serde_json::from_str::<Report>(output) else {
        return Vec::new();
    };
    let mine: Vec<Comment> = report
        .comments
        .into_iter()
        .filter_map(|comment| serde_json::from_value::<Comment>(comment).ok())
        .filter(|comment| comment.file == named)
        .collect();
    if mine
        .iter()
        .any(|comment| comment.code == SHELL_NOT_SUPPORTED)
    {
        return Vec::new();
    }
    mine.iter()
        .filter(|comment| comment.code != TARGET_SHELL_UNKNOWN)
        .map(|comment| lsp::Diagnostic {
            range: at_least_one_character(
                text,
                lsp::Range {
                    start: utf16_position_of(text, comment.line, comment.column),
                    end: utf16_position_of(text, comment.end_line, comment.end_column),
                },
            ),
            severity: Some(severity_of(&comment.level)),
            code: Some(lsp::NumberOrString::String(format!("SC{}", comment.code))),
            source: Some("shellcheck".to_string()),
            message: comment.message.clone(),
            ..Default::default()
        })
        .collect()
}

/// shellcheck points at a place rather than a span for most of what it finds
/// about a script's structure -- a `[` it could not parse comes back with the
/// same start and end column. An empty range underlines nothing, so it is
/// widened to the one character it points at, or to the line's last character
/// where it points past the end.
fn at_least_one_character(text: &str, range: lsp::Range) -> lsp::Range {
    if range.start != range.end {
        return range;
    }
    let units = utf16_units_in_line(text, range.start.line);
    let mut widened = range;
    if range.start.character < units {
        widened.end.character = range.start.character + 1;
    } else {
        widened.start.character = range.start.character.saturating_sub(1);
    }
    widened
}

fn utf16_units_in_line(text: &str, line: u32) -> u32 {
    text.lines()
        .nth(line as usize)
        .map(|line| line.encode_utf16().count() as u32)
        .unwrap_or(0)
}

/// shellcheck grades every finding, and the grades mean different things to a
/// reader: a parse failure stops it from saying anything else about the file,
/// while a style note is a preference the reader gets to decline. Flattening
/// them to one grade would paint a script red over a legacy backtick.
///
/// A grade this reader has never heard of is kept as a warning rather than
/// dropped: a finding shown at the wrong grade is still a finding, and one
/// silently discarded is not.
fn severity_of(level: &str) -> lsp::DiagnosticSeverity {
    match level {
        "error" => lsp::DiagnosticSeverity::ERROR,
        "info" => lsp::DiagnosticSeverity::INFORMATION,
        "style" => lsp::DiagnosticSeverity::HINT,
        _ => lsp::DiagnosticSeverity::WARNING,
    }
}

/// The position a line and column fall at, counted the way the protocol
/// counts: lines from zero, and characters as UTF-16 code units.
///
/// Three units are in play and they disagree. On the line
/// `label="café 🦀🔥"; echo $label` the `$` sits at byte 29, at character 22
/// and at UTF-16 unit 24, all counted from zero. shellcheck reports the
/// character, from one; the protocol asks for the UTF-16 unit, from zero.
/// Only the text can convert between them, which is why this takes the text.
///
/// A line or column past the end lands at the end of what is there, rather
/// than panicking on a file shellcheck read and the editor has since changed.
fn utf16_position_of(text: &str, line: u32, column: u32) -> lsp::Position {
    let line = line.saturating_sub(1);
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

    /// Real output, captured from `shellcheck --format=json1 lint_me.sh` over
    /// the script [`LINTED`] holds, both kept verbatim: every field this
    /// reader depends on is one shellcheck actually emits, in the shape it
    /// actually emits it, and every column in the report was measured on this
    /// exact text.
    const REAL_OUTPUT: &str = include_str!("../test_data/shellcheck-check.json");
    const LINTED: &str = include_str!("../test_data/lint_me.sh.source");

    /// Real output over a script shellcheck cannot parse at all.
    const UNPARSABLE: &str = include_str!("../test_data/shellcheck-unparsable.json");

    /// Real output over a script with nothing wrong with it.
    const CLEAN: &str = include_str!("../test_data/shellcheck-clean.json");

    /// Real output over a `#!/bin/zsh` script, which shellcheck refuses.
    const ZSH: &str = include_str!("../test_data/shellcheck-zsh.json");

    fn over_the_real_script(output: &str) -> Vec<lsp::Diagnostic> {
        what_shellcheck_reported(output, "lint_me.sh", LINTED)
    }

    fn codes_of(found: &[lsp::Diagnostic]) -> Vec<(String, Option<lsp::DiagnosticSeverity>)> {
        found
            .iter()
            .map(|one| {
                let code = match &one.code {
                    Some(lsp::NumberOrString::String(code)) => code.clone(),
                    _ => String::new(),
                };
                (code, one.severity)
            })
            .collect()
    }

    /// Every finding keeps its own rule number and its own grade. The reader
    /// silences a rule by number, and a style note must not paint the script
    /// the same colour as a parse failure.
    #[test]
    fn every_finding_keeps_its_own_code_and_its_own_grade() {
        let found = over_the_real_script(REAL_OUTPUT);
        assert_eq!(
            codes_of(&found),
            vec![
                (
                    "SC2086".to_string(),
                    Some(lsp::DiagnosticSeverity::INFORMATION)
                ),
                ("SC2164".to_string(), Some(lsp::DiagnosticSeverity::WARNING)),
                ("SC2115".to_string(), Some(lsp::DiagnosticSeverity::WARNING)),
                ("SC2006".to_string(), Some(lsp::DiagnosticSeverity::HINT)),
                (
                    "SC2086".to_string(),
                    Some(lsp::DiagnosticSeverity::INFORMATION)
                ),
            ]
        );
        assert_eq!(found[0].source.as_deref(), Some("shellcheck"));
        assert_eq!(
            found[0].message,
            "Double quote to prevent globbing and word splitting."
        );
    }

    /// shellcheck counts a column in Unicode characters, the protocol counts
    /// it in UTF-16 code units, and the file is stored in bytes. All three
    /// disagree on the same line, and reading one as another puts every
    /// diagnostic on a line with an emoji in the wrong column.
    #[test]
    fn a_character_column_becomes_a_utf16_one_and_not_a_byte_or_character_one() {
        let line = LINTED.lines().nth(1).expect("the café line");
        let expansion = line.find("$label").expect("the expansion");

        // Three counts of the same place, all different.
        assert_eq!(line[..expansion].len(), 29, "bytes");
        assert_eq!(line[..expansion].chars().count(), 22, "characters");
        assert_eq!(line[..expansion].encode_utf16().count(), 24, "UTF-16 units");

        let found = over_the_real_script(REAL_OUTPUT);
        assert_eq!(
            found[0].range,
            lsp::Range {
                start: lsp::Position::new(1, 24),
                end: lsp::Position::new(1, 30),
            },
            "the protocol's own unit -- not the 29 of bytes, and not the 22 shellcheck reports"
        );
    }

    /// A script shellcheck cannot parse is still reported on. The findings
    /// that say why are the most useful ones in the file, and a reader who
    /// gets silence instead has to go looking for the fault by hand.
    #[test]
    fn a_script_that_will_not_parse_is_reported_on_rather_than_dropped() {
        let found =
            what_shellcheck_reported(UNPARSABLE, "unparsable.sh", "if [ 1\nthen\n  echo hi\n");
        assert_eq!(
            codes_of(&found),
            vec![
                (
                    "SC1009".to_string(),
                    Some(lsp::DiagnosticSeverity::INFORMATION)
                ),
                ("SC1073".to_string(), Some(lsp::DiagnosticSeverity::ERROR)),
                ("SC1080".to_string(), Some(lsp::DiagnosticSeverity::ERROR)),
                ("SC1072".to_string(), Some(lsp::DiagnosticSeverity::ERROR)),
            ]
        );
    }

    /// A place rather than a span is widened to the character it points at.
    /// shellcheck reports most of what it finds about a script's structure
    /// with the same start and end column, and an empty range underlines
    /// nothing at all.
    #[test]
    fn a_place_rather_than_a_span_still_underlines_a_character() {
        let found =
            what_shellcheck_reported(UNPARSABLE, "unparsable.sh", "if [ 1\nthen\n  echo hi\n");
        assert_eq!(
            found[1].range,
            lsp::Range {
                start: lsp::Position::new(0, 3),
                end: lsp::Position::new(0, 4),
            },
            "shellcheck gave column 4 twice"
        );
        // A place at the very end of a line is widened backwards instead,
        // which is the only direction left.
        assert_eq!(
            at_least_one_character(
                "ab\n",
                lsp::Range {
                    start: lsp::Position::new(0, 2),
                    end: lsp::Position::new(0, 2),
                }
            ),
            lsp::Range {
                start: lsp::Position::new(0, 1),
                end: lsp::Position::new(0, 2),
            }
        );
    }

    /// A script with nothing wrong with it reports nothing, which is what
    /// takes down the underlining of a fault the reader has just fixed. The
    /// editor keeps what it was last given, so an empty report is a different
    /// thing from silence.
    #[test]
    fn a_fixed_script_reports_nothing_at_all() {
        assert_eq!(
            what_shellcheck_reported(CLEAN, "clean.sh", "#!/bin/bash\necho \"ok\"\n"),
            Vec::new()
        );
    }

    /// Zed maps `.zsh` and `.zshrc` to the same language as `.sh`, and
    /// shellcheck reads none of them. Its refusal is about the tool and not
    /// about the script, so nothing is shown -- least of all the refusal
    /// itself, underlined on the shebang.
    #[test]
    fn a_shell_shellcheck_refuses_is_reported_on_not_at_all() {
        assert_eq!(
            what_shellcheck_reported(ZSH, "z.zsh", "#!/bin/zsh\necho $x\n"),
            Vec::new()
        );
    }

    /// The missing-shebang complaint is dropped: Zed maps `.bashrc`, `.envrc`
    /// and `PKGBUILD` to Shell Script, where a shebang would be wrong, and
    /// shellcheck lints the file as bash anyway once it has said this.
    #[test]
    fn the_missing_shebang_complaint_is_dropped_and_the_rest_is_kept() {
        const NO_SHEBANG: &str = r#"{"comments":[
          {"file":"noshebang.sh","line":1,"endLine":1,"column":1,"endColumn":1,
           "level":"error","code":2148,
           "message":"Tips depend on target shell and yours is unknown. Add a shebang or a 'shell' directive.","fix":null},
          {"file":"noshebang.sh","line":1,"endLine":1,"column":6,"endColumn":8,
           "level":"warning","code":2154,
           "message":"x is referenced but not assigned.","fix":null}
        ]}"#;
        let found = what_shellcheck_reported(NO_SHEBANG, "noshebang.sh", "echo $x\n");
        assert_eq!(
            codes_of(&found),
            vec![("SC2154".to_string(), Some(lsp::DiagnosticSeverity::WARNING))]
        );
    }

    /// A finding about another file belongs on that file. A `source`
    /// directive plus `external-sources=true` lets shellcheck report about
    /// one, and placing it here would underline a line nobody wrote.
    #[test]
    fn a_finding_about_another_file_is_not_placed_on_this_one() {
        let elsewhere = REAL_OUTPUT.replace("lint_me.sh", "helpers.sh");
        assert_eq!(
            what_shellcheck_reported(&elsewhere, "lint_me.sh", LINTED),
            Vec::new()
        );
    }

    /// Output that is not what this reader expects yields nothing. shellcheck
    /// writes the reason it could not run as plain text, and a reader that
    /// panicked on that would take the editor down over a broken
    /// `.shellcheckrc`.
    #[test]
    fn output_that_will_not_parse_is_skipped_rather_than_panicked_on() {
        for output in ["", "shellcheck: no such file", "[]", "null", "{}"] {
            assert!(
                over_the_real_script(output).is_empty(),
                "{output:?} yielded diagnostics"
            );
        }
    }

    /// One entry that will not parse does not cost the ones around it: a
    /// field shellcheck renames in some later version should lose that
    /// finding, not every finding.
    #[test]
    fn one_unreadable_entry_does_not_cost_the_rest() {
        let mut report: serde_json::Value =
            serde_json::from_str(REAL_OUTPUT).expect("the fixture parses");
        let comments = report["comments"]
            .as_array_mut()
            .expect("the fixture holds a list");
        comments.insert(1, serde_json::json!({ "file": "lint_me.sh" }));
        let stream = serde_json::to_string(&report).expect("re-serialised");

        assert_eq!(
            over_the_real_script(&stream).len(),
            5,
            "the five real findings"
        );
    }

    /// A grade this reader has never heard of keeps its finding, as a
    /// warning. A finding shown at the wrong grade is still a finding; one
    /// silently discarded is not.
    #[test]
    fn a_grade_this_reader_does_not_know_keeps_its_finding() {
        let renamed = REAL_OUTPUT.replace("\"style\"", "\"pedantic\"");
        let found = over_the_real_script(&renamed);
        assert_eq!(
            found[3].severity,
            Some(lsp::DiagnosticSeverity::WARNING),
            "SC2006, whose grade was renamed"
        );
    }

    /// A line or column past the end of the text lands at the end of what is
    /// there. shellcheck measured them against a file the editor may have
    /// changed since.
    #[test]
    fn a_place_past_the_end_lands_at_the_end() {
        assert_eq!(
            utf16_position_of("x=1\n", 9_000, 1),
            lsp::Position::new(8999, 0)
        );
        assert_eq!(
            utf16_position_of("x=1\n", 1, 9_000),
            lsp::Position::new(0, 3)
        );
        assert_eq!(utf16_position_of("", 1, 1), lsp::Position::new(0, 0));
    }
}
