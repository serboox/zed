mod watching;

pub use watching::{Check, init};

/// What ruby writes at the end of the one line naming the file and the line.
const SYNTAX_ERRORS_FOUND: &str = ": syntax errors found (SyntaxError)";

/// The gutter ruby draws down the left of the source it quotes back. Every
/// quoted line and every marker line carries it, which is what makes the two
/// kinds of line tellable apart: a marker line holds nothing before it but
/// spaces.
const GUTTER: char = '|';

/// The mark ruby puts under the place it stopped.
const MARK: char = '^';

/// Reads what `ruby -c` said into diagnostics the editor can show, with no
/// language server anywhere.
///
/// `named` is the path ruby was given, and a report about any other file is
/// dropped. `text` is that file's own text, needed to cover each finding's
/// whole line.
///
/// Every finding covers a whole line rather than the place ruby marked, and
/// that is a limit of the output rather than a choice. Ruby 3.4 prints the
/// source line back with the mark under it, but elides a line it thinks too
/// wide -- a 48-character line comes back as `... `, and the mark then sits
/// two columns into an ellipsis. Since the elision cannot be told from a
/// short line without re-measuring ruby's own idea of width, no column is
/// trusted from it and the line is covered whole.
///
/// Output that will not parse yields nothing rather than panicking, and one
/// unreadable marker does not cost the rest of them.
pub fn what_ruby_reported(output: &str, named: &str, text: &str) -> Vec<lsp::Diagnostic> {
    if !names_this_file(output, named) {
        return Vec::new();
    }
    let mut said = Vec::new();
    let mut line = None;
    for reported in output.lines() {
        if let Some(quoted) = a_quoted_source_line(reported) {
            line = Some(quoted);
        } else if let (Some(quoted), Some(message)) = (line, a_marks_message(reported)) {
            said.push(lsp::Diagnostic {
                range: the_whole_line(text, quoted),
                severity: Some(lsp::DiagnosticSeverity::ERROR),
                source: Some("ruby -c".to_string()),
                message,
                ..Default::default()
            });
        }
    }
    said
}

/// Whether this output is about the file that was asked about.
///
/// Ruby prefixes the line with its own binary's path -- `/usr/bin/ruby-mri:`
/// on this machine -- so the file name is matched inside the line rather than
/// at its start, and bounded on the right by the number and the words that
/// follow it.
fn names_this_file(output: &str, named: &str) -> bool {
    output.lines().any(|line| {
        line.ends_with(SYNTAX_ERRORS_FOUND)
            && line
                .strip_suffix(SYNTAX_ERRORS_FOUND)
                .and_then(|said| said.rsplit_once(':'))
                .is_some_and(|(before_number, _)| before_number.ends_with(named))
    })
}

/// The line number of a quoted source line, zero-based.
///
/// A quoted line is `  3 | text` or `> 3 | text`, the arrow marking the one
/// ruby stopped on. Only the number is read; the text after the gutter is the
/// part that may have been elided.
fn a_quoted_source_line(reported: &str) -> Option<u32> {
    let (before, _) = reported.split_once(GUTTER)?;
    let number = before.trim_start_matches(['>', ' ']).trim();
    number
        .parse::<u32>()
        .ok()
        .map(|line| line.saturating_sub(1))
}

/// The message on a marker line, which is everything after the mark.
///
/// A marker line holds only spaces before the gutter, which is how it is told
/// from a quoted source line whose text happens to start with the mark.
fn a_marks_message(reported: &str) -> Option<String> {
    let (before, after) = reported.split_once(GUTTER)?;
    if !before.chars().all(char::is_whitespace) {
        return None;
    }
    let (_, message) = after.split_once(MARK)?;
    let message = message.trim();
    (!message.is_empty()).then(|| message.to_string())
}

/// The whole of one line, in the UTF-16 code units the protocol counts.
///
/// A line past the end of the file yields an empty range on that line rather
/// than nothing: ruby names the line after the last one as the place an
/// unclosed `def` should have been closed, and that is a real answer.
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

    const NAMED: &str = "bad.rb";

    /// Verbatim from `ruby -c` on ruby 3.4.10 with Prism, over a `def` never
    /// closed. Three findings on one line, which is ordinary for this output.
    const A_DEF_NEVER_CLOSED: &str = concat!(
        "/usr/bin/ruby-mri: bad.rb:3: syntax errors found (SyntaxError)\n",
        "  1 | a = 1\n",
        "  2 | b = 2\n",
        "> 3 | def f(\n",
        "    |       ^ expected an `end` to close the `def` statement\n",
        "    |       ^ unexpected end-of-input, assuming it is closing the parent",
        " top level context\n",
        "    |       ^ unexpected end-of-input; expected a `)` to close the parameters\n",
        "\n"
    );

    #[test]
    fn every_marker_ruby_prints_becomes_its_own_finding_on_the_line_it_marks() {
        let text = "a = 1\nb = 2\ndef f(\n";
        let found = what_ruby_reported(A_DEF_NEVER_CLOSED, NAMED, text);
        assert_eq!(found.len(), 3, "{found:?}");
        assert_eq!(
            found
                .iter()
                .map(|one| one.message.as_str())
                .collect::<Vec<_>>(),
            vec![
                "expected an `end` to close the `def` statement",
                "unexpected end-of-input, assuming it is closing the parent top level context",
                "unexpected end-of-input; expected a `)` to close the parameters",
            ],
            "each marker keeps its own message rather than being glued together"
        );
        for one in &found {
            assert_eq!(
                one.range,
                lsp::Range {
                    start: lsp::Position::new(2, 0),
                    end: lsp::Position::new(2, 6),
                },
                "all three are on `def f(`, covered whole"
            );
            assert_eq!(one.severity, Some(lsp::DiagnosticSeverity::ERROR));
        }
    }

    /// The quoted lines ruby prints for context carry no marker, and a
    /// context line must not become a finding of its own.
    #[test]
    fn a_quoted_line_with_no_marker_under_it_reports_nothing() {
        let output = concat!(
            "/usr/bin/ruby-mri: bad.rb:2: syntax errors found (SyntaxError)\n",
            "  1 | a = 1\n",
            "> 2 | b = (\n",
            "    |     ^ unexpected end-of-input\n"
        );
        let found = what_ruby_reported(output, NAMED, "a = 1\nb = (\n");
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].range.start.line, 1);
    }

    /// Two lines, each with its own marker: the finding follows the last line
    /// quoted before it, not the one named in the header.
    #[test]
    fn a_marker_belongs_to_the_line_quoted_above_it() {
        let output = concat!(
            "/usr/bin/ruby-mri: bad.rb:1: syntax errors found (SyntaxError)\n",
            "> 1 | a = (\n",
            "    |     ^ first fault\n",
            "> 4 | b = [\n",
            "    |     ^ second fault\n"
        );
        let found = what_ruby_reported(output, NAMED, "a = (\nx\ny\nb = [\n");
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found[0].range.start.line, 0, "{:?}", found[0]);
        assert_eq!(found[1].range.start.line, 3, "{:?}", found[1]);
        assert_eq!(found[1].range.end.character, 5, "the whole of `b = [`");
    }

    /// A clean file gets nothing: ruby says `Syntax OK`, which names no file
    /// and holds no marker.
    #[test]
    fn a_clean_file_reports_nothing() {
        assert!(what_ruby_reported("Syntax OK\n", "ok.rb", "puts 1\n").is_empty());
    }

    /// A `require`d file ruby read on its own account is not this file, and
    /// its fault belongs on it rather than here.
    #[test]
    fn a_fault_about_another_file_is_left_on_that_file() {
        let output = concat!(
            "/usr/bin/ruby-mri: other.rb:1: syntax errors found (SyntaxError)\n",
            "> 1 | def f(\n",
            "    |       ^ unexpected end-of-input\n"
        );
        assert!(what_ruby_reported(output, NAMED, "puts 1\n").is_empty());
    }

    /// A file whose name ends with the asked-about name is a different file:
    /// `spec/bad.rb` is not `bad.rb` when `bad.rb` is what was asked about,
    /// but the check is by suffix and both would pass it. What matters is that
    /// the caller asks about a bare file name in that file's own directory,
    /// which is what the run does -- so this pins the behaviour rather than
    /// claiming the stricter one.
    #[test]
    fn the_name_is_matched_at_the_end_of_the_path_ruby_printed() {
        let output = concat!(
            "/usr/bin/ruby-mri: /tmp/project/bad.rb:1: syntax errors found (SyntaxError)\n",
            "> 1 | def f(\n",
            "    |       ^ unexpected end-of-input\n"
        );
        assert_eq!(what_ruby_reported(output, NAMED, "def f(\n").len(), 1);
    }

    /// A line past the last one is ruby's answer for a `def` never closed,
    /// and it is kept: an empty range at the file's end still tells the
    /// reader where the fault is.
    #[test]
    fn a_line_past_the_end_of_the_file_is_still_placed() {
        let output = concat!(
            "/usr/bin/ruby-mri: bad.rb:9: syntax errors found (SyntaxError)\n",
            "> 9 | \n",
            "    | ^ expected an `end`\n"
        );
        let found = what_ruby_reported(output, NAMED, "def f(\n");
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
        let line = "x = \"привет 🎉\" ; def f(";
        let text = format!("{line}\n");
        let output = concat!(
            "/usr/bin/ruby-mri: bad.rb:1: syntax errors found (SyntaxError)\n",
            "> 1 | ... \n",
            "    |     ^ unexpected end-of-input\n"
        );
        let found = what_ruby_reported(output, NAMED, &text);
        assert_eq!(line.len(), 32, "bytes");
        assert_eq!(line.chars().count(), 23, "characters");
        assert_eq!(line.encode_utf16().count(), 24, "UTF-16 units");
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(
            found[0].range.end.character, 24,
            "the whole line, whatever ruby elided its quote of it to"
        );
    }

    /// Output that is not ruby's at all yields nothing rather than a wrong
    /// diagnostic.
    #[test]
    fn output_this_reader_does_not_recognise_yields_nothing() {
        for output in ["", "\n", "bad.rb:1: syntax error, unexpected end"] {
            assert!(
                what_ruby_reported(output, NAMED, "puts 1\n").is_empty(),
                "{output:?}"
            );
        }
    }
}
