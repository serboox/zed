use std::sync::Arc;

use gpui::App;
use language::Buffer;
use project::{InProcessFormatting, InProcessFormattingRequest};

pub fn init(cx: &mut App) {
    project::register_in_process_formatting(Arc::new(TomlFormatting), cx);
}

struct TomlFormatting;

impl InProcessFormatting for TomlFormatting {
    fn formats(&self, buffer: &Buffer) -> bool {
        buffer
            .language()
            .is_some_and(|language| language.name() == "TOML")
    }

    fn format(&self, request: InProcessFormattingRequest) -> Option<String> {
        format(&request.text, indent_of(&request))
    }
}

/// The indentation a buffer's own settings ask for. The formatter is told a
/// string rather than a width because a reader who asked for tabs gets tabs.
fn indent_of(request: &InProcessFormattingRequest) -> String {
    if request.hard_tabs {
        "\t".to_string()
    } else {
        " ".repeat(request.tab_size.max(1) as usize)
    }
}

/// This text formatted, or nothing where it is already formatted or cannot
/// be read.
///
/// A file the parser rejected is handed back unchanged. Formatting a half
/// written document means rewriting it around a mistake, and what comes out
/// is neither what the reader wrote nor what they meant -- so a broken file
/// keeps every character it has until the mistake is fixed.
pub fn format(text: &str, indent: String) -> Option<String> {
    let parsed = taplo::parser::parse(text);
    if !parsed.errors.is_empty() {
        return None;
    }
    let options = taplo::formatter::Options {
        indent_string: indent,
        // Keys stay in the order the reader wrote them. Sorting a
        // `Cargo.toml` on save would rewrite a file nobody asked to have
        // rewritten, and would fight whatever order the project keeps.
        reorder_keys: false,
        reorder_arrays: false,
        ..Default::default()
    };
    let formatted = taplo::formatter::format_syntax(parsed.into_syntax(), options);
    (formatted != text).then_some(formatted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn formatted(text: &str) -> Option<String> {
        format(text, "  ".to_string())
    }

    /// Real taplo output over a real file: the padding around `=` is
    /// normalised, the blank lines are kept and the file ends in a newline.
    #[test]
    fn a_file_written_carelessly_comes_back_tidy() {
        let text =
            "[package]\nname=\"zed\"\nversion   =    \"0.1.0\"\n\n[dependencies]\nanyhow=\"1\"";
        let formatted = formatted(text).expect("there is something to change");
        assert_eq!(
            formatted,
            "[package]\nname = \"zed\"\nversion = \"0.1.0\"\n\n[dependencies]\nanyhow = \"1\"\n"
        );
    }

    /// A file already in the shape the formatter wants is left exactly as it
    /// is, and says so by answering with nothing. A formatter that always
    /// answers puts an empty change into every save.
    #[test]
    fn a_file_already_formatted_is_not_rewritten() {
        let text = "[package]\nname = \"zed\"\n\n[dependencies]\nanyhow = \"1\"\n";
        assert_eq!(formatted(text), None, "nothing to change");
    }

    /// The mistake stays where the reader can see it, and every character
    /// they wrote stays with it.
    #[test]
    fn a_file_the_parser_rejected_is_handed_back_untouched() {
        assert_eq!(formatted("[package]\nname = \n"), None);
        assert_eq!(formatted("[package\nname = \"zed\"\n"), None);
    }

    /// The reader's own indentation setting reaches the formatter, so a
    /// project that indents with tabs is not quietly converted to spaces.
    #[test]
    fn the_indentation_the_reader_asked_for_is_the_one_that_is_written() {
        let text = "[a]\nb = 1\n".to_string();
        assert_eq!(
            indent_of(&InProcessFormattingRequest {
                text: text.clone(),
                tab_size: 4,
                hard_tabs: true,
            }),
            "\t"
        );
        assert_eq!(
            indent_of(&InProcessFormattingRequest {
                text,
                tab_size: 4,
                hard_tabs: false,
            }),
            "    "
        );
    }
}
