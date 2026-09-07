use quick_xml::errors::{Error, IllFormedError, SyntaxError};
use quick_xml::events::attributes::AttrError;
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::ResolveResult;
use quick_xml::reader::NsReader;

mod watching;

pub use watching::{Check, init};

/// The name the editor files these under, so a reader can tell them from a
/// language server's.
const SOURCE: &str = "xml";

/// One thing wrong with a document, before it is placed in the text.
struct Fault {
    /// Where it is, as byte offsets into the whole document. The parser
    /// counts bytes and nothing else, which is the one unit that converts to
    /// the protocol's without ambiguity.
    at: std::ops::Range<usize>,
    /// A stable name for the kind of fault, so a reader can look one up or
    /// tell two apart at a glance. The parser has no codes of its own, so
    /// these are named after its own words for each fault.
    code: &'static str,
    grade: lsp::DiagnosticSeverity,
    message: String,
}

/// Everything wrong with one XML document, with no language server anywhere.
///
/// Two grades, and they mean different things. A fault the parser could not
/// read past is an error: no tool will read the document, and nothing else
/// can be said about it -- which is also why it is the last thing reported,
/// the parser having stopped there. An undeclared namespace prefix is a
/// warning: the document is well-formed, and whether the prefix is declared
/// is settled outside this file, so a fragment meant to be included
/// elsewhere is not wrong for lacking the declaration.
pub fn faults_in(text: &str) -> Vec<lsp::Diagnostic> {
    let mut reader = NsReader::from_str(text);
    let mut faults = Vec::new();
    // The parser reports a start tag left open only to whoever reads a subtree
    // to its end, which nothing here does: at the end of the input it says
    // `Eof` and no more. So the open tags are counted here, and whatever is
    // still open when the input runs out is a document that was cut short.
    let mut still_open: Vec<(String, std::ops::Range<usize>)> = Vec::new();
    loop {
        let started_at = reader.buffer_position() as usize;
        // `read_event` rather than `read_resolved_event`: the resolved name
        // borrows the reader, which the positions below have to be asked for
        // as well.
        match reader.read_event() {
            Ok(Event::Eof) => {
                for (name, at) in still_open.drain(..) {
                    faults.push(Fault {
                        at,
                        code: "missing-end-tag",
                        grade: lsp::DiagnosticSeverity::ERROR,
                        message: format!("start tag `{name}` is never closed"),
                    });
                }
                break;
            }
            Ok(event) => {
                let ends_at = reader.buffer_position() as usize;
                if let Event::End(_) = &event {
                    // Which end tag matches which start tag is the parser's
                    // own check, and it stops on a mismatch, so by here the
                    // innermost open tag is the one being closed.
                    still_open.pop();
                }
                // Only where a tag opens. An end tag carries the same prefix
                // as the start tag it closes, so resolving it as well would
                // report every undeclared prefix twice.
                let tag = match &event {
                    Event::Start(tag) => {
                        still_open.push((
                            String::from_utf8_lossy(tag.name().as_ref()).into_owned(),
                            started_at..ends_at,
                        ));
                        tag
                    }
                    Event::Empty(tag) => tag,
                    _ => continue,
                };
                if let Some(prefix) = undeclared_prefix_of(&reader, tag) {
                    faults.push(Fault {
                        at: started_at..ends_at,
                        code: "undeclared-namespace-prefix",
                        grade: lsp::DiagnosticSeverity::WARNING,
                        message: format!(
                            "namespace prefix `{prefix}` is not declared in this document"
                        ),
                    });
                }
                // The parser reads a tag's attributes only when they are
                // asked for, so a duplicate or an unquoted value reaches
                // nobody until this iterates them.
                if let Some(error) = tag.attributes().find_map(|attribute| attribute.err()) {
                    let (offset, code) = what_the_attribute_error_says(&error);
                    faults.push(Fault {
                        // The offset the parser gives is counted from the
                        // byte after the tag's `<`, which is where the
                        // content it was handed begins.
                        at: (started_at + 1 + offset).min(ends_at)..ends_at,
                        code,
                        grade: lsp::DiagnosticSeverity::ERROR,
                        message: error.to_string(),
                    });
                    break;
                }
            }
            Err(error) => {
                faults.push(Fault {
                    at: reader.error_position() as usize..reader.buffer_position() as usize,
                    code: what_the_error_says(&error),
                    grade: lsp::DiagnosticSeverity::ERROR,
                    message: error.to_string(),
                });
                break;
            }
        }
    }
    let lines = Lines::of(text);
    faults
        .into_iter()
        .map(|fault| lsp::Diagnostic {
            range: lines.covering(fault.at),
            severity: Some(fault.grade),
            code: Some(lsp::NumberOrString::String(fault.code.to_string())),
            source: Some(SOURCE.to_string()),
            message: fault.message,
            ..Default::default()
        })
        .collect()
}

/// The prefix a tag's name carries where the document declares no namespace
/// for it, and nothing where it declares one or the tag carries none.
fn undeclared_prefix_of(reader: &NsReader<&[u8]>, tag: &BytesStart<'_>) -> Option<String> {
    match reader.resolve_element(tag.name()).0 {
        ResolveResult::Unknown(prefix) => Some(String::from_utf8_lossy(&prefix).into_owned()),
        ResolveResult::Bound(_) | ResolveResult::Unbound => None,
    }
}

/// A stable name for each fault the parser can stop on.
///
/// Matched variant by variant rather than by the message text: the messages
/// are prose meant for a reader and may be reworded, while a code a reader
/// has learnt to recognise must not change under them.
fn what_the_error_says(error: &Error) -> &'static str {
    match error {
        Error::Syntax(SyntaxError::InvalidBangMarkup) => "invalid-markup",
        Error::Syntax(SyntaxError::UnclosedPIOrXmlDecl) => "unclosed-declaration",
        Error::Syntax(SyntaxError::UnclosedComment) => "unclosed-comment",
        Error::Syntax(SyntaxError::UnclosedDoctype) => "unclosed-doctype",
        Error::Syntax(SyntaxError::UnclosedCData) => "unclosed-cdata",
        Error::Syntax(SyntaxError::UnclosedTag) => "unclosed-tag",
        Error::IllFormed(IllFormedError::MissingDeclVersion(_)) => "missing-declaration-version",
        Error::IllFormed(IllFormedError::MissingDoctypeName) => "missing-doctype-name",
        Error::IllFormed(IllFormedError::MissingEndTag(_)) => "missing-end-tag",
        Error::IllFormed(IllFormedError::UnmatchedEndTag(_)) => "unmatched-end-tag",
        Error::IllFormed(IllFormedError::MismatchedEndTag { .. }) => "mismatched-end-tag",
        Error::IllFormed(IllFormedError::DoubleHyphenInComment) => "double-hyphen-in-comment",
        Error::IllFormed(IllFormedError::UnclosedReference) => "unclosed-reference",
        Error::InvalidAttr(attribute) => what_the_attribute_error_says(attribute).1,
        Error::Escape(_) => "invalid-reference",
        Error::Encoding(_) => "unreadable-encoding",
        Error::Namespace(_) => "invalid-namespace-declaration",
        Error::Io(_) => "unreadable",
    }
}

/// Where an attribute fault is, counted from the byte after the tag's `<`,
/// and a stable name for it.
fn what_the_attribute_error_says(error: &AttrError) -> (usize, &'static str) {
    match error {
        AttrError::ExpectedEq(at) => (*at, "attribute-without-value"),
        AttrError::ExpectedValue(at) => (*at, "attribute-without-value"),
        AttrError::UnquotedValue(at) => (*at, "unquoted-attribute-value"),
        AttrError::ExpectedQuote(at, _) => (*at, "unclosed-attribute-value"),
        AttrError::Duplicated(at, _) => (*at, "duplicate-attribute"),
    }
}

/// Where each line of a document starts, so a byte offset can be turned into
/// the line and character the protocol asks for.
struct Lines<'a> {
    text: &'a str,
    /// The byte offset each line begins at, first line first.
    starts: Vec<usize>,
}

impl<'a> Lines<'a> {
    fn of(text: &'a str) -> Self {
        let mut starts = vec![0];
        starts.extend(
            text.char_indices()
                .filter(|(_, character)| *character == '\n')
                .map(|(at, _)| at + 1),
        );
        Self { text, starts }
    }

    fn covering(&self, at: std::ops::Range<usize>) -> lsp::Range {
        lsp::Range {
            start: self.position_at(at.start),
            end: self.position_at(at.end.max(at.start)),
        }
    }

    /// The position a byte offset falls at, counted the way the protocol
    /// counts: lines from zero, and characters as UTF-16 code units.
    ///
    /// Three units are in play and they disagree. On the line
    /// `  <café note="🦀🔥">text</cafe>` the closing tag's `<` sits at byte
    /// 29, at character 22 and at UTF-16 unit 24. The parser counts bytes and
    /// nothing else; the protocol asks for the UTF-16 unit. Only the text can
    /// convert between them, which is why this holds the text.
    ///
    /// An offset past the end, or one landing inside a character, is moved
    /// back to the nearest boundary rather than panicking on a slice.
    fn position_at(&self, at: usize) -> lsp::Position {
        let mut at = at.min(self.text.len());
        while at > 0 && !self.text.is_char_boundary(at) {
            at -= 1;
        }
        let line = self
            .starts
            .partition_point(|start| *start <= at)
            .saturating_sub(1);
        let line_starts_at = self.starts.get(line).copied().unwrap_or(0);
        let character = self
            .text
            .get(line_starts_at..at)
            .unwrap_or("")
            .encode_utf16()
            .count();
        lsp::Position {
            line: line as u32,
            character: character as u32,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// Real documents, kept verbatim so that every offset asserted below was
    /// measured on the exact bytes the parser reads. `xmllint --noout` was
    /// run over each of them and agrees on which are well-formed and which
    /// are not.
    const MISMATCHED: &str = include_str!("../test_data/mismatched-end-tag.xml");
    const WELL_FORMED: &str = include_str!("../test_data/well-formed.xml");
    const UNCLOSED: &str = include_str!("../test_data/unclosed-tag.xml");
    const DUPLICATE_ATTRIBUTE: &str = include_str!("../test_data/duplicate-attribute.xml");
    const UNDECLARED_PREFIX: &str = include_str!("../test_data/undeclared-prefix.xml");

    fn described(found: &[lsp::Diagnostic]) -> Vec<(String, Option<lsp::DiagnosticSeverity>)> {
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

    /// A document nothing is wrong with reports nothing, which is what takes
    /// down the underlining of a fault the reader has just fixed. The editor
    /// keeps what it was last given, so an empty report is a different thing
    /// from silence.
    #[test]
    fn a_well_formed_document_reports_nothing_at_all() {
        assert_eq!(faults_in(WELL_FORMED), Vec::new());
        assert_eq!(faults_in(""), Vec::new());
    }

    /// The parser counts bytes, the protocol counts UTF-16 code units, and
    /// the reader sees characters. All three disagree on the same line, and
    /// reading one as another puts every diagnostic on a line with an emoji
    /// in the wrong column.
    #[test]
    fn a_byte_offset_becomes_a_utf16_column_and_not_a_byte_or_character_one() {
        let line = MISMATCHED.lines().nth(2).expect("the café line");
        let closing = line.find("</cafe>").expect("the closing tag");

        // Three counts of the same place, all different.
        assert_eq!(line[..closing].len(), 29, "bytes");
        assert_eq!(line[..closing].chars().count(), 22, "characters");
        assert_eq!(line[..closing].encode_utf16().count(), 24, "UTF-16 units");

        let found = faults_in(MISMATCHED);
        assert_eq!(
            described(&found),
            vec![(
                "mismatched-end-tag".to_string(),
                Some(lsp::DiagnosticSeverity::ERROR)
            )]
        );
        assert_eq!(
            found[0].range.start,
            lsp::Position::new(2, 24),
            "the protocol's own unit -- not the 29 bytes the parser counted"
        );
        assert_eq!(
            found[0].message,
            "ill-formed document: expected `</café>`, but `</cafe>` was found"
        );
        assert_eq!(found[0].source.as_deref(), Some("xml"));
    }

    /// A document that ends mid-tag is reported rather than dropped. It is
    /// what a file being typed looks like, and the reader wants to know
    /// which tag is still open.
    #[test]
    fn a_document_the_parser_cannot_finish_is_reported_rather_than_dropped() {
        let found = faults_in(UNCLOSED);
        assert_eq!(
            described(&found),
            vec![(
                "missing-end-tag".to_string(),
                Some(lsp::DiagnosticSeverity::ERROR)
            )]
        );
        assert_eq!(
            found[0].range.start,
            lsp::Position::new(1, 0),
            "the `<root>` still open"
        );

        // Every tag left open is named where it opens. The parser itself says
        // only `Eof` here, so a document cut off mid-nesting would otherwise
        // read as well-formed.
        let both = faults_in("<root>\n  <item>\n");
        assert_eq!(
            described(&both),
            vec![
                (
                    "missing-end-tag".to_string(),
                    Some(lsp::DiagnosticSeverity::ERROR)
                ),
                (
                    "missing-end-tag".to_string(),
                    Some(lsp::DiagnosticSeverity::ERROR)
                ),
            ]
        );
        assert_eq!(both[0].message, "start tag `root` is never closed");
        assert_eq!(both[0].range.start, lsp::Position::new(0, 0));
        assert_eq!(both[1].range.start, lsp::Position::new(1, 2));
    }

    /// The same attribute twice is ill-formed, and the parser only says so
    /// when a tag's attributes are read -- which nothing else does. The
    /// offset it gives is counted from the byte after the tag's `<`.
    #[test]
    fn the_same_attribute_twice_is_reported_at_the_attribute_and_not_the_tag() {
        let found = faults_in(DUPLICATE_ATTRIBUTE);
        assert_eq!(
            described(&found),
            vec![(
                "duplicate-attribute".to_string(),
                Some(lsp::DiagnosticSeverity::ERROR)
            )]
        );
        let line = DUPLICATE_ATTRIBUTE.lines().nth(1).expect("the tag");
        let second = line.rfind("width").expect("the second `width`");
        assert_eq!(
            found[0].range.start,
            lsp::Position::new(1, line[..second].encode_utf16().count() as u32),
            "the second `width`, and not the `<` the tag starts at"
        );
    }

    /// An undeclared namespace prefix is a warning and not an error: the
    /// document is well-formed, and whether the prefix is declared is settled
    /// outside this file -- a fragment meant to be included elsewhere is not
    /// wrong for lacking the declaration.
    #[test]
    fn an_undeclared_prefix_is_a_warning_and_the_document_is_read_to_the_end() {
        let found = faults_in(UNDECLARED_PREFIX);
        assert_eq!(
            described(&found),
            vec![
                (
                    "undeclared-namespace-prefix".to_string(),
                    Some(lsp::DiagnosticSeverity::WARNING)
                ),
                (
                    "undeclared-namespace-prefix".to_string(),
                    Some(lsp::DiagnosticSeverity::WARNING)
                ),
            ],
            "both tags, so a warning does not stop the read the way an error does"
        );
        assert_eq!(
            found[0].message,
            "namespace prefix `xsl` is not declared in this document"
        );
    }

    /// A prefix the document declares is not complained about, whether it is
    /// declared on the element that uses it or on one above.
    #[test]
    fn a_declared_prefix_is_not_complained_about() {
        for document in [
            "<xsl:root xmlns:xsl=\"http://x\"><xsl:leaf/></xsl:root>",
            "<root xmlns:xsl=\"http://x\"><xsl:leaf/></root>",
        ] {
            assert_eq!(faults_in(document), Vec::new(), "{document}");
        }
    }

    /// Text that is not XML at all is a fault reported at a place, not a
    /// panic and not silence. A reader who has pasted the wrong thing into a
    /// `.xml` file needs to be told so.
    #[test]
    fn text_that_is_not_xml_at_all_is_reported_rather_than_panicked_on() {
        for text in ["<", "<<<", "</>", "</unopened>", "<!-- never closed", "&"] {
            let found = faults_in(text);
            assert_eq!(found.len(), 1, "{text:?} yielded {found:?}");
            assert_eq!(
                found[0].severity,
                Some(lsp::DiagnosticSeverity::ERROR),
                "{text:?}"
            );
        }
    }

    /// An offset past the end, or one landing inside a character, is moved
    /// back to a boundary. The parser reads the document the reader has since
    /// changed, and a slice through the middle of an emoji would panic.
    #[test]
    fn an_offset_past_the_end_or_inside_a_character_lands_on_a_boundary() {
        let text = "a\u{1f980}b\n";
        let lines = Lines::of(text);
        assert_eq!(lines.position_at(0), lsp::Position::new(0, 0));
        // Inside the crab, which is four bytes and two UTF-16 units.
        assert_eq!(lines.position_at(3), lsp::Position::new(0, 1));
        assert_eq!(lines.position_at(5), lsp::Position::new(0, 3));
        assert_eq!(lines.position_at(9_000), lsp::Position::new(1, 0));
        assert_eq!(Lines::of("").position_at(9_000), lsp::Position::new(0, 0));
    }
}
