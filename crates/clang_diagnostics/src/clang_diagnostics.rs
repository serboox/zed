mod compile_commands;
mod describing;
mod parsing;
mod relating;
mod watching;

use std::path::{Path, PathBuf};

use collections::HashMap;

pub use compile_commands::{Entry, arguments_for, entries_in, where_the_database_is};
pub use parsing::{
    Asked, Described, HELD_AT_MOST, NamedType, Parsed, Place, Related, Relatives, Request,
    ask_libclang,
};
pub use relating::{Target, ask_about_types};
pub use watching::Diagnose;

/// What this crate registers with the application: the compiler's own findings
/// on save, and the type under the cursor on hover. The type hierarchy it also
/// answers is asked for directly, by the panel that shows one.
pub fn init(cx: &mut gpui::App) {
    watching::init(cx);
    describing::init(cx);
}

/// How bad the compiler front end thought a finding was.
///
/// Its own scale has five steps; two of them never reach here. `Ignored` is a
/// diagnostic the flags in force have switched off, and a `Fatal` is an
/// `Error` that also stopped the parse -- a distinction about the compiler's
/// progress, not about the reader's code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Note,
    Warning,
    Error,
}

/// One thing the front end said, and where it said it.
///
/// Plain data, and deliberately so: it is read on the one thread that owns
/// `libclang` and answered on another, and nothing borrowed from a
/// translation unit may outlive the parse that produced it.
///
/// `start` and `end` are byte offsets into `file`, which is not always the file
/// that was parsed: a header the file includes is part of the same translation
/// unit, and an error in one belongs on the header's own line. The front end
/// counts columns in bytes and the protocol counts them in UTF-16 code units,
/// so the offsets are kept as they came and converted once, against the text of
/// the file they are in, in [`as_diagnostics_by_file`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub severity: Severity,
    pub message: String,
    /// The warning's own flag, as the compiler spells it on the command line --
    /// `-Wunused-variable`. Absent for a hard error, which has no flag to
    /// switch it off with and so has no name to look up.
    pub code: Option<String>,
    pub file: PathBuf,
    pub start: usize,
    pub end: usize,
    /// The notes the front end hung off it, each still at its own place.
    pub notes: Vec<Note>,
}

/// One note explaining a [`Finding`] -- which overload was tried, where the
/// conflicting declaration is -- at the place it is about, which is very often
/// not the place the finding is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub message: String,
    pub file: PathBuf,
    pub start: usize,
    pub end: usize,
}

/// The name the editor files these under, so a reader can see which of the
/// sources on a file said what.
pub const SOURCE: &str = "clang";

/// What the front end said, grouped by the file each finding is in and in the
/// shape the editor shows.
///
/// `text_of` must answer with the text each file was parsed as -- the buffer's
/// text for the file being edited, and what is on disk for a header. The
/// offsets in a [`Finding`] are only meaningful against that, and placing a
/// diagnostic on the wrong column is worse than not showing it. A file it
/// cannot answer for is left out rather than guessed at.
pub fn as_diagnostics_by_file(
    findings: &[Finding],
    text_of: impl Fn(&Path) -> Option<String>,
) -> Vec<(PathBuf, Vec<lsp::Diagnostic>)> {
    let mut texts: HashMap<PathBuf, Option<String>> = HashMap::default();
    let mut text_for = |file: &Path| -> Option<String> {
        if !texts.contains_key(file) {
            texts.insert(file.to_path_buf(), text_of(file));
        }
        texts.get(file).cloned().flatten()
    };

    let mut by_file: Vec<(PathBuf, Vec<lsp::Diagnostic>)> = Vec::new();
    for finding in findings {
        let Some(text) = text_for(&finding.file) else {
            continue;
        };
        let mut related = Vec::new();
        for note in &finding.notes {
            let Some(note_text) = text_for(&note.file) else {
                continue;
            };
            let Ok(uri) = lsp::Uri::from_file_path(&note.file) else {
                continue;
            };
            related.push(lsp::DiagnosticRelatedInformation {
                location: lsp::Location {
                    uri,
                    range: lsp::Range {
                        start: utf16_position_at(&note_text, note.start),
                        end: utf16_position_at(&note_text, note.end),
                    },
                },
                message: note.message.clone(),
            });
        }
        let diagnostic = lsp::Diagnostic {
            range: lsp::Range {
                start: utf16_position_at(&text, finding.start),
                end: utf16_position_at(&text, finding.end),
            },
            severity: Some(match finding.severity {
                Severity::Error => lsp::DiagnosticSeverity::ERROR,
                Severity::Warning => lsp::DiagnosticSeverity::WARNING,
                Severity::Note => lsp::DiagnosticSeverity::INFORMATION,
            }),
            code: finding.code.clone().map(lsp::NumberOrString::String),
            source: Some(SOURCE.to_string()),
            message: finding.message.clone(),
            related_information: (!related.is_empty()).then_some(related),
            ..Default::default()
        };
        match by_file.iter_mut().find(|(file, _)| *file == finding.file) {
            Some((_, diagnostics)) => diagnostics.push(diagnostic),
            None => by_file.push((finding.file.clone(), vec![diagnostic])),
        }
    }
    by_file
}

/// The position a byte offset falls at, counted the way the protocol counts:
/// lines from zero, and characters as UTF-16 code units.
///
/// Three units are in play and they disagree. On the line `int café = 1; // 🦀`
/// the semicolon sits at byte 13, at character 12, and at UTF-16 unit 12. The
/// front end reports the byte; the protocol asks for the UTF-16 unit. Only the
/// text can convert between them, which is why this takes the text.
///
/// An offset past the end, or one landing inside a character, is pulled back to
/// the nearest boundary at or before it rather than panicking: the front end
/// measured against a buffer the reader may have changed since.
fn utf16_position_at(text: &str, offset: usize) -> lsp::Position {
    let mut offset = offset.min(text.len());
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    let before = &text[..offset];
    let line = before.matches('\n').count() as u32;
    let line_started_at = before.rfind('\n').map_or(0, |newline| newline + 1);
    lsp::Position {
        line,
        character: before[line_started_at..].encode_utf16().count() as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    const LINES: &str = "int main() {\n  int café = 1; // 🦀\n  return café;\n}\n";

    /// The front end counts a column in bytes, the protocol counts it in UTF-16
    /// code units, and the two disagree on every line holding anything outside
    /// ASCII. Reading one as the other puts the diagnostic in the wrong place.
    #[test]
    fn a_byte_offset_becomes_a_utf16_position_and_not_a_byte_or_character_one() {
        let semicolon = LINES.find(';').expect("the semicolon");
        let line_start = LINES.find("  int").expect("the second line");

        // Three counts of the same place on that line, all different.
        assert_eq!(LINES[line_start..semicolon].len(), 15, "bytes");
        assert_eq!(
            LINES[line_start..semicolon].chars().count(),
            14,
            "characters"
        );
        assert_eq!(
            LINES[line_start..semicolon].encode_utf16().count(),
            14,
            "UTF-16 units"
        );

        assert_eq!(
            utf16_position_at(LINES, semicolon),
            lsp::Position::new(1, 14)
        );
    }

    /// A surrogate pair is where the character count and the UTF-16 count part
    /// company, and a crab is the cheapest way to prove it.
    #[test]
    fn a_character_outside_the_basic_plane_counts_as_two_units() {
        let after_crab = LINES
            .find("\n  return")
            .expect("the end of the second line");
        assert_eq!(
            utf16_position_at(LINES, after_crab),
            lsp::Position::new(1, 21),
            "the crab is one character and two UTF-16 units"
        );
    }

    /// The parse was measured against a buffer the reader may have changed
    /// since, so an offset past the end lands at the end rather than panicking.
    #[test]
    fn an_offset_past_the_end_lands_at_the_end() {
        assert_eq!(utf16_position_at("x;\n", 9_000), lsp::Position::new(1, 0));
        assert_eq!(utf16_position_at("", 0), lsp::Position::new(0, 0));
        assert_eq!(utf16_position_at("", 5), lsp::Position::new(0, 0));
    }

    /// An offset landing inside a character is pulled back to the boundary
    /// before it. Slicing a `str` there would panic, and taking the editor down
    /// over a stale offset is not an option.
    #[test]
    fn an_offset_inside_a_character_is_pulled_back_rather_than_panicked_on() {
        let crab = LINES.find('\u{1f980}').expect("the crab");
        assert_eq!(
            utf16_position_at(LINES, crab + 1),
            utf16_position_at(LINES, crab),
        );
    }

    /// A note is not a warning and a warning is not an error: painting a file
    /// red over a `-Wunused-variable` is how a source stops being read.
    #[test]
    fn each_severity_keeps_its_own_weight() {
        let findings =
            [Severity::Error, Severity::Warning, Severity::Note].map(|severity| Finding {
                severity,
                message: "something".to_string(),
                code: None,
                file: PathBuf::from("/p/a.cpp"),
                start: 0,
                end: 1,
                notes: Vec::new(),
            });
        let shown: Vec<_> = only_file(as_diagnostics_by_file(&findings, |_| {
            Some("x;\n".to_string())
        }))
        .into_iter()
        .map(|diagnostic| diagnostic.severity)
        .collect();
        assert_eq!(
            shown,
            vec![
                Some(lsp::DiagnosticSeverity::ERROR),
                Some(lsp::DiagnosticSeverity::WARNING),
                Some(lsp::DiagnosticSeverity::INFORMATION),
            ]
        );
    }

    /// Every finding says where it came from. A file can carry diagnostics from
    /// several sources at once, and the reader deciding whether to believe one
    /// starts by seeing which said it.
    #[test]
    fn every_finding_names_the_source_it_came_from() {
        let findings = [Finding {
            severity: Severity::Error,
            message: "expected ';'".to_string(),
            code: None,
            file: PathBuf::from("/p/a.cpp"),
            start: 3,
            end: 3,
            notes: Vec::new(),
        }];
        let shown = only_file(as_diagnostics_by_file(&findings, |_| {
            Some("int x\n".to_string())
        }));
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].source.as_deref(), Some("clang"));
        assert_eq!(shown[0].message, "expected ';'");
        assert_eq!(shown[0].range.start, lsp::Position::new(0, 3));
        assert_eq!(shown[0].code, None, "a hard error has no flag to name");
    }

    /// A finding in a header goes to the header. A reader told that a header's
    /// missing semicolon is on a line of the file that included it would go
    /// looking at code that is fine.
    #[test]
    fn a_finding_is_filed_against_the_file_it_is_in() {
        let findings = [
            Finding {
                severity: Severity::Error,
                message: "expected ';' after class member declaration".to_string(),
                code: None,
                file: PathBuf::from("/p/thing.h"),
                start: 12,
                end: 12,
                notes: Vec::new(),
            },
            Finding {
                severity: Severity::Warning,
                message: "unused variable 'spare'".to_string(),
                code: Some("-Wunused-variable".to_string()),
                file: PathBuf::from("/p/a.cpp"),
                start: 0,
                end: 5,
                notes: Vec::new(),
            },
        ];
        let by_file = as_diagnostics_by_file(&findings, |file| match file.to_str() {
            Some("/p/thing.h") => Some("class Thing {\n  int one\n};\n".to_string()),
            _ => Some("int spare;\n".to_string()),
        });

        assert_eq!(
            by_file
                .iter()
                .map(|(file, diagnostics)| (file.clone(), diagnostics.len()))
                .collect::<Vec<_>>(),
            vec![
                (PathBuf::from("/p/thing.h"), 1),
                (PathBuf::from("/p/a.cpp"), 1),
            ]
        );
        assert_eq!(by_file[0].1[0].range.start, lsp::Position::new(0, 12));
        assert_eq!(
            by_file[1].1[0].code,
            Some(lsp::NumberOrString::String("-Wunused-variable".to_string())),
            "the warning carries the flag that fired it",
        );
    }

    /// A note keeps its own file and line, so a reader can go to the
    /// declaration it names rather than read a path out of a message.
    #[test]
    fn a_note_arrives_as_a_location_and_not_as_text() {
        let findings = [Finding {
            severity: Severity::Error,
            message: "redefinition of 'Thing'".to_string(),
            code: None,
            file: PathBuf::from("/p/a.cpp"),
            start: 0,
            end: 5,
            notes: vec![Note {
                message: "previous definition is here".to_string(),
                file: PathBuf::from("/p/thing.h"),
                start: 6,
                end: 11,
            }],
        }];
        let shown = only_file(as_diagnostics_by_file(&findings, |file| {
            match file.to_str() {
                Some("/p/thing.h") => Some("class Thing {};\n".to_string()),
                _ => Some("class Thing {};\n".to_string()),
            }
        }));
        let related = shown[0]
            .related_information
            .as_ref()
            .expect("the note came through");
        assert_eq!(related.len(), 1);
        assert_eq!(related[0].message, "previous definition is here");
        assert_eq!(related[0].location.range.start, lsp::Position::new(0, 6));
        assert!(
            related[0].location.uri.to_string().ends_with("/p/thing.h"),
            "the note points at the header it is in, not at the file being read",
        );
    }

    /// A file whose text nobody can produce is left out. Its offsets mean
    /// nothing without it, and a diagnostic placed at a guessed position points
    /// the reader at innocent code.
    #[test]
    fn a_file_with_no_text_is_left_out_rather_than_guessed_at() {
        let findings = [Finding {
            severity: Severity::Error,
            message: "expected ';'".to_string(),
            code: None,
            file: PathBuf::from("/p/gone.h"),
            start: 3,
            end: 3,
            notes: Vec::new(),
        }];
        assert!(as_diagnostics_by_file(&findings, |_| None).is_empty());
    }

    fn only_file(by_file: Vec<(PathBuf, Vec<lsp::Diagnostic>)>) -> Vec<lsp::Diagnostic> {
        assert_eq!(by_file.len(), 1, "one file was expected");
        by_file
            .into_iter()
            .next()
            .map(|(_, diagnostics)| diagnostics)
            .unwrap_or_default()
    }
}
