mod watching;

pub use watching::{Check, init};

/// How `php -l` opens the one line that matters.
const PARSE_ERROR: &str = "PHP Parse error:";

/// What follows the message and precedes the file name.
const IN_FILE: &str = " in ";

/// What follows the file name and precedes the line number.
const ON_LINE: &str = " on line ";

/// Reads what `php -l` said into a diagnostic the editor can show, with no
/// language server anywhere.
///
/// `named` is the path php was given; a report about any other file is
/// dropped, because a finding put on the wrong file underlines a line nobody
/// wrote. `text` is that file's own text, needed to widen the finding to the
/// whole line.
///
/// At most one diagnostic comes back, and that is php's doing rather than a
/// simplification: `php -l` stops at the first parse error, so there is never
/// a second one to report.
///
/// The finding covers the whole line. php names no column at all -- its
/// message ends at the line number -- and a diagnostic that claims a column
/// it does not know would put the squiggle in an arbitrary place.
pub fn what_php_reported(output: &str, named: &str, text: &str) -> Vec<lsp::Diagnostic> {
    let Some(reported) = output
        .lines()
        .find_map(|line| a_parse_error_in(line, named))
    else {
        return Vec::new();
    };
    vec![lsp::Diagnostic {
        range: the_whole_line(text, reported.line),
        severity: Some(lsp::DiagnosticSeverity::ERROR),
        source: Some("php -l".to_string()),
        message: reported.message,
        ..Default::default()
    }]
}

/// One parse error, as much of it as php gives.
struct Reported {
    /// Zero-based, the way the protocol counts, from the one-based number php
    /// prints.
    line: u32,
    message: String,
}

/// Reads one output line, where it is a parse error about the file that was
/// asked about.
///
/// The path is matched by the tail of the line rather than by splitting on
/// `" in "`: a php message can hold that word itself -- "unexpected token
/// \"in\"" among them -- and splitting on the first occurrence would cut the
/// message in half and lose the path.
fn a_parse_error_in(line: &str, named: &str) -> Option<Reported> {
    let said = line.strip_prefix(PARSE_ERROR)?;
    let (before_number, number) = said.rsplit_once(ON_LINE)?;
    let ending = format!("{IN_FILE}{named}");
    let message = before_number.strip_suffix(&ending)?;
    Some(Reported {
        line: number.trim().parse::<u32>().ok()?.saturating_sub(1),
        message: message.trim().to_string(),
    })
}

/// The whole of one line, in the UTF-16 code units the protocol counts.
///
/// A line past the end of the file yields an empty range on that line rather
/// than nothing: php counts the line after the last one as the place an
/// unclosed brace should have been closed, and that is a real answer.
fn the_whole_line(text: &str, line: u32) -> lsp::Range {
    let units = text
        .lines()
        .nth(line as usize)
        .map(|found| found.encode_utf16().count() as u32)
        .unwrap_or(0);
    lsp::Range {
        start: lsp::Position::new(line, 0),
        end: lsp::Position::new(line, units),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    const NAMED: &str = "bad.php";

    /// Verbatim from `php -l` on PHP 8.5.10, over a function body missing a
    /// semicolon. Note the two spaces after the colon, which is php's own
    /// spacing and not a typo here.
    const A_MISSING_SEMICOLON: &str = concat!(
        "PHP Parse error:  syntax error, unexpected token \"}\", expecting \",\"",
        " or \";\" in bad.php on line 4\n",
        "Errors parsing bad.php\n"
    );

    #[test]
    fn a_parse_error_is_read_off_the_line_php_prints_it_on() {
        let text = "<?php\nfunction greet($name) {\n    echo \"hello\"\n}\n";
        let found = what_php_reported(A_MISSING_SEMICOLON, NAMED, text);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(
            found[0].message, "syntax error, unexpected token \"}\", expecting \",\" or \";\"",
            "the message keeps everything php said and nothing it did not"
        );
        assert_eq!(
            found[0].range,
            lsp::Range {
                start: lsp::Position::new(3, 0),
                end: lsp::Position::new(3, 1),
            },
            "php's line 4 is the protocol's line 3, covered whole"
        );
        assert_eq!(found[0].severity, Some(lsp::DiagnosticSeverity::ERROR));
    }

    /// A message that holds the word this parser splits on has to survive it.
    /// "unexpected token \"in\"" is a real php message, and splitting on the
    /// first `" in "` would cut it in half and lose the path with it.
    #[test]
    fn a_message_holding_the_word_in_is_not_cut_in_half() {
        let output =
            "PHP Parse error:  syntax error, unexpected token \"in\" in bad.php on line 2\n";
        let found = what_php_reported(output, NAMED, "<?php\n$x in;\n");
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].message, "syntax error, unexpected token \"in\"");
        assert_eq!(found[0].range.start.line, 1);
    }

    /// A clean file gets nothing, and that is the whole of it: php says so on
    /// its own line, which matches no parse error and so yields none.
    #[test]
    fn a_clean_file_reports_nothing() {
        let output = "No syntax errors detected in ok.php\n";
        assert!(what_php_reported(output, "ok.php", "<?php\necho 1;\n").is_empty());
    }

    /// An `include`d file php read on its own account is not this file, and
    /// its fault belongs on it rather than here.
    #[test]
    fn a_fault_about_another_file_is_left_on_that_file() {
        let output = "PHP Parse error:  syntax error in other.php on line 9\n";
        assert!(what_php_reported(output, NAMED, "<?php\n").is_empty());
    }

    /// A line past the last one is php's answer for a brace never closed, and
    /// it is kept: an empty range at the file's end still tells the reader
    /// where the fault is.
    #[test]
    fn a_line_past_the_end_of_the_file_is_still_placed() {
        let output =
            "PHP Parse error:  syntax error, unexpected end of file in bad.php on line 9\n";
        let found = what_php_reported(output, NAMED, "<?php\nif (1) {\n");
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(
            found[0].range,
            lsp::Range {
                start: lsp::Position::new(8, 0),
                end: lsp::Position::new(8, 0),
            }
        );
    }

    /// The line is covered in the units the protocol counts, which a line
    /// carrying anything outside ASCII is the only way to tell apart. The
    /// three numbers differ, and only one of them is right.
    #[test]
    fn the_line_is_covered_in_utf16_code_units() {
        let line = "$привет = \"🎉\"";
        let text = format!("<?php\n{line}\n");
        let output = "PHP Parse error:  syntax error in bad.php on line 2\n";
        let found = what_php_reported(output, NAMED, &text);
        assert_eq!(line.len(), 22, "bytes");
        assert_eq!(line.chars().count(), 13, "characters");
        assert_eq!(line.encode_utf16().count(), 14, "UTF-16 units");
        assert_eq!(found[0].range.end.character, 14);
    }

    /// Output that is not php's at all -- an empty run, a crash, a message
    /// this reader has never seen -- yields nothing rather than a wrong
    /// diagnostic.
    #[test]
    fn output_this_reader_does_not_recognise_yields_nothing() {
        for output in ["", "\n", "Segmentation fault", "PHP Parse error: no line"] {
            assert!(
                what_php_reported(output, NAMED, "<?php\n").is_empty(),
                "{output:?}"
            );
        }
    }
}
