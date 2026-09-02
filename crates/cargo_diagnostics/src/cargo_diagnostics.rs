use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::Deserialize;

mod watching;

pub use watching::{Check, init};

/// One diagnostic the compiler reported, at a place the editor can put it.
///
/// The range is in the units the protocol means by a character -- UTF-16 code
/// units -- because that is what this editor's own conversion reads. The
/// compiler counts something else; see [`utf16_position_of`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reported {
    /// Absolute, so a caller does not have to remember where cargo ran.
    pub path: PathBuf,
    pub diagnostic: lsp::Diagnostic,
}

/// What `cargo check --message-format=json` writes: one JSON object per line,
/// of which only some are diagnostics.
#[derive(Deserialize)]
struct Line {
    reason: String,
    #[serde(default)]
    message: Option<Message>,
}

#[derive(Deserialize)]
struct Message {
    level: String,
    message: String,
    #[serde(default)]
    code: Option<Code>,
    #[serde(default)]
    spans: Vec<Span>,
    #[serde(default)]
    children: Vec<Message>,
}

#[derive(Deserialize)]
struct Code {
    code: String,
}

#[derive(Deserialize)]
struct Span {
    file_name: String,
    is_primary: bool,
    /// Absolute byte offsets into the file. The one unambiguous thing in a
    /// span: `column_start` counts Unicode characters, which is neither what
    /// the file is stored in nor what the protocol asks for.
    byte_start: usize,
    byte_end: usize,
    #[serde(default)]
    label: Option<String>,
}

/// Reads what the compiler reported into diagnostics the editor can show,
/// with no language server anywhere.
///
/// `ran_in` is the directory cargo was run in, because the paths in its
/// output are relative to that and to nothing else. `read` supplies a file's
/// text, which is needed and not optional: the compiler gives byte offsets,
/// the protocol wants UTF-16 code units, and only the text itself can convert
/// one to the other.
///
/// A file `read` cannot supply is skipped rather than guessed at. Putting a
/// diagnostic on the wrong line is worse than not showing it.
pub fn what_the_compiler_reported(
    output: &str,
    ran_in: &Path,
    read: impl Fn(&Path) -> Option<String>,
) -> Vec<Reported> {
    let mut reported = Vec::new();
    // The compiler says the same thing once per target it was asked to check,
    // so `--all-targets` reports every error in a library twice. Identical
    // twice over is once.
    let mut already: HashSet<(PathBuf, lsp::Range, String)> = HashSet::new();
    for line in output.lines() {
        let Ok(line) = serde_json::from_str::<Line>(line) else {
            continue;
        };
        if line.reason != "compiler-message" {
            continue;
        }
        let Some(message) = line.message else {
            continue;
        };
        let Some(severity) = severity_of(&message.level) else {
            continue;
        };
        // The notes that close a failing build -- "aborting due to 3 previous
        // errors" -- carry no span, and there is nowhere to put them.
        let Some(span) = message.spans.iter().find(|span| span.is_primary) else {
            continue;
        };
        let path = ran_in.join(&span.file_name);
        let Some(text) = read(&path) else {
            continue;
        };
        let range = lsp::Range {
            start: utf16_position_of(&text, span.byte_start),
            end: utf16_position_of(&text, span.byte_end),
        };
        let said = what_it_said(&message, span);
        if !already.insert((path.clone(), range, said.clone())) {
            continue;
        }
        reported.push(Reported {
            path,
            diagnostic: lsp::Diagnostic {
                range,
                severity: Some(severity),
                code: message
                    .code
                    .as_ref()
                    .map(|code| lsp::NumberOrString::String(code.code.clone())),
                source: Some("cargo".to_string()),
                message: said,
                ..Default::default()
            },
        });
    }
    reported
}

/// The whole of what one message says: its own text, the label the compiler
/// wrote under the primary span, and whatever its children add. The children
/// are where the useful half usually is -- "help: try adding a conversion" --
/// and dropping them would leave the reader with the diagnosis and none of
/// the advice.
fn what_it_said(message: &Message, primary: &Span) -> String {
    let mut said = message.message.clone();
    if let Some(label) = &primary.label
        && !label.is_empty()
    {
        said.push_str(": ");
        said.push_str(label);
    }
    for child in &message.children {
        if child.message.is_empty() {
            continue;
        }
        said.push('\n');
        said.push_str(&child.level);
        said.push_str(": ");
        said.push_str(&child.message);
    }
    said
}

fn severity_of(level: &str) -> Option<lsp::DiagnosticSeverity> {
    match level {
        "error" | "error: internal compiler error" => Some(lsp::DiagnosticSeverity::ERROR),
        "warning" => Some(lsp::DiagnosticSeverity::WARNING),
        "note" => Some(lsp::DiagnosticSeverity::INFORMATION),
        "help" => Some(lsp::DiagnosticSeverity::HINT),
        // `failure-note` is the summary a failing build ends with. It is not
        // a diagnostic about a place in the code, and it has no span.
        _ => None,
    }
}

/// The position an absolute byte offset falls at, counted the way the
/// protocol counts: lines from zero, and characters as UTF-16 code units.
///
/// Three units are in play and they disagree. On the line
/// `let _ = "🦀🔥"; "a string"` the second string starts at byte 24, at
/// character 18, and at UTF-16 unit 20. The compiler reports the character;
/// the protocol asks for the UTF-16 unit; the file is stored in bytes. Only
/// the text can convert between them, which is why this takes the text.
///
/// An offset past the end of the text lands at the end of it, rather than
/// panicking on a file the compiler saw and the editor has since changed.
fn utf16_position_of(text: &str, byte: usize) -> lsp::Position {
    let byte = byte.min(text.len());
    let mut line = 0u32;
    let mut line_started_at = 0usize;
    for (at, character) in text.char_indices() {
        if at >= byte {
            break;
        }
        if character == '\n' {
            line += 1;
            line_started_at = at + character.len_utf8();
        }
    }
    // A byte offset that falls inside a character belongs to that character,
    // so the count is over what is wholly before it.
    let up_to = text
        .get(line_started_at..byte)
        .unwrap_or_else(|| &text[line_started_at..]);
    lsp::Position {
        line,
        character: up_to.encode_utf16().count() as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// Real output, captured from `cargo check --message-format=json
    /// --all-targets` over a crate with one type error and one unused
    /// variable. Kept verbatim rather than written by hand: every field this
    /// reader depends on is one the compiler actually emits, in the shape it
    /// actually emits it.
    const REAL_OUTPUT: &str = include_str!("../test_data/cargo-check.json");

    fn library() -> String {
        "pub fn wrong() -> u32 {\n    \"a string\"\n}\n\npub fn unused() {\n    let never_read = 1;\n}\n"
            .to_string()
    }

    #[test]
    fn the_two_diagnostics_in_a_real_report_are_read_with_their_places_and_codes() {
        let reported = what_the_compiler_reported(REAL_OUTPUT, Path::new("/project"), |path| {
            (path == Path::new("/project/src/lib.rs")).then(library)
        });

        assert_eq!(
            reported.len(),
            2,
            "one error and one warning; found {:?}",
            reported
                .iter()
                .map(|one| &one.diagnostic.message)
                .collect::<Vec<_>>()
        );

        let error = &reported[0];
        assert_eq!(error.path, Path::new("/project/src/lib.rs"));
        assert_eq!(
            error.diagnostic.severity,
            Some(lsp::DiagnosticSeverity::ERROR)
        );
        assert_eq!(
            error.diagnostic.code,
            Some(lsp::NumberOrString::String("E0308".to_string()))
        );
        assert_eq!(error.diagnostic.source.as_deref(), Some("cargo"));
        // The second line, and the string literal on it -- not the function
        // on the first line, which is where the *other* span of the same
        // message points.
        assert_eq!(error.diagnostic.range.start.line, 1);
        assert_eq!(error.diagnostic.range.start.character, 4);
        assert_eq!(error.diagnostic.range.end.character, 14);
        assert!(
            error.diagnostic.message.starts_with("mismatched types"),
            "{}",
            error.diagnostic.message
        );
        assert!(
            error.diagnostic.message.contains("expected `u32`"),
            "the label under the primary span is part of what it said: {}",
            error.diagnostic.message
        );

        let warning = &reported[1];
        assert_eq!(
            warning.diagnostic.severity,
            Some(lsp::DiagnosticSeverity::WARNING)
        );
        assert_eq!(warning.diagnostic.range.start.line, 5);
        assert!(
            warning.diagnostic.message.contains("never_read"),
            "{}",
            warning.diagnostic.message
        );
    }

    /// `--all-targets` asks the compiler to check a library and its test
    /// target, and it says the same thing about both -- so the real output
    /// above holds every message twice. Showing an error twice in the same
    /// place is a bug the reader sees immediately.
    #[test]
    fn the_same_thing_said_once_per_target_is_shown_once() {
        let said_twice = REAL_OUTPUT
            .lines()
            .filter(|line| line.contains("\"mismatched types\""))
            .count();
        assert_eq!(
            said_twice, 2,
            "the fixture is only meaningful while the compiler still repeats itself"
        );

        let reported =
            what_the_compiler_reported(REAL_OUTPUT, Path::new("/project"), |_| Some(library()));
        assert_eq!(
            reported
                .iter()
                .filter(|one| one.diagnostic.message.starts_with("mismatched types"))
                .count(),
            1
        );
    }

    /// The note a failing build ends with -- "For more information about this
    /// error, try `rustc --explain E0308`" -- is not about a place in the
    /// code and carries no span. There is nowhere to put it, and inventing
    /// somewhere would put it on the first line of a file at random.
    #[test]
    fn a_note_with_no_place_is_not_given_one() {
        assert!(
            REAL_OUTPUT.contains("failure-note"),
            "the fixture is only meaningful while the compiler still sends one"
        );
        let reported =
            what_the_compiler_reported(REAL_OUTPUT, Path::new("/project"), |_| Some(library()));
        assert!(
            !reported
                .iter()
                .any(|one| one.diagnostic.message.contains("For more information")),
            "{reported:?}"
        );
    }

    /// The compiler counts a column in Unicode characters, the protocol counts
    /// it in UTF-16 code units, and the file is stored in bytes. All three
    /// disagree on the same line, and reading one as another puts every
    /// diagnostic on a line with an emoji in the wrong column.
    #[test]
    fn a_byte_offset_becomes_a_utf16_column_and_not_a_character_one() {
        let text = "fn wrong() -> u32 {\n    let _ = \"\u{1f980}\u{1f525}\"; \"a string\"\n}\n";
        let line = text.lines().nth(1).expect("the second line");
        let inside = line.find("\"a string\"").expect("the second string");
        let byte = text.find(line).expect("the line's own offset") + inside;

        // Three counts of the same place, all different.
        assert_eq!(line[..inside].len(), 24, "bytes");
        assert_eq!(line[..inside].chars().count(), 18, "characters");
        assert_eq!(line[..inside].encode_utf16().count(), 20, "UTF-16 units");

        let at = utf16_position_of(text, byte);
        assert_eq!(at.line, 1);
        assert_eq!(
            at.character, 20,
            "the protocol's own unit -- not 18, which is what the compiler reports"
        );
    }

    /// A file the compiler saw and the editor cannot read is skipped. The
    /// offsets are only meaningful against the text they were measured on,
    /// and a diagnostic on the wrong line is worse than one not shown.
    #[test]
    fn a_file_that_cannot_be_read_is_skipped_rather_than_placed_by_guess() {
        let reported = what_the_compiler_reported(REAL_OUTPUT, Path::new("/project"), |_| None);
        assert!(reported.is_empty(), "{reported:?}");
    }

    /// An offset past the end of the text lands at the end of it. The
    /// compiler measured its offsets against a file the editor may have
    /// changed since, and a panic in a diagnostics reader would take the
    /// editor down over a stale byte count.
    #[test]
    fn an_offset_past_the_end_lands_at_the_end() {
        let text = "fn one() {}\n";
        let at = utf16_position_of(text, 9_000);
        assert_eq!(at.line, 1);
        assert_eq!(at.character, 0);
        assert_eq!(utf16_position_of("", 4), lsp::Position::new(0, 0));
    }

    /// Nothing in a stream that is not a diagnostic becomes one: the artifact
    /// lines, the build's own summary, and anything that is not JSON at all
    /// -- cargo writes progress to the same stream in some configurations.
    #[test]
    fn only_the_compilers_own_messages_are_read() {
        let stream = format!(
            "{}\n{}\n{}\n{}\n",
            "not json at all",
            r#"{"reason":"compiler-artifact","package_id":"scratch 0.1.0","target":{"name":"scratch"}}"#,
            r#"{"reason":"build-finished","success":false}"#,
            REAL_OUTPUT.lines().next().expect("one real message"),
        );
        let reported =
            what_the_compiler_reported(&stream, Path::new("/project"), |_| Some(library()));
        assert_eq!(reported.len(), 1, "{reported:?}");
    }
}
