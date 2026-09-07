use std::ops::Range;

use harper_core::linting::{LintGroup, LintKind, Linter};
use harper_core::spell::{Dictionary, FstDictionary};
use harper_core::{Dialect, Document, Span};

pub mod prose_diagnostics_settings;
mod watching;

pub use prose_diagnostics_settings::ProseDiagnosticsSettings;
pub use watching::{Check, init};

/// The language whose files are prose from the first byte to the last.
pub const A_LANGUAGE_THAT_IS_ALL_PROSE: &str = "Markdown";

/// The dialect the advice is given in. American English is what harper's own
/// curated rule set is written against, and the choice only decides between
/// spellings both of which are correct, so it is not worth a setting until
/// somebody asks for one.
const DIALECT: Dialect = Dialect::American;

/// Where in a file to look for prose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prose {
    /// Every byte of it, read as Markdown so that code fences, links and
    /// headings are not read as sentences.
    TheWholeFileAsMarkdown,
    /// Only the lines that are entirely a comment, marked by one of these
    /// prefixes.
    OnlyTheCommentsMarkedBy(Vec<String>),
}

/// What of a buffer is worth reading as prose, given the language it is in and
/// what the reader has asked for. Nothing at all where there is no prose to
/// read, or where the reader has turned this off.
pub fn prose_of(
    language: Option<&str>,
    line_comments: &[String],
    settings: &ProseDiagnosticsSettings,
) -> Option<Prose> {
    if !settings.enabled {
        return None;
    }
    if language == Some(A_LANGUAGE_THAT_IS_ALL_PROSE) {
        return Some(Prose::TheWholeFileAsMarkdown);
    }
    if !settings.check_comments || line_comments.is_empty() {
        return None;
    }
    Some(Prose::OnlyTheCommentsMarkedBy(line_comments.to_vec()))
}

/// Everything harper has to say about the prose in this text, as diagnostics
/// the editor can show.
///
/// Advice throughout, never a fault: prose in a comment is a matter of taste in
/// a way that a type error is not, so nothing here is allowed to look like
/// something that stops work.
pub fn what_harper_said(text: &str, prose: &Prose) -> Vec<lsp::Diagnostic> {
    let dictionary = FstDictionary::curated();
    let mut rules = LintGroup::new_curated(dictionary.clone(), DIALECT);
    let mut said = Vec::new();
    for lifted in lift(text, prose) {
        if !reads_as_english(&lifted.prose, dictionary.as_ref()) {
            continue;
        }
        let document = if lifted.read_as_markdown {
            Document::new_markdown_default_curated(&lifted.prose)
        } else {
            Document::new_plain_english_curated(&lifted.prose)
        };
        for lint in rules.lint(&document) {
            if !lifted.read_as_markdown && lint.lint_kind == LintKind::Spelling {
                continue;
            }
            let Some(bytes) = lifted.back_in_the_file(&lint.span) else {
                continue;
            };
            said.push(lsp::Diagnostic {
                range: lsp::Range {
                    start: utf16_position_at(text, bytes.start),
                    end: utf16_position_at(text, bytes.end),
                },
                severity: Some(lsp::DiagnosticSeverity::HINT),
                code: Some(lsp::NumberOrString::String(lint.lint_kind.to_string())),
                source: Some("harper".to_string()),
                message: lint.message,
                ..Default::default()
            });
        }
    }
    said
}

/// A stretch of prose taken out of a file, with the file it came out of still
/// reachable from every byte of it.
struct Lifted {
    /// The prose as harper reads it, with nothing of the file's own syntax in
    /// it.
    prose: String,
    /// The runs of `prose` that came from the file, in order. Markdown is one
    /// run covering everything; a block of comments is one run per line, since
    /// the marker and the indent in between belong to neither.
    runs: Vec<Run>,
    read_as_markdown: bool,
}

/// One run of bytes that reads the same in the lifted prose and in the file.
struct Run {
    at_in_prose: usize,
    at_in_file: usize,
    length: usize,
}

impl Lifted {
    /// The bytes of the file a span of the lifted prose came from, or nothing
    /// for a span that falls outside it.
    ///
    /// Two conversions, and both are easy to get wrong. harper counts
    /// characters, so the span has to be resolved against the prose's own
    /// characters before its bytes mean anything; and the prose is not
    /// contiguous in the file, so a byte of it has to be traced back to the run
    /// it came from.
    fn back_in_the_file(&self, span: &Span<char>) -> Option<Range<usize>> {
        let start = self.in_the_file(byte_at_character(&self.prose, span.start)?)?;
        let end = self.in_the_file(byte_at_character(&self.prose, span.end)?)?;
        (start <= end).then_some(start..end)
    }

    fn in_the_file(&self, at_in_prose: usize) -> Option<usize> {
        self.runs.iter().find_map(|run| {
            let past_the_end = run.at_in_prose + run.length;
            (run.at_in_prose <= at_in_prose && at_in_prose <= past_the_end)
                .then(|| run.at_in_file + (at_in_prose - run.at_in_prose))
        })
    }
}

/// The byte a character index falls at, or nothing for an index past the end.
/// The index just past the last character is the length, which is what an
/// end-exclusive span's end is.
fn byte_at_character(text: &str, characters: usize) -> Option<usize> {
    if characters == 0 {
        return Some(0);
    }
    text.char_indices()
        .nth(characters)
        .map(|(at, _)| at)
        .or_else(|| (text.chars().count() == characters).then_some(text.len()))
}

fn lift(text: &str, prose: &Prose) -> Vec<Lifted> {
    match prose {
        Prose::TheWholeFileAsMarkdown => vec![Lifted {
            prose: text.to_string(),
            runs: vec![Run {
                at_in_prose: 0,
                at_in_file: 0,
                length: text.len(),
            }],
            read_as_markdown: true,
        }],
        Prose::OnlyTheCommentsMarkedBy(markers) => comment_blocks(text, markers),
    }
}

/// The runs of comment lines in a file, each block of neighbouring lines lifted
/// as one piece of prose.
///
/// A block rather than a line at a time because a sentence in a comment is
/// wrapped over as many lines as it needs, and a checker shown one line of it
/// has been shown a fragment: it would report the fragment's missing verb and
/// miss the real mistake two lines down.
///
/// Only a line that is a comment from its first non-blank character counts. A
/// comment written after code on the same line is left alone -- the marker
/// would also be found inside a string or a URL, and reading source as prose
/// would put a complaint under every identifier in the file.
fn comment_blocks(text: &str, markers: &[String]) -> Vec<Lifted> {
    let mut blocks = Vec::new();
    let mut building: Option<Lifted> = None;
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        match comment_body(line, markers) {
            Some((at_in_line, body)) => {
                let block = building.get_or_insert_with(|| Lifted {
                    prose: String::new(),
                    runs: Vec::new(),
                    read_as_markdown: false,
                });
                let at_in_prose = block.prose.len();
                block.runs.push(Run {
                    at_in_prose,
                    at_in_file: at + at_in_line,
                    length: body.len(),
                });
                block.prose.push_str(body);
                block.prose.push('\n');
            }
            None => {
                if let Some(block) = building.take() {
                    blocks.push(block);
                }
            }
        }
        at += line.len();
    }
    blocks.extend(building);
    blocks.retain(|block| !block.prose.trim().is_empty());
    blocks
}

/// Where a line's comment text starts and what it says, or nothing for a line
/// that is not wholly a comment.
fn comment_body<'line>(line: &'line str, markers: &[String]) -> Option<(usize, &'line str)> {
    let line = line.trim_end_matches(['\n', '\r']);
    let text = line.trim_start();
    // The longest marker that fits, not the first: Rust's markers are `// `,
    // `/// ` and `//! ` in that order, and stripping `/// ` with `// ` leaves a
    // stray slash at the front of every doc comment.
    let marker = markers
        .iter()
        .map(|marker| marker.trim_end())
        .filter(|marker| !marker.is_empty() && text.starts_with(marker))
        .max_by_key(|marker| marker.len())?;
    let starts_at = line.len() - text.len() + marker.len();
    line.get(starts_at..).map(|body| (starts_at, body))
}

/// Fewer words than this and there is nothing to judge either way: a two-word
/// comment is as likely to be a name as a sentence.
const FEWER_WORDS_THAN_THIS_SAY_NOTHING: usize = 3;

/// How many words in five have to be English words for the text to be read as
/// English at all.
const IN_FIVE_WORDS_THIS_MANY_ARE_ENGLISH: usize = 3;

/// Whether this text is English, which is the only language harper checks.
///
/// harper's own description of itself is an English grammar checker, and its
/// rules assume it: shown a Russian or a German comment it reports every word
/// of it. So a text is only checked where most of its words are words --
/// measured against the same curated dictionary the checking itself uses, which
/// is the one thing here that already knows what an English word is.
///
/// It is deliberately a conservative test. A comment thick with identifiers
/// reads as not-English and goes unchecked, which loses some real advice; the
/// other way round loses the reader's trust in all of it.
pub fn reads_as_english(text: &str, dictionary: &impl Dictionary) -> bool {
    let words: Vec<&str> = text
        .split(|character: char| !character.is_alphabetic() && character != '\'')
        .filter(|word| !word.is_empty())
        .collect();
    if words.len() < FEWER_WORDS_THAN_THIS_SAY_NOTHING {
        return false;
    }
    let known = words
        .iter()
        .filter(|word| dictionary.contains_word_str(word))
        .count();
    known * 5 >= words.len() * IN_FIVE_WORDS_THIS_MANY_ARE_ENGLISH
}

/// The position a byte offset falls at, counted the way the protocol counts:
/// lines from zero, and characters as UTF-16 code units.
///
/// Three units are in play and they disagree. harper counts characters, the
/// protocol asks for UTF-16 code units, and a byte offset is what locates
/// anything in a Rust string -- so the count has to be converted twice on the
/// way out, and reading one of the three as another puts every diagnostic on a
/// line with an emoji in it in the wrong column.
///
/// An offset past the end, or inside a character, lands on the nearest boundary
/// at or before it, rather than panicking on a buffer that has since changed.
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

/// How many words the curated dictionary holds, which is the bulk of what this
/// feature costs in memory once it is loaded.
pub fn dictionary_word_count() -> usize {
    FstDictionary::curated().word_count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn rust_markers() -> Vec<String> {
        ["// ", "/// ", "//! "]
            .into_iter()
            .map(str::to_string)
            .collect()
    }

    fn in_rust_comments(text: &str) -> Vec<lsp::Diagnostic> {
        what_harper_said(text, &Prose::OnlyTheCommentsMarkedBy(rust_markers()))
    }

    fn as_markdown(text: &str) -> Vec<lsp::Diagnostic> {
        what_harper_said(text, &Prose::TheWholeFileAsMarkdown)
    }

    /// What a diagnostic actually underlines, read back out of the file it is
    /// about. Every test here asserts this rather than a count: advice under
    /// the wrong words is worse than no advice.
    fn marked<'text>(text: &'text str, said: &lsp::Diagnostic) -> &'text str {
        let from = byte_of(text, said.range.start);
        let to = byte_of(text, said.range.end);
        text.get(from..to).unwrap_or_default()
    }

    fn marks(text: &str, said: &[lsp::Diagnostic]) -> Vec<String> {
        said.iter()
            .map(|one| marked(text, one).to_string())
            .collect()
    }

    /// The whole promise, over real mistakes: a comment that says a word twice
    /// and puts `a` in front of a vowel is advised on, on the comment's own
    /// line and never running back over the marker that is not prose.
    #[test]
    fn a_comment_with_a_real_mistake_is_advice_on_the_words_it_is_about() {
        let text =
            "// The results are cached for the the next caller, and a index is kept.\nfn s() {}\n";
        let said = in_rust_comments(text);
        assert!(!said.is_empty(), "harper found nothing to say");
        for one in &said {
            assert_eq!(
                one.severity,
                Some(lsp::DiagnosticSeverity::HINT),
                "advice, not a fault: nothing here stops work"
            );
            assert_eq!(one.source.as_deref(), Some("harper"));
            assert_eq!(one.range.start.line, 0, "on the comment's own line");
            let mark = marked(text, one);
            assert!(!mark.is_empty(), "a diagnostic marking nothing");
            assert!(!mark.contains('/'), "{mark:?} runs back over the marker");
        }
    }

    /// A comment with nothing wrong with it says nothing. A checker that
    /// complained about correct prose would be turned off within a day.
    #[test]
    fn a_comment_with_nothing_wrong_with_it_says_nothing() {
        let text = "// The results of the search are cached for the next caller.\nfn search() {}\n";
        let said = in_rust_comments(text);
        assert!(said.is_empty(), "{:?}", marks(text, &said));
    }

    /// Markdown is prose from the first byte, and a mistake in a paragraph of
    /// it is found with no comment marker anywhere in the file.
    #[test]
    fn a_markdown_paragraph_is_read_as_prose_from_the_first_byte() {
        let text = "# Notes\n\nThe list of the open files is kept between the the runs.\n";
        let said = as_markdown(text);
        assert!(!said.is_empty(), "harper found nothing to say");
        assert!(
            said.iter().all(|one| one.range.start.line == 2),
            "{:?} is not the paragraph",
            said.iter()
                .map(|one| one.range.start.line)
                .collect::<Vec<_>>()
        );
    }

    /// harper is an English checker and says so itself. A comment in Russian
    /// must go unchecked rather than be reported word by word.
    #[test]
    fn a_comment_in_russian_is_left_alone_rather_than_reported_word_by_word() {
        let text = "// Эта функция возвращает значение по умолчанию для пустого списка.\nfn default() {}\n";
        let said = in_rust_comments(text);
        assert!(said.is_empty(), "{:?}", marks(text, &said));
    }

    /// The same test one language over, because a Latin alphabet is no test of
    /// anything: German is written in the letters English is and is still not
    /// English.
    #[test]
    fn a_comment_in_german_is_left_alone_too() {
        let text = "// Diese Funktion gibt den vorgegebenen Wert für eine leere Liste zurück.\nfn d() {}\n";
        let said = in_rust_comments(text);
        assert!(said.is_empty(), "{:?}", marks(text, &said));
    }

    /// Source code is not prose. Every identifier in a file would be a
    /// complaint, so nothing outside a whole-line comment is read at all --
    /// including a line carrying a marker inside a string, and a comment
    /// written after code on the same line, where a marker also turns up in
    /// every URL.
    #[test]
    fn source_code_outside_a_comment_says_nothing() {
        for code in [
            "fn main() { let is_a_the_of = 1; }\n",
            "let note = \"// the results is cached\";\n",
            "let url = \"https://example.com/a/b\";\n",
            "let cached = search(); // the results of the search is cached\n",
            "struct Thing { the: usize, of: usize, is: usize }\n",
        ] {
            let said = in_rust_comments(code);
            assert!(
                said.is_empty(),
                "{code:?} produced {:?}",
                marks(code, &said)
            );
        }
    }

    /// Three counts of the same place, all different, and the editor wants the
    /// third. harper counts characters, a byte offset is what indexes a Rust
    /// string, and the protocol reads UTF-16 code units -- so a finding on a
    /// line with an accent and two emoji in it lands columns out if any one of
    /// the three is read as another.
    ///
    /// What harper counts is not assumed here. It is read straight out of
    /// harper, twice: the span it reports is resolved against the document's
    /// own characters and shown to name the misspelt word, and the same number
    /// read as a byte offset is shown to name something else entirely.
    #[test]
    fn a_character_column_becomes_a_utf16_one_and_not_a_byte_or_character_one() {
        // The one deliberate misspelling in this file, kept in one place so the
        // spell checker that runs over this repository has one line to be told
        // about rather than four.
        const MISSPELT: &str = "serach"; // spellchecker:disable-line
        let text = format!("The café 🦀🔥 results of the {MISSPELT} are cached.\n");
        let text = text.as_str();
        let at = text.find(MISSPELT).expect("the misspelling");

        // Three counts of the same place, all different.
        assert_eq!(at, 34, "bytes");
        assert_eq!(text[..at].chars().count(), 27, "characters");
        assert_eq!(text[..at].encode_utf16().count(), 29, "UTF-16 code units");

        let document = Document::new_markdown_default_curated(text);
        let mut rules = LintGroup::new_curated(FstDictionary::curated(), DIALECT);
        let lints = rules.lint(&document);
        let found = lints
            .iter()
            .find(|lint| document.get_span_content_str(&lint.span) == MISSPELT)
            .expect("harper reports the misspelling");
        assert_eq!(
            found.span.start, 27,
            "a character index -- not 34 bytes and not 29 UTF-16 units"
        );
        assert_ne!(
            text.as_bytes().get(found.span.start),
            Some(&b's'),
            "read as a byte offset the same number names something else"
        );

        let said = as_markdown(text);
        let on_the_word: Vec<&lsp::Diagnostic> = said
            .iter()
            .filter(|one| marked(text, one) == MISSPELT)
            .collect();
        assert_eq!(on_the_word.len(), 1, "{:?}", marks(text, &said));
        assert_eq!(
            on_the_word[0].range.start,
            lsp::Position::new(0, 29),
            "the protocol's own unit, counted from the start of the line"
        );
    }

    /// A sentence wrapped over several comment lines is one piece of prose, and
    /// a word on its third line is still traced back to the third line of the
    /// file. Read a line at a time it would be three fragments, and a checker
    /// shown a fragment reports the fragment.
    #[test]
    fn a_sentence_wrapped_over_several_comment_lines_is_read_as_one() {
        let text = "\
// The results of the
// search that the caller
// asked for are cached.
fn search() {}
";
        let lifted = comment_blocks(text, &rust_markers());
        assert_eq!(lifted.len(), 1, "three neighbouring lines are one piece");
        assert_eq!(
            lifted[0].prose,
            " The results of the\n search that the caller\n asked for are cached.\n"
        );

        let prose = lifted[0].prose.clone();
        let byte = prose.find("caller").expect("the word is in the prose");
        let from = prose[..byte].chars().count();
        let back = lifted[0]
            .back_in_the_file(&Span::new(from, from + "caller".chars().count()))
            .expect("a word of the prose is somewhere in the file");
        assert_eq!(
            text.get(back.clone()),
            Some("caller"),
            "the second line's own bytes, not the first line's"
        );
        assert_eq!(utf16_position_at(text, back.start).line, 1);
    }

    /// Two blocks of comments with code between them are two pieces of prose,
    /// not one: joining them would make a sentence out of two unrelated halves.
    #[test]
    fn comments_either_side_of_code_are_two_pieces_of_prose() {
        let text = "\
// The results are cached for the the caller.
fn search() {}
// The names are counted for the the log.
fn callers() {}
";
        assert_eq!(comment_blocks(text, &rust_markers()).len(), 2);
        let said = in_rust_comments(text);
        let lines: Vec<u32> = said.iter().map(|one| one.range.start.line).collect();
        assert!(lines.contains(&0), "{lines:?}");
        assert!(lines.contains(&2), "{lines:?}");
        assert!(
            lines.iter().all(|line| *line == 0 || *line == 2),
            "{lines:?} includes a line that is not a comment"
        );
    }

    /// A doc comment's marker is the longer one. Stripping `///` with `//`
    /// leaves a stray slash at the front of the prose, which shifts every
    /// column after it and reads as a word of its own.
    #[test]
    fn the_longest_marker_that_fits_is_the_one_taken_off() {
        let (at, body) = comment_body("    /// The results is cached.", &rust_markers())
            .expect("a doc comment is a comment");
        assert_eq!(body, " The results is cached.", "no stray slash");
        assert_eq!(at, 7, "past the indent and the whole marker");
        assert_eq!(comment_body("    let x = 1;", &rust_markers()), None);
    }

    /// Turned off, nothing is read at all -- not the comments, and not a
    /// Markdown file either. Prose advice in a comment is a matter of taste in
    /// a way that a type error is not, so this has to be a switch that really
    /// switches.
    #[test]
    fn the_setting_turns_the_whole_thing_off() {
        let off = ProseDiagnosticsSettings {
            enabled: false,
            check_comments: true,
        };
        assert_eq!(prose_of(Some("Markdown"), &[], &off), None);
        assert_eq!(prose_of(Some("Rust"), &rust_markers(), &off), None);

        let no_comments = ProseDiagnosticsSettings {
            enabled: true,
            check_comments: false,
        };
        assert_eq!(
            prose_of(Some("Rust"), &rust_markers(), &no_comments),
            None,
            "the comments are off"
        );
        assert_eq!(
            prose_of(Some("Markdown"), &[], &no_comments),
            Some(Prose::TheWholeFileAsMarkdown),
            "and a Markdown file is still all prose"
        );

        let on = ProseDiagnosticsSettings {
            enabled: true,
            check_comments: true,
        };
        assert_eq!(
            prose_of(Some("Rust"), &rust_markers(), &on),
            Some(Prose::OnlyTheCommentsMarkedBy(rust_markers()))
        );
        assert_eq!(
            prose_of(Some("Plain Text"), &[], &on),
            None,
            "a language with no comment marker has nowhere to look"
        );
        assert_eq!(prose_of(None, &[], &on), None);
    }

    /// The test the English rule is worth nothing without: real English prose
    /// has to pass it, or the feature is off everywhere and every other test
    /// here passes by saying nothing.
    #[test]
    fn english_reads_as_english_and_other_languages_do_not() {
        let dictionary = FstDictionary::curated();
        for english in [
            "The results of the search are cached for the next caller.",
            "Advice, not a fault: nothing here stops work.",
        ] {
            assert!(
                reads_as_english(english, dictionary.as_ref()),
                "{english:?}"
            );
        }
        for other in [
            "Эта функция возвращает значение по умолчанию.",
            "Diese Funktion gibt den vorgegebenen Wert zurück.",
            "この関数は既定の値を返します。",
            "TODO",
        ] {
            assert!(!reads_as_english(other, dictionary.as_ref()), "{other:?}");
        }
    }

    /// The dictionary is the weight this feature carries, and the number is
    /// worth knowing rather than guessing at.
    #[test]
    fn the_curated_dictionary_is_loaded_and_holds_a_dictionarys_worth_of_words() {
        let words = dictionary_word_count();
        assert!(words > 10_000, "{words} words is not a dictionary");
    }

    /// A place the prose does not have is skipped rather than clamped onto a
    /// line it was never measured against.
    #[test]
    fn a_place_past_the_end_of_the_prose_is_skipped() {
        let lifted = Lifted {
            prose: "a b\n".to_string(),
            runs: vec![Run {
                at_in_prose: 0,
                at_in_file: 3,
                length: 3,
            }],
            read_as_markdown: false,
        };
        assert_eq!(lifted.back_in_the_file(&Span::new(0, 3)), Some(3..6));
        assert_eq!(lifted.back_in_the_file(&Span::new(0, 9_000)), None);
    }

    /// The same three units, checked on the conversion itself, including an
    /// offset that lands inside a character.
    #[test]
    fn a_place_inside_a_character_lands_on_the_boundary_before_it() {
        assert_eq!(utf16_position_at("x\n", 9_000), lsp::Position::new(1, 0));
        assert_eq!(utf16_position_at("", 7), lsp::Position::new(0, 0));
        // Inside the four bytes of the crab, which begins at byte 2.
        assert_eq!(utf16_position_at("a 🦀 b", 4), lsp::Position::new(0, 2));
        // And just past it, where its two UTF-16 units have been counted.
        assert_eq!(utf16_position_at("a 🦀 b", 6), lsp::Position::new(0, 4));
    }

    /// The byte a position names, so a test can say which words were marked.
    fn byte_of(text: &str, position: lsp::Position) -> usize {
        let line_starts_at = text
            .split_inclusive('\n')
            .take(position.line as usize)
            .map(str::len)
            .sum::<usize>();
        let line = text.get(line_starts_at..).unwrap_or_default();
        let mut counted = 0;
        for (at, character) in line.char_indices() {
            if counted >= position.character as usize {
                return line_starts_at + at;
            }
            counted += character.len_utf16();
        }
        line_starts_at + line.len()
    }
}
