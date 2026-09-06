mod compile_commands;
mod parsing;
mod watching;

pub use compile_commands::{Entry, arguments_for, entries_in, where_the_database_is};
pub use parsing::{HELD_AT_MOST, Parsed, Request, ask_libclang};
pub use watching::{Diagnose, init};

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

/// One thing the front end said about the file it parsed.
///
/// Plain data, and deliberately so: it is read on the one thread that owns
/// `libclang` and answered on another, and nothing borrowed from a
/// translation unit may outlive the parse that produced it.
///
/// `start` and `end` are byte offsets into the text that was parsed, which is
/// the buffer's text and not the file on disk. The front end counts columns in
/// bytes and the protocol counts them in UTF-16 code units, so the offsets are
/// kept as they came and converted once, against that same text, in
/// [`as_diagnostics`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub severity: Severity,
    pub message: String,
    pub start: usize,
    pub end: usize,
}

/// The name the editor files these under, so a reader can see which of the
/// sources on a file said what.
pub const SOURCE: &str = "clang";

/// What the front end said, in the shape the editor shows.
///
/// `text` must be the text that was parsed. The offsets in a [`Finding`] are
/// only meaningful against it, and placing a diagnostic on the wrong column is
/// worse than not showing it.
pub fn as_diagnostics(findings: &[Finding], text: &str) -> Vec<lsp::Diagnostic> {
    findings
        .iter()
        .map(|finding| lsp::Diagnostic {
            range: lsp::Range {
                start: utf16_position_at(text, finding.start),
                end: utf16_position_at(text, finding.end),
            },
            severity: Some(match finding.severity {
                Severity::Error => lsp::DiagnosticSeverity::ERROR,
                Severity::Warning => lsp::DiagnosticSeverity::WARNING,
                Severity::Note => lsp::DiagnosticSeverity::INFORMATION,
            }),
            source: Some(SOURCE.to_string()),
            message: finding.message.clone(),
            ..Default::default()
        })
        .collect()
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
                start: 0,
                end: 1,
            });
        let shown: Vec<_> = as_diagnostics(&findings, "x;\n")
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
            start: 3,
            end: 3,
        }];
        let shown = as_diagnostics(&findings, "int x\n");
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].source.as_deref(), Some("clang"));
        assert_eq!(shown[0].message, "expected ';'");
        assert_eq!(shown[0].range.start, lsp::Position::new(0, 3));
    }
}
