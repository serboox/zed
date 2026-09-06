use std::borrow::Cow;

use typos::Status;
use typos::tokens::{Case, Identifier, Tokenizer, Word};
use unicase::UniCase;

mod accepting;
mod typo_settings;
mod watching;

pub use accepting::{Accepted, CONFIGURATION_FILE_NAMES, accepted_near};
pub use typo_settings::TypoSettings;
pub use watching::{Check, init, what_to_report};

const SOURCE: &str = "typos";

/// The words a misspelling is judged against: the ones `typos` ships with,
/// and the ones the project has said something about in its own
/// configuration.
///
/// The project's word wins. A team that has written a word down has already
/// decided about it, and reporting it anyway argues with a decision that was
/// taken deliberately.
struct KnownWords<'a> {
    accepted: &'a Accepted,
}

/// Names `typos` itself approves of whole, which would otherwise be
/// reported once split: `WRONLY` reads as a misspelling of `wrongly`, `dBA`
/// as one of `dba`. The tool carries this list; without it the check
/// disagrees with the tool it is supposed to be.
const NAMES_TAKEN_AS_THEY_ARE: [&str; 3] = ["O_WRONLY", "dBA", "HashiCorp"];

impl typos::Dictionary for KnownWords<'_> {
    fn correct_ident<'s>(&'s self, identifier: Identifier<'_>) -> Option<Status<'s>> {
        if let Some(said) = self.accepted.said_about_identifier(identifier.token()) {
            return Some(said);
        }
        NAMES_TAKEN_AS_THEY_ARE
            .contains(&identifier.token())
            .then_some(Status::Valid)
    }

    fn correct_word<'s>(&'s self, word: Word<'_>) -> Option<Status<'s>> {
        // A run of digits, a hash, a base64 blob: nothing a dictionary can
        // say anything about.
        if word.case() == Case::None {
            return None;
        }
        if let Some(said) = self.accepted.said_about_word(word.token()) {
            return Some(written_like(said, word.case()));
        }
        let corrections = typos_dict::WORD
            .find(&UniCase::new(word.token()))
            .copied()?;
        if corrections.is_empty() {
            return Some(Status::Invalid);
        }
        Some(written_like(
            Status::Corrections(corrections.iter().copied().map(Cow::Borrowed).collect()),
            word.case(),
        ))
    }
}

/// Everything `typos` has to say about this text, as diagnostics the editor
/// can show, with no language server anywhere.
///
/// A name is looked at whole first and split into words only if nothing is
/// known about it whole, which is `typos`' own rule and the reason
/// `myFunctoin` and `my_functoin` are both caught: neither name is in any
/// dictionary, but the `Functoin` and `functoin` inside them are.
pub fn typos_in(text: &str, accepted: &Accepted) -> Vec<lsp::Diagnostic> {
    let tokenizer = Tokenizer::new();
    let known = KnownWords { accepted };
    typos::check_str(text, &tokenizer, &known)
        .map(|typo| lsp::Diagnostic {
            range: lsp::Range {
                start: utf16_position_at(text, typo.byte_offset),
                end: utf16_position_at(text, typo.byte_offset.saturating_add(typo.typo.len())),
            },
            // Advice, not a fault. A misspelled name compiles and runs, and
            // the reader is the one who decides whether it was meant.
            severity: Some(lsp::DiagnosticSeverity::HINT),
            source: Some(SOURCE.to_string()),
            message: what_it_said(&typo),
            ..Default::default()
        })
        .collect()
}

/// The whole of what one finding says. The correction is the useful half: a
/// reader who is told only that a word is wrong still has to guess what was
/// meant, and `typos` already knows.
fn what_it_said(typo: &typos::Typo<'_>) -> String {
    match &typo.corrections {
        Status::Corrections(corrections) if !corrections.is_empty() => {
            format!("`{}` should be {}", typo.typo, one_of(corrections))
        }
        // `Valid` never reaches here -- a word the dictionary approves of is
        // not reported at all -- and `Invalid` is a word known to be wrong
        // with nothing known to put in its place.
        _ => format!("`{}` is misspelled", typo.typo),
    }
}

/// The corrections as a reader would say them: "`a`", "`a` or `b`", "`a`,
/// `b` or `c`".
fn one_of(corrections: &[Cow<'_, str>]) -> String {
    let quoted: Vec<String> = corrections
        .iter()
        .map(|correction| format!("`{correction}`"))
        .collect();
    match quoted.split_last() {
        Some((last, [])) => last.clone(),
        Some((last, earlier)) => format!("{} or {last}", earlier.join(", ")),
        None => String::new(),
    }
}

/// A correction written the way the word it replaces was written, so the
/// advice can be taken as it stands: `Functoin` is corrected to `Function`
/// and `TEH` to `THE`, not to `function` and `the`.
fn written_like(mut status: Status<'_>, case: Case) -> Status<'_> {
    for correction in status.corrections_mut() {
        match case {
            Case::Lower | Case::None => {}
            Case::Title => {
                let capitalised = with_a_capital(correction);
                *correction = Cow::Owned(capitalised);
            }
            Case::Upper => {
                let shouted = correction.to_uppercase();
                *correction = Cow::Owned(shouted);
            }
        }
    }
    status
}

fn with_a_capital(word: &str) -> String {
    let mut characters = word.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().chain(characters).collect(),
        None => String::new(),
    }
}

/// The position a byte offset falls at, counted the way the protocol counts:
/// lines from zero, and characters as UTF-16 code units.
///
/// Three units are in play and they disagree. `typos` reports a byte offset
/// from the start of the buffer -- `Typo::byte_offset`, measured as the
/// distance between the token's own pointer and the buffer's -- while the
/// protocol asks for UTF-16 code units within a line, and a reader looking at
/// the line counts characters. Only the text can convert between them, which
/// is why this takes the text.
///
/// An offset past the end, or inside a character, lands on the nearest
/// boundary at or before it, rather than panicking on text that has changed
/// since it was measured.
pub fn utf16_position_at(text: &str, offset: usize) -> lsp::Position {
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
    use std::path::Path;

    use super::*;
    use pretty_assertions::assert_eq;

    /// Real source, with one misspelling in a snake_case name, one in a
    /// camelCase name, and two in comments. Every one of them is a word only
    /// once the name around it is split, which is the whole reason a
    /// spell-checker over raw text finds none of them.
    const CHECKED: &str = "\
// The lenght is measured once.
fn my_functoin() {}

struct Reader {
    /// How many bytes the reader recieved.
    total: usize,
}

fn myOtherFunctoin(reader: &Reader) -> usize {
    reader.total
}
";

    /// The same source with every misspelling fixed, and nothing else
    /// changed.
    const CORRECTED: &str = "\
// The length is measured once.
fn my_function() {}

struct Reader {
    /// How many bytes the reader received.
    total: usize,
}

fn myOtherFunction(reader: &Reader) -> usize {
    reader.total
}
";

    fn over(text: &str) -> Vec<lsp::Diagnostic> {
        typos_in(text, &Accepted::default())
    }

    fn placed(diagnostic: &lsp::Diagnostic) -> (u32, u32, u32) {
        (
            diagnostic.range.start.line,
            diagnostic.range.start.character,
            diagnostic.range.end.character,
        )
    }

    /// A misspelling inside a snake_case name, reported on the word rather
    /// than on the whole name: `my_functoin` is in no dictionary, and only
    /// splitting it turns it into something that can be judged.
    #[test]
    fn a_misspelling_in_a_snake_case_name_is_reported_with_its_correction() {
        let reported = over(CHECKED);
        let found = &reported[1];
        assert_eq!(found.message, "`functoin` should be `function`");
        assert_eq!(
            placed(found),
            (1, 6, 14),
            "the `functoin` in `my_functoin`, not the whole name"
        );
        assert_eq!(found.source.as_deref(), Some("typos"));
    }

    /// The same misspelling inside a camelCase name, corrected the way it
    /// was written: `Functoin` is advised to be `Function`, so the advice can
    /// be taken as it stands.
    #[test]
    fn a_misspelling_in_a_camel_case_name_is_reported_with_its_correction() {
        let reported = over(CHECKED);
        let found = &reported[3];
        assert_eq!(found.message, "`Functoin` should be `Function`");
        assert_eq!(
            placed(found),
            (8, 10, 18),
            "the `Functoin` in `myOtherFunctoin`"
        );
    }

    /// A misspelling in a comment, which is where most of them are: the
    /// words there are prose, and nothing else in the editor reads them.
    #[test]
    fn a_misspelling_in_a_comment_is_reported_with_its_correction() {
        let reported = over(CHECKED);
        assert_eq!(reported.len(), 4, "{reported:?}");
        assert_eq!(reported[0].message, "`lenght` should be `length`");
        assert_eq!(placed(&reported[0]), (0, 7, 13));
        assert_eq!(reported[2].message, "`recieved` should be `received`");
        assert_eq!(placed(&reported[2]), (4, 34, 42));
    }

    /// The same file with the four misspellings fixed says nothing at all.
    /// Everything else in it -- `usize`, `fn`, `struct`, the names -- is
    /// unknown to the dictionary rather than wrong, and `typos` reports only
    /// what it knows to be a misspelling.
    #[test]
    fn a_file_with_nothing_misspelled_reports_nothing() {
        assert_eq!(over(CORRECTED), Vec::new());
    }

    /// A misspelled name compiles and runs. It is worth saying and it is not
    /// a fault, so it arrives at the severity that says "consider" rather
    /// than the one a compiler uses.
    #[test]
    fn a_misspelling_is_advice_rather_than_an_error() {
        for found in over(CHECKED) {
            assert_eq!(found.severity, Some(lsp::DiagnosticSeverity::HINT));
        }
    }

    /// A name the tool itself approves of whole is not reported. `WRONLY`
    /// is a misspelling of `wrongly` anywhere else, and a C project full of
    /// `O_WRONLY` would be underlined on every line of it.
    #[test]
    fn a_name_the_tool_takes_as_it_is_is_not_reported() {
        assert_eq!(over("let flags = O_WRONLY;\n"), Vec::new());
        assert_eq!(
            over("let flags = WRONLY;\n")
                .iter()
                .map(|found| found.message.clone())
                .collect::<Vec<_>>(),
            vec!["`WRONLY` should be `WRONGLY`".to_string()],
            "the same word on its own is still a misspelling"
        );
    }

    /// A word the project has written down in its own configuration is not
    /// reported, however sure the dictionary is about it. The team has
    /// already decided, and the editor arguing with that decision is worse
    /// than the misspelling.
    #[test]
    fn a_word_the_project_accepts_is_not_reported() {
        let accepted = accepted_near(Path::new("/project/src"), |path| {
            (path == Path::new("/project/_typos.toml"))
                .then(|| "[default.extend-words]\nfunctoin = \"functoin\"\n".to_string())
        });

        let reported = typos_in(CHECKED, &accepted);
        assert_eq!(
            reported
                .iter()
                .map(|found| found.message.clone())
                .collect::<Vec<_>>(),
            vec![
                "`lenght` should be `length`".to_string(),
                "`recieved` should be `received`".to_string(),
            ],
            "neither `my_functoin` nor `myOtherFunctoin` is mentioned again"
        );
    }

    /// `typos` reports a byte offset into the buffer, the protocol counts a
    /// column in UTF-16 code units, and a reader looking at the line counts
    /// characters. All three disagree on the same place, and reading one as
    /// another puts every diagnostic on a line with an accent or an emoji in
    /// the wrong column.
    #[test]
    fn a_byte_offset_becomes_a_utf16_column_and_not_a_byte_or_character_one() {
        const LINE: &str = "let café = \"🦀🔥\"; // teh end\n";
        let before = LINE.split("teh").next().expect("the text before `teh`");

        // Three counts of the same place, all different.
        assert_eq!(before.len(), 27, "bytes");
        assert_eq!(before.chars().count(), 20, "characters");
        assert_eq!(before.encode_utf16().count(), 22, "UTF-16 units");

        // What the library itself reports, unread by the conversion under
        // test: the byte offset, and nothing else.
        let tokenizer = Tokenizer::new();
        let accepted = Accepted::default();
        let raw: Vec<usize> = typos::check_str(
            LINE,
            &tokenizer,
            &KnownWords {
                accepted: &accepted,
            },
        )
        .map(|typo| typo.byte_offset)
        .collect();
        assert_eq!(
            raw,
            vec![27],
            "27 is the byte offset -- not 20 characters and not 22 UTF-16 units"
        );

        let reported = typos_in(LINE, &accepted);
        assert_eq!(reported.len(), 1, "{reported:?}");
        assert_eq!(
            reported[0].range,
            lsp::Range {
                start: lsp::Position::new(0, 22),
                end: lsp::Position::new(0, 25),
            },
            "the protocol's own unit -- not 27, which is what `typos` reports"
        );
    }

    /// An offset past the end of the text, or inside a character, lands on
    /// the nearest boundary at or before it rather than panicking. The text
    /// a finding was measured on can have changed since.
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
