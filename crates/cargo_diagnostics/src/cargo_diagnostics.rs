use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;

mod fixing;
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
    /// The fixes the compiler computed for this diagnostic and handed over
    /// with it, in the order it listed them.
    pub fixes: Vec<Fix>,
}

/// A fix the compiler wrote itself, offered word for word as it gave it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fix {
    /// The compiler's own words for it -- "consider borrowing here".
    pub title: String,
    pub replacements: Vec<Replacement>,
}

/// One piece of text the compiler asked to be put somewhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replacement {
    /// Absolute, like [`Reported::path`]: a fix may reach a file other than
    /// the one the diagnostic is in.
    pub path: PathBuf,
    /// In UTF-16 code units, like [`Reported`]'s range and for the same
    /// reason.
    pub range: lsp::Range,
    /// Empty where the compiler asked for the range to be deleted, which is
    /// a fix like any other and not the absence of one.
    pub new_text: String,
    /// The text that was in `range` when the compiler measured it. A caller
    /// compares it against what is there now: a file edited since the check
    /// has moved every offset in the report, and a replacement made against
    /// moved offsets overwrites something the compiler never looked at.
    pub replaced: String,
}

/// The one grade of suggestion this reader offers.
///
/// The compiler grades every suggestion it makes, and only this grade is a
/// fix rather than a guess. `HasPlaceholders` replacements contain literal
/// placeholder text -- `todo!()`, `_` -- that would be written into the
/// buffer verbatim, and `MaybeIncorrect` ones are the compiler's guess at
/// what was meant, which it says out loud is often wrong. Applying either at
/// one click would put something in the file that nobody asked for, and that
/// is worse than offering nothing.
const ONLY_GRADE_OFFERED: &str = "MachineApplicable";

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
    /// The text the compiler asked to put over `byte_start..byte_end`.
    /// Absent where the span only points at something; present and empty
    /// where the compiler asked for a deletion.
    #[serde(default)]
    suggested_replacement: Option<String>,
    /// Read as text rather than as an enum: a grade this reader has never
    /// heard of must leave the rest of the message readable, and an unknown
    /// variant would fail the whole line instead.
    #[serde(default)]
    suggestion_applicability: Option<String>,
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
    // A file is read once however many spans land in it: one message's fix
    // can name several places, and `--all-targets` sends every message
    // twice.
    let mut texts: HashMap<PathBuf, Option<String>> = HashMap::new();
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
        let Some(text) = text_of(&mut texts, &path, &read) else {
            continue;
        };
        let range = lsp::Range {
            start: utf16_position_of(text, span.byte_start),
            end: utf16_position_of(text, span.byte_end),
        };
        let said = what_it_said(&message, span);
        if !already.insert((path.clone(), range, said.clone())) {
            continue;
        }
        let related_information = the_other_places(&message, ran_in, &mut texts, &read);
        let fixes = fixes_in(&message, ran_in, &mut texts, &read);
        reported.push(Reported {
            path,
            fixes,
            diagnostic: lsp::Diagnostic {
                range,
                severity: Some(severity),
                code: message
                    .code
                    .as_ref()
                    .map(|code| lsp::NumberOrString::String(code.code.clone())),
                source: Some("cargo".to_string()),
                message: said,
                related_information,
                ..Default::default()
            },
        });
    }
    reported
}

/// A file's text, read at most once however many spans point into it.
///
/// A file the reader cannot supply is remembered as unreadable, so a
/// workspace of generated files the editor has no worktree for is not read
/// once per span.
fn text_of<'a>(
    texts: &'a mut HashMap<PathBuf, Option<String>>,
    path: &Path,
    read: &impl Fn(&Path) -> Option<String>,
) -> Option<&'a str> {
    texts
        .entry(path.to_path_buf())
        .or_insert_with(|| read(path))
        .as_deref()
}

/// Every place this message points at except the one it is filed under, each
/// keeping its own position.
///
/// A type error is about two places -- the expression and the return type it
/// has to match -- and a reassignment is about two more, the second write and
/// the first. The message names only one of them; the other spans are where
/// the rest of the answer is, and gluing their labels into the text would
/// leave the reader looking for a line the editor could have jumped to.
///
/// A span the compiler gave no label for is dropped: the editor shows a
/// related place by its message, so one with nothing to say would draw an
/// empty row over the reader's code. So is a span in a file that cannot be
/// read, because only that file's text can place it.
///
/// Only this message's own spans, and not its children's: a child's spans are
/// where its suggestion goes, and those are already offered as fixes.
fn the_other_places(
    message: &Message,
    ran_in: &Path,
    texts: &mut HashMap<PathBuf, Option<String>>,
    read: &impl Fn(&Path) -> Option<String>,
) -> Option<Vec<lsp::DiagnosticRelatedInformation>> {
    let mut places = Vec::new();
    for span in &message.spans {
        if span.is_primary {
            continue;
        }
        let Some(label) = span.label.as_deref().filter(|label| !label.is_empty()) else {
            continue;
        };
        let path = ran_in.join(&span.file_name);
        let Ok(uri) = lsp::Uri::from_file_path(&path) else {
            continue;
        };
        let Some(text) = text_of(texts, &path, read) else {
            continue;
        };
        places.push(lsp::DiagnosticRelatedInformation {
            location: lsp::Location {
                uri,
                range: lsp::Range {
                    start: utf16_position_of(text, span.byte_start),
                    end: utf16_position_of(text, span.byte_end),
                },
            },
            message: label.to_string(),
        });
    }
    (!places.is_empty()).then_some(places)
}

/// Every fix this message handed over, its own and its children's.
///
/// The fix almost always arrives in a child, whose `message` is the fix's
/// title -- "consider borrowing here" -- and whose spans carry the text to
/// put where. One child is one fix, however many places it touches: the
/// compiler's suggestions to add a `&` and to remove the matching `*` are
/// halves of the same change, and applying one without the other leaves the
/// file worse than before.
///
/// Only [`ONLY_GRADE_OFFERED`] is kept; see there for why.
fn fixes_in(
    message: &Message,
    ran_in: &Path,
    texts: &mut HashMap<PathBuf, Option<String>>,
    read: &impl Fn(&Path) -> Option<String>,
) -> Vec<Fix> {
    let mut fixes = Vec::new();
    let mut replacements = Vec::new();
    for span in &message.spans {
        let Some(new_text) = &span.suggested_replacement else {
            continue;
        };
        if span.suggestion_applicability.as_deref() != Some(ONLY_GRADE_OFFERED) {
            continue;
        }
        let path = ran_in.join(&span.file_name);
        let Some(text) = text_of(texts, &path, read) else {
            continue;
        };
        // A range the text cannot be sliced by has been measured against a
        // different file than the one being read, and an empty string here
        // would be indistinguishable from the compiler asking for an
        // insertion.
        let Some(replaced) = text.get(span.byte_start..span.byte_end) else {
            continue;
        };
        let replaced = replaced.to_string();
        replacements.push(Replacement {
            path,
            range: lsp::Range {
                start: utf16_position_of(text, span.byte_start),
                end: utf16_position_of(text, span.byte_end),
            },
            new_text: new_text.clone(),
            replaced,
        });
    }
    // A fix with no name cannot be offered: what the reader picks from a menu
    // is the title, and an unnamed entry says nothing about what it will do.
    if !replacements.is_empty() && !message.message.is_empty() {
        fixes.push(Fix {
            title: message.message.clone(),
            replacements,
        });
    }
    for child in &message.children {
        fixes.extend(fixes_in(child, ran_in, texts, read));
    }
    fixes
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

    /// Real output, captured the same way, over a crate written to provoke a
    /// suggestion of every grade the compiler hands out: two
    /// `MachineApplicable` ones -- an insertion, for a borrow through a `&`
    /// reference, and a deletion, for an unused import -- two
    /// `MaybeIncorrect` ones, and a `HasPlaceholders` one for an
    /// unimplemented trait method.
    const SUGGESTED: &str = include_str!("../test_data/cargo-check-suggestions.json");
    /// The crate the report above was made over, byte for byte: the offsets in
    /// it are only meaningful against this exact text.
    const SUGGESTED_SOURCE: &str = include_str!("../test_data/cargo-check-suggestions.source");

    /// Real output over a crate whose error is on a line of Cyrillic and
    /// emoji, where a byte count, a character count and a UTF-16 count of the
    /// same place are three different numbers.
    const MULTIBYTE: &str = include_str!("../test_data/cargo-check-multibyte.json");
    const MULTIBYTE_SOURCE: &str = include_str!("../test_data/cargo-check-multibyte.source");

    fn reported_over(output: &str, source: &'static str) -> Vec<Reported> {
        what_the_compiler_reported(output, Path::new("/project"), |path| {
            (path == Path::new("/project/src/lib.rs")).then(|| source.to_string())
        })
    }

    fn every_fix(reported: &[Reported]) -> Vec<&Fix> {
        reported.iter().flat_map(|one| &one.fixes).collect()
    }

    /// The byte a UTF-16 position falls at, read back out of the text.
    ///
    /// The reader converts bytes to UTF-16 units; this converts them back,
    /// through a separate walk of the text, so a fix placed at the wrong
    /// column comes out of [`applying`] as the wrong text rather than as a
    /// number that matches whatever the reader happened to write.
    fn byte_offset_of(text: &str, at: lsp::Position) -> usize {
        let mut offset = 0;
        for (index, line) in text.split_inclusive('\n').enumerate() {
            if index as u32 != at.line {
                offset += line.len();
                continue;
            }
            let mut units = 0u32;
            for (byte, character) in line.char_indices() {
                if units >= at.character {
                    return offset + byte;
                }
                units += character.len_utf16() as u32;
            }
            return offset + line.len();
        }
        text.len()
    }

    fn applying(fix: &Fix, source: &str) -> String {
        let mut replacements: Vec<(usize, usize, &str)> = fix
            .replacements
            .iter()
            .map(|replacement| {
                (
                    byte_offset_of(source, replacement.range.start),
                    byte_offset_of(source, replacement.range.end),
                    replacement.new_text.as_str(),
                )
            })
            .collect();
        replacements.sort_by_key(|(start, _, _)| *start);
        let mut applied = String::new();
        let mut at = 0;
        for (start, end, new_text) in replacements {
            applied.push_str(source.get(at..start).unwrap_or_default());
            applied.push_str(new_text);
            at = end;
        }
        applied.push_str(source.get(at..).unwrap_or_default());
        applied
    }

    /// The compiler computes the fix and hands it over with the error. It
    /// arrives in a child message, whose own text is the fix's title.
    #[test]
    fn a_machine_applicable_suggestion_becomes_a_fix_with_the_compilers_own_title() {
        let reported = reported_over(SUGGESTED, SUGGESTED_SOURCE);
        let borrow = reported
            .iter()
            .find(|one| one.diagnostic.message.starts_with("cannot borrow"))
            .expect("the borrow error");

        assert_eq!(
            borrow
                .fixes
                .iter()
                .map(|fix| fix.title.as_str())
                .collect::<Vec<_>>(),
            vec!["consider changing this to be a mutable reference"]
        );
        let fix = &borrow.fixes[0];
        assert_eq!(fix.replacements.len(), 1, "{:?}", fix.replacements);
        assert_eq!(fix.replacements[0].new_text, "mut ");
        assert_eq!(
            fix.replacements[0].path,
            Path::new("/project/src/lib.rs"),
            "a replacement names its own file: a fix may reach one the error is not in"
        );
    }

    fn the_fix_titled<'a>(reported: &'a [Reported], title: &str) -> &'a Fix {
        every_fix(reported)
            .into_iter()
            .find(|fix| fix.title == title)
            .unwrap_or_else(|| panic!("no fix titled {title:?}"))
    }

    /// What the fix produces is the text the compiler asked for, character
    /// for character.
    #[test]
    fn applying_a_fix_produces_the_text_the_compiler_asked_for() {
        let reported = reported_over(SUGGESTED, SUGGESTED_SOURCE);
        let fix = the_fix_titled(
            &reported,
            "consider changing this to be a mutable reference",
        );

        assert_eq!(
            applying(fix, SUGGESTED_SOURCE),
            SUGGESTED_SOURCE.replace("values: &Vec<i32>", "values: &mut Vec<i32>"),
            "the fix turns the `&` reference into a `&mut` one and changes nothing else"
        );
    }

    /// A fix can be a deletion. The compiler asks for one by naming a range
    /// and no text to put there, which is a fix like any other and not the
    /// absence of one.
    #[test]
    fn a_fix_that_deletes_carries_the_text_it_expects_to_delete() {
        let reported = reported_over(SUGGESTED, SUGGESTED_SOURCE);
        let fix = the_fix_titled(&reported, "remove the whole `use` item");

        assert_eq!(fix.replacements[0].new_text, "");
        assert_eq!(fix.replacements[0].replaced, "use std::fmt::Debug;\n");
        assert_eq!(
            applying(fix, SUGGESTED_SOURCE),
            SUGGESTED_SOURCE.replace("use std::fmt::Debug;\n", "")
        );
    }

    /// The compiler grades its own suggestions, and only the top grade is a
    /// fix rather than a guess. A `HasPlaceholders` replacement would write
    /// `todo!()` into the file; a `MaybeIncorrect` one would write the
    /// compiler's guess at what was meant.
    #[test]
    fn a_suggestion_the_compiler_is_unsure_of_is_not_offered() {
        assert!(
            SUGGESTED.contains("MaybeIncorrect") && SUGGESTED.contains("HasPlaceholders"),
            "the fixture is only meaningful while the compiler still grades these lower"
        );

        let reported = reported_over(SUGGESTED, SUGGESTED_SOURCE);
        let mut titles: Vec<&str> = every_fix(&reported)
            .iter()
            .map(|fix| fix.title.as_str())
            .collect();
        titles.sort_unstable();
        assert_eq!(
            titles,
            vec![
                "consider changing this to be a mutable reference",
                "remove the whole `use` item",
            ],
            "only the machine-applicable suggestions are fixes"
        );

        // The errors themselves are all still reported: refusing to offer a
        // guess as a fix is not refusing to show the error it came with.
        let mut said: Vec<&str> = reported
            .iter()
            .filter_map(|one| match &one.diagnostic.code {
                Some(lsp::NumberOrString::String(code)) => Some(code.as_str()),
                _ => None,
            })
            .collect();
        said.sort_unstable();
        assert_eq!(
            said,
            vec!["E0046", "E0308", "E0596", "E0599", "unused_imports"]
        );
        for one in &reported {
            assert!(
                one.diagnostic.message.contains("help:"),
                "the advice is still in what the error says: {}",
                one.diagnostic.message
            );
        }
    }

    /// The other place a reassignment error is about -- the first write --
    /// keeps its own position rather than being glued into the message. The
    /// line it is on is Cyrillic, so a byte count and a UTF-16 count of that
    /// place are different numbers, and only one of them is the protocol's.
    #[test]
    fn a_non_primary_span_becomes_a_related_place_at_its_own_position() {
        let line = MULTIBYTE_SOURCE
            .lines()
            .nth(1)
            .expect("the line the binding is on");
        let inside = line.find("счётчик").expect("the binding");

        // Three counts of the same place, all different.
        assert_eq!(line[..inside].len(), 42, "bytes");
        assert_eq!(line[..inside].chars().count(), 29, "characters");
        assert_eq!(line[..inside].encode_utf16().count(), 31, "UTF-16 units");

        let reported = reported_over(MULTIBYTE, MULTIBYTE_SOURCE);
        let one = reported.first().expect("the error");

        // The place the error is filed under is the second write, unmoved.
        assert_eq!(
            one.diagnostic.range,
            lsp::Range {
                start: lsp::Position::new(2, 4),
                end: lsp::Position::new(2, 16),
            }
        );
        assert_eq!(
            one.diagnostic.related_information,
            Some(vec![lsp::DiagnosticRelatedInformation {
                location: lsp::Location {
                    uri: lsp::Uri::from_file_path("/project/src/lib.rs")
                        .expect("an absolute path is a file URI"),
                    range: lsp::Range {
                        start: lsp::Position::new(1, 31),
                        end: lsp::Position::new(1, 38),
                    },
                },
                message: "first assignment to `счётчик`".to_string(),
            }]),
            "the protocol's own unit -- not the 42 bytes the compiler counted from"
        );
    }

    /// A type error names the return type it had to match, and that place is
    /// on another line entirely. It arrives as a place and not as text.
    #[test]
    fn the_return_type_a_type_error_had_to_match_arrives_as_a_place() {
        let reported =
            what_the_compiler_reported(REAL_OUTPUT, Path::new("/project"), |_| Some(library()));
        let mismatched = reported.first().expect("the type error");
        let related = mismatched
            .diagnostic
            .related_information
            .as_ref()
            .expect("the return type is a place of its own");
        assert_eq!(related.len(), 1);
        assert_eq!(related[0].message, "expected `u32` because of return type");
        assert_eq!(
            related[0].location.range,
            lsp::Range {
                start: byte_position_in(&library(), 18),
                end: byte_position_in(&library(), 21),
            }
        );
        assert_ne!(
            related[0].location.range.start.line, mismatched.diagnostic.range.start.line,
            "the two places are on different lines, which is the whole point of keeping both"
        );
    }

    /// A warning about one place carries no related place at all, rather
    /// than one pointing back at itself.
    #[test]
    fn a_message_about_one_place_carries_no_related_place() {
        let reported =
            what_the_compiler_reported(REAL_OUTPUT, Path::new("/project"), |_| Some(library()));
        let unused = &reported[1];
        assert!(
            unused.diagnostic.message.starts_with("unused variable"),
            "{}",
            unused.diagnostic.message
        );
        assert_eq!(unused.diagnostic.related_information, None);
    }

    /// Which diagnostics are reported and where each one points. Keeping the
    /// other places must not add, drop or move one of them.
    #[test]
    fn which_diagnostics_are_reported_and_where_they_point_is_unchanged() {
        let reported =
            what_the_compiler_reported(REAL_OUTPUT, Path::new("/project"), |_| Some(library()));
        let placed: Vec<(Option<&str>, lsp::Range)> = reported
            .iter()
            .map(|one| {
                let code = match &one.diagnostic.code {
                    Some(lsp::NumberOrString::String(code)) => Some(code.as_str()),
                    _ => None,
                };
                (code, one.diagnostic.range)
            })
            .collect();
        assert_eq!(
            placed,
            vec![
                (
                    Some("E0308"),
                    lsp::Range {
                        start: byte_position_in(&library(), 28),
                        end: byte_position_in(&library(), 38),
                    }
                ),
                (
                    Some("unused_variables"),
                    lsp::Range {
                        start: byte_position_in(&library(), 68),
                        end: byte_position_in(&library(), 78),
                    }
                ),
            ]
        );
    }

    /// Where a byte offset falls, walked separately from the reader's own
    /// conversion so that a test pinning a place cannot be satisfied by
    /// whatever the reader happened to compute.
    fn byte_position_in(text: &str, byte: usize) -> lsp::Position {
        let before = &text[..byte];
        let line_starts_at = before.rfind('\n').map_or(0, |newline| newline + 1);
        lsp::Position {
            line: before.matches('\n').count() as u32,
            character: before[line_starts_at..].encode_utf16().count() as u32,
        }
    }

    /// A fix on a line of Cyrillic and emoji has to land on the character the
    /// compiler pointed at. The compiler counts characters, the file is
    /// stored in bytes, and the protocol asks for UTF-16 code units: on this
    /// line those are 29, 42 and 31.
    #[test]
    fn a_fix_on_a_line_with_an_emoji_lands_on_the_right_characters() {
        let line = MULTIBYTE_SOURCE
            .lines()
            .nth(1)
            .expect("the line the binding is on");
        let inside = line.find("счётчик").expect("the binding");
        assert_eq!(line[..inside].len(), 42, "bytes");
        assert_eq!(line[..inside].chars().count(), 29, "characters");
        assert_eq!(line[..inside].encode_utf16().count(), 31, "UTF-16 units");

        let reported = reported_over(MULTIBYTE, MULTIBYTE_SOURCE);
        let fix = the_fix_titled(&reported, "consider making this binding mutable");
        assert_eq!(fix.replacements[0].range.start.line, 1);
        assert_eq!(
            fix.replacements[0].range.start.character, 31,
            "the protocol's own unit -- not 29, which is what the compiler reports"
        );
        assert_eq!(
            applying(fix, MULTIBYTE_SOURCE),
            MULTIBYTE_SOURCE.replace("let счётчик", "let mut счётчик")
        );
    }

    /// The error a fix belongs to is not always where the fix lands: here the
    /// error is on the assignment and the replacement is on the line above,
    /// where the binding is. What the reader's cursor has to be near is the
    /// error.
    #[test]
    fn a_fix_can_land_on_a_different_line_than_the_error_it_belongs_to() {
        let reported = reported_over(MULTIBYTE, MULTIBYTE_SOURCE);
        let one = reported.first().expect("the error");
        assert_eq!(one.diagnostic.range.start.line, 2);
        assert_eq!(one.fixes[0].replacements[0].range.start.line, 1);
    }

    /// The report the diagnostics tests above are built on carries a fix too,
    /// for the unused variable in it. Nothing about reading it changed: the
    /// same two diagnostics come out, and one of them now also carries the
    /// rename the compiler asked for.
    #[test]
    fn the_earlier_report_keeps_its_diagnostics_and_gains_the_fix_it_carried() {
        let reported =
            what_the_compiler_reported(REAL_OUTPUT, Path::new("/project"), |_| Some(library()));
        assert_eq!(reported.len(), 2);
        assert!(reported[0].fixes.is_empty(), "the type error suggests none");

        let fix = the_fix_titled(
            &reported,
            "if this is intentional, prefix it with an underscore",
        );
        assert_eq!(fix.replacements[0].new_text, "_never_read");
        assert_eq!(fix.replacements[0].replaced, "never_read");
        assert_eq!(
            applying(fix, &library()),
            library().replace("never_read", "_never_read")
        );
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
