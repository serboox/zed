use std::ops::Range;

use api_client::request::RawBodyContentType;

/// How a body is laid out.
///
/// XML and HTML are told apart because the text between their tags means
/// different things. In XML it is data a schema describes, and the whitespace
/// around it is layout; in HTML it is what the reader sees, and a line break
/// introduced between two words is a space that was not there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Shape {
    Brackets,
    Xml,
    Html,
}

/// The shape a raw body of this content type is laid out by, or nothing for
/// plain text, which has no structure to lay out.
pub(crate) fn shape_of(content_type: RawBodyContentType) -> Option<Shape> {
    match content_type {
        RawBodyContentType::Json | RawBodyContentType::JavaScript => Some(Shape::Brackets),
        RawBodyContentType::Xml => Some(Shape::Xml),
        RawBodyContentType::Html => Some(Shape::Html),
        RawBodyContentType::Text => None,
    }
}

/// The shape a content-type header names, or nothing where it names none.
///
/// The header and nothing else. Guessing from the first character of the body
/// is right when a parser has already agreed the body is JSON, and wrong here:
/// a plain-text answer that happens to open with a brace is plain text, and
/// laying it out would be rearranging somebody's prose.
pub(crate) fn shape_named_by(content_type: &str) -> Option<Shape> {
    if content_type.contains("json") || content_type.contains("javascript") {
        return Some(Shape::Brackets);
    }
    if content_type.contains("html") {
        return Some(Shape::Html);
    }
    if content_type.contains("xml") {
        return Some(Shape::Xml);
    }
    None
}

const INDENT: &str = "  ";

/// Lays `text` out again, changing only the whitespace between its tokens.
///
/// Every character that is not whitespace comes out again, in the order it
/// went in. That is the whole design: a body halfway through being typed is
/// exactly when a reader wants to see its shape, and a formatter that gives up
/// on anything it cannot parse is a formatter that is missing when it is
/// wanted. An unclosed brace, a half-typed string, a stray comma -- each lays
/// out as far as it goes and the rest follows it out unchanged.
///
/// What it deliberately does not do is rewrite what the tokens say. A parser
/// that read the body and printed it back would quietly turn `1e3` into
/// `1000.0` and a sixty-four-bit id into something shorter; a request body is
/// sent to somebody else's server, and it leaves this editor spelled the way
/// the reader spelled it.
pub(crate) fn laid_out_again(text: &str, shape: Shape) -> String {
    let mut out = match shape {
        Shape::Brackets => by_brackets(text),
        Shape::Xml => by_tags(text, false),
        Shape::Html => by_tags(text, true),
    };
    // The lines are separated, not terminated: this lays out a buffer a reader
    // is typing in, and a trailing newline there is an empty last line they
    // then have to delete.
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

/// One piece of a bracketed body: a run of characters that must come out
/// whole.
#[derive(Debug, PartialEq, Eq)]
enum Piece<'a> {
    Open(char),
    Close(char),
    Comma,
    Colon,
    /// A string, a block comment, a number, a name -- anything that is not
    /// punctuation the layout cares about.
    Whole(&'a str),
    /// A comment that runs to the end of its line. Nothing may follow it on
    /// that line: a token written after one is a token inside it, which is a
    /// formatter changing what the body says.
    ToEndOfLine(&'a str),
}

fn by_brackets(text: &str) -> String {
    let pieces = pieces_of(text);
    let mut out = String::with_capacity(text.len() + text.len() / 4);
    let mut depth: usize = 0;
    // What was written last, so a piece knows whether it needs a line of its
    // own, a space, or nothing at all before it.
    let mut last: Option<&Piece> = None;

    for (at, piece) in pieces.iter().enumerate() {
        match piece {
            Piece::Open(bracket) => {
                separate(&mut out, last, depth, Separation::SameLine);
                out.push(*bracket);
                // An empty pair reads better closed on the line it opened on,
                // and every other pair reads better opened out.
                if matches!(pieces.get(at + 1), Some(Piece::Close(_))) {
                    last = Some(piece);
                    continue;
                }
                depth += 1;
                out.push('\n');
                last = None;
            }
            Piece::Close(bracket) => {
                if matches!(last, Some(Piece::Open(_))) {
                    out.push(*bracket);
                    last = Some(piece);
                    continue;
                }
                depth = depth.saturating_sub(1);
                if !out.is_empty() && !out.ends_with('\n') {
                    out.push('\n');
                }
                indent(&mut out, depth);
                out.push(*bracket);
                last = Some(piece);
            }
            Piece::Comma => {
                out.push(',');
                out.push('\n');
                last = None;
            }
            Piece::Colon => {
                out.push(':');
                out.push(' ');
                // Written already, so what follows must not add a space of its
                // own or indent itself.
                last = Some(piece);
            }
            Piece::Whole(run) => {
                separate(&mut out, last, depth, Separation::Spaced);
                out.push_str(run);
                last = Some(piece);
            }
            Piece::ToEndOfLine(run) => {
                separate(&mut out, last, depth, Separation::Spaced);
                out.push_str(run);
                out.push('\n');
                last = None;
            }
        }
    }
    // A body that ended mid-line still ends with one.
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

enum Separation {
    /// A bracket opening: it may follow a colon or a name with no space.
    SameLine,
    /// A run of text: two of them in a row need a space between.
    Spaced,
}

fn separate(out: &mut String, last: Option<&Piece>, depth: usize, separation: Separation) {
    match last {
        // Nothing written on this line yet, so the line begins here.
        None => indent(out, depth),
        // A colon has already written the space after itself.
        Some(Piece::Colon) => {}
        Some(Piece::Whole(_)) | Some(Piece::Close(_)) => {
            if matches!(separation, Separation::Spaced) {
                out.push(' ');
            }
        }
        // Both write their own newline, so nothing follows them on a line.
        Some(Piece::Open(_)) | Some(Piece::Comma) | Some(Piece::ToEndOfLine(_)) => {}
    }
}

fn indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str(INDENT);
    }
}

/// Splits a bracketed body into the pieces the layout moves around.
///
/// Strings and comments come out whole, which is what keeps a brace inside a
/// string from being read as a brace. An unterminated one takes the rest of
/// the text with it rather than being abandoned -- the reader is still typing
/// it, and the characters after it are theirs.
fn pieces_of(text: &str) -> Vec<Piece<'_>> {
    let bytes = text.as_bytes();
    let mut pieces = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let byte = bytes[at];
        match byte {
            b' ' | b'\t' | b'\r' | b'\n' => {
                at += 1;
            }
            b'{' | b'[' | b'(' => {
                pieces.push(Piece::Open(byte as char));
                at += 1;
            }
            b'}' | b']' | b')' => {
                pieces.push(Piece::Close(byte as char));
                at += 1;
            }
            b',' => {
                pieces.push(Piece::Comma);
                at += 1;
            }
            b':' => {
                pieces.push(Piece::Colon);
                at += 1;
            }
            b'"' | b'\'' | b'`' => {
                let ends = end_of_string(bytes, at, byte);
                pieces.push(Piece::Whole(&text[at..ends]));
                at = ends;
            }
            b'/' if bytes.get(at + 1) == Some(&b'/') => {
                let ends = bytes[at..]
                    .iter()
                    .position(|byte| *byte == b'\n')
                    .map_or(bytes.len(), |found| at + found);
                pieces.push(Piece::ToEndOfLine(text[at..ends].trim_end()));
                at = ends;
            }
            b'/' if bytes.get(at + 1) == Some(&b'*') => {
                let ends = find_from(bytes, at + 2, b"*/").map_or(bytes.len(), |found| found + 2);
                pieces.push(Piece::Whole(&text[at..ends]));
                at = ends;
            }
            _ => {
                let starts = at;
                while at < bytes.len() && !is_punctuation(bytes[at]) {
                    at += 1;
                }
                // A character this does not know is still a character: it goes
                // out as its own piece rather than being dropped.
                if at == starts {
                    at += next_character(text, starts);
                }
                pieces.push(Piece::Whole(text[starts..at].trim_end()));
            }
        }
    }
    pieces
}

fn is_punctuation(byte: u8) -> bool {
    matches!(
        byte,
        b' ' | b'\t'
            | b'\r'
            | b'\n'
            | b'{'
            | b'}'
            | b'['
            | b']'
            | b'('
            | b')'
            | b','
            | b':'
            | b'"'
            | b'\''
            | b'`'
    )
}

fn next_character(text: &str, at: usize) -> usize {
    text[at..].chars().next().map_or(1, char::len_utf8)
}

/// Where the string opened at `at` ends, past its closing quote, or the end of
/// the text where it never closes.
fn end_of_string(bytes: &[u8], at: usize, quote: u8) -> usize {
    let mut walking = at + 1;
    while walking < bytes.len() {
        match bytes[walking] {
            b'\\' => walking += 2,
            byte if byte == quote => return walking + 1,
            _ => walking += 1,
        }
    }
    bytes.len()
}

fn find_from(bytes: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if from >= bytes.len() {
        return None;
    }
    bytes[from..]
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|found| from + found)
}

/// One piece of a tagged body, and where it was written.
///
/// The range is what lets a whole element be written out again exactly as it
/// was, which is the only honest way to lay out markup whose text is content.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Tagged<'a> {
    at: Range<usize>,
    kind: TagKind<'a>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TagKind<'a> {
    /// An element that opens, with the name it opens.
    Opening(&'a str),
    /// An element that closes, with the name it closes.
    Closing(&'a str),
    /// A self-closing tag, a comment, a doctype, a processing instruction, a
    /// CDATA section, a void element -- written as it is, nesting nothing.
    /// Named where it is an element, so that an inline one is known as such.
    Standalone(Option<&'a str>),
    /// Everything between two tags, as it was written, whitespace and all.
    Text,
}

fn by_tags(text: &str, text_is_content: bool) -> String {
    let pieces = tagged_pieces(text, text_is_content);
    let mut out = String::with_capacity(text.len() + text.len() / 4);
    let mut depth: usize = 0;
    let mut at = 0;
    while at < pieces.len() {
        match &pieces[at].kind {
            TagKind::Opening(name) => {
                // Where the text between tags is what the reader sees, an
                // element holding any of it goes out exactly as it came in.
                // Breaking `<b>a</b><i>b</i>` over two lines puts a space
                // between two words that had none, and closing `<a> x </a>`
                // up takes one away -- both change what the markup says.
                if text_is_content
                    && let Some(whole) = an_element_holding_content(text, &pieces, at)
                {
                    indent(&mut out, depth);
                    out.push_str(&text[pieces[at].at.start..pieces[whole].at.end]);
                    out.push('\n');
                    at = whole + 1;
                    continue;
                }
                // One line of text between a tag and the tag that closes it
                // stays on that line: three lines for `<title>Report</title>`
                // is not a tidier body.
                if let Some(closing) = closed_right_after(&pieces, at, name) {
                    indent(&mut out, depth);
                    out.push_str(&text[pieces[at].at.start..pieces[closing].at.end]);
                    out.push('\n');
                    at = closing + 1;
                    continue;
                }
                indent(&mut out, depth);
                out.push_str(&text[pieces[at].at.clone()]);
                out.push('\n');
                depth += 1;
                at += 1;
            }
            TagKind::Closing(_) => {
                depth = depth.saturating_sub(1);
                indent(&mut out, depth);
                out.push_str(&text[pieces[at].at.clone()]);
                out.push('\n');
                at += 1;
            }
            TagKind::Standalone(_) => {
                indent(&mut out, depth);
                out.push_str(&text[pieces[at].at.clone()]);
                out.push('\n');
                at += 1;
            }
            TagKind::Text => {
                let written = text[pieces[at].at.clone()].trim();
                if !written.is_empty() {
                    indent(&mut out, depth);
                    out.push_str(written);
                    out.push('\n');
                }
                at += 1;
            }
        }
    }
    out
}

/// The piece closing the element opened at `at`, when there is nothing
/// between them, or one line of text, and the names agree.
///
/// The names have to agree: `<a>x</b>` is two different elements and laying it
/// out as one would indent everything after it wrongly.
fn closed_right_after(pieces: &[Tagged<'_>], at: usize, name: &str) -> Option<usize> {
    // An element closed immediately, holding nothing at all.
    if let Some(TagKind::Closing(closes)) = pieces.get(at + 1).map(|piece| &piece.kind)
        && closes.eq_ignore_ascii_case(name)
    {
        return Some(at + 1);
    }
    let inside = pieces.get(at + 1)?;
    let closing = pieces.get(at + 2)?;
    let TagKind::Closing(closes) = closing.kind else {
        return None;
    };
    if !closes.eq_ignore_ascii_case(name) || !matches!(inside.kind, TagKind::Text) {
        return None;
    }
    Some(at + 2)
}

/// The piece closing the element opened at `at`, when that element holds
/// content of its own -- words, or an element written in the run of them.
///
/// Whitespace between two block elements is layout and may be moved; a word,
/// or the space beside an `<em>`, is what the reader sees. An element holding
/// either goes out exactly as it came in rather than being opened out.
///
/// An element nobody closed holds nothing this can write out whole, so it lays
/// out like any other: as far as it goes.
fn an_element_holding_content(text: &str, pieces: &[Tagged<'_>], at: usize) -> Option<usize> {
    let TagKind::Opening(name) = pieces[at].kind else {
        return None;
    };
    let mut depth = 0usize;
    let mut holds_content = false;
    for (walked, piece) in pieces.iter().enumerate().skip(at + 1) {
        match &piece.kind {
            TagKind::Opening(inner) => {
                if depth == 0 && is_inline(inner) {
                    holds_content = true;
                }
                depth += 1;
            }
            TagKind::Closing(closes) => {
                if depth == 0 {
                    return (closes.eq_ignore_ascii_case(name) && holds_content).then_some(walked);
                }
                depth -= 1;
            }
            TagKind::Standalone(inner) => {
                if depth == 0 && inner.is_some_and(is_inline) {
                    holds_content = true;
                }
            }
            TagKind::Text => {
                if depth == 0 && !text[piece.at.clone()].trim().is_empty() {
                    holds_content = true;
                }
            }
        }
    }
    None
}

/// Elements written in the run of the words around them, where the whitespace
/// beside them is part of what the reader sees.
const INLINE: &[&str] = &[
    "a", "abbr", "b", "bdi", "bdo", "br", "button", "cite", "code", "data", "dfn", "em", "i",
    "img", "input", "kbd", "label", "mark", "q", "s", "samp", "select", "small", "span", "strong",
    "sub", "sup", "textarea", "time", "u", "var", "wbr",
];

fn is_inline(name: &str) -> bool {
    INLINE.contains(&name.to_ascii_lowercase().as_str())
}

/// Elements the markup closes for you, so what follows them is not inside
/// them. Every one of these is an HTML element with no closing tag; XML has
/// none of its own, and an XML document using these names still nests them
/// with a closing tag, which this reads and honours.
const CLOSED_ALREADY: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

/// Elements whose content is not markup. A `<` inside a script is a
/// comparison, and reading it as a tag turns the rest of the document into
/// nonsense.
const NOT_MARKUP_INSIDE: &[&str] = &["script", "style", "textarea", "title"];

fn tagged_pieces(text: &str, text_is_content: bool) -> Vec<Tagged<'_>> {
    let bytes = text.as_bytes();
    let mut pieces = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'<' {
            let ends = end_of_tag(bytes, at);
            let tag = &text[at..ends];
            let kind = kind_of_tag(tag);
            let opened = match kind {
                TagKind::Opening(name) if text_is_content => Some(name),
                _ => None,
            };
            pieces.push(Tagged { at: at..ends, kind });
            at = ends;
            // The content of a script or a style is not markup, so it is taken
            // whole, up to the tag that closes it.
            if let Some(name) = opened
                && NOT_MARKUP_INSIDE.contains(&name.to_ascii_lowercase().as_str())
            {
                let closes = closing_tag_of(text, at, name).unwrap_or(bytes.len());
                if closes > at {
                    pieces.push(Tagged {
                        at: at..closes,
                        kind: TagKind::Text,
                    });
                }
                at = closes;
            }
            continue;
        }
        let starts = at;
        while at < bytes.len() && bytes[at] != b'<' {
            at += 1;
        }
        if at > starts {
            pieces.push(Tagged {
                at: starts..at,
                kind: TagKind::Text,
            });
        }
    }
    pieces
}

/// Where `</name` is written next, from `from`, whatever case it is in.
fn closing_tag_of(text: &str, from: usize, name: &str) -> Option<usize> {
    let looking_for = format!("</{}", name.to_ascii_lowercase());
    let rest = text.get(from..)?.to_ascii_lowercase();
    rest.find(&looking_for).map(|found| from + found)
}

fn kind_of_tag(tag: &str) -> TagKind<'_> {
    let inside = tag.trim_start_matches('<').trim_end_matches('>');
    if tag.starts_with("<!") || tag.starts_with("<?") {
        return TagKind::Standalone(None);
    }
    if inside.ends_with('/') {
        return TagKind::Standalone(Some(name_of(inside)));
    }
    if let Some(closes) = inside.strip_prefix('/') {
        return TagKind::Closing(name_of(closes));
    }
    let name = name_of(inside);
    if CLOSED_ALREADY.contains(&name.to_ascii_lowercase().as_str()) {
        return TagKind::Standalone(Some(name));
    }
    TagKind::Opening(name)
}

fn name_of(inside: &str) -> &str {
    let inside = inside.trim_start();
    let ends = inside
        .find(|character: char| character.is_whitespace() || character == '/')
        .unwrap_or(inside.len());
    &inside[..ends]
}

/// Where the tag opened at `at` ends, past its `>`, or the end of the text
/// where it never closes -- a tag still being typed is still the reader's.
fn end_of_tag(bytes: &[u8], at: usize) -> usize {
    if bytes[at..].starts_with(b"<!--") {
        return find_from(bytes, at + 4, b"-->").map_or(bytes.len(), |found| found + 3);
    }
    if bytes[at..].starts_with(b"<![CDATA[") {
        return find_from(bytes, at + 9, b"]]>").map_or(bytes.len(), |found| found + 3);
    }
    let mut walking = at + 1;
    let mut quote: Option<u8> = None;
    while walking < bytes.len() {
        match (quote, bytes[walking]) {
            (Some(open), byte) if byte == open => quote = None,
            (Some(_), _) => {}
            (None, byte @ (b'"' | b'\'')) => quote = Some(byte),
            (None, b'>') => return walking + 1,
            (None, _) => {}
        }
        walking += 1;
    }
    bytes.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The promise the whole thing rests on: whitespace moves, nothing else.
    fn nothing_but_whitespace_moved(before: &str, after: &str) {
        let stripped =
            |text: &str| -> String { text.chars().filter(|c| !c.is_whitespace()).collect() };
        assert_eq!(
            stripped(before),
            stripped(after),
            "laying a body out again must not change a single character of it"
        );
    }

    fn laid_out(text: &str, shape: Shape) -> String {
        let out = laid_out_again(text, shape);
        nothing_but_whitespace_moved(text, &out);
        out
    }

    #[test]
    fn a_whole_json_body_is_laid_out() {
        let out = laid_out(r#"{"a":1,"b":[1,2],"c":{"d":"e"}}"#, Shape::Brackets);
        assert_eq!(
            out,
            "{\n  \"a\": 1,\n  \"b\": [\n    1,\n    2\n  ],\n  \"c\": {\n    \"d\": \"e\"\n  }\n}"
        );
    }

    /// The reason this exists at all: the reader is halfway through typing.
    #[test]
    fn a_json_body_missing_its_closing_brace_is_still_laid_out() {
        let out = laid_out(r#"{"a":1,"b":2"#, Shape::Brackets);
        assert_eq!(out, "{\n  \"a\": 1,\n  \"b\": 2");
    }

    #[test]
    fn a_closing_brace_with_nothing_open_is_kept() {
        let out = laid_out("}{\"a\":1}", Shape::Brackets);
        assert!(
            out.starts_with('}'),
            "the stray brace is still there: {out:?}"
        );
    }

    #[test]
    fn a_brace_inside_a_string_is_not_a_brace() {
        let out = laid_out(r#"{"a":"{not a brace}"}"#, Shape::Brackets);
        assert_eq!(out, "{\n  \"a\": \"{not a brace}\"\n}");
    }

    #[test]
    fn a_string_the_reader_has_not_closed_takes_the_rest_with_it() {
        let out = laid_out(r#"{"a":"unclosed, {"#, Shape::Brackets);
        assert_eq!(out, "{\n  \"a\": \"unclosed, {");
    }

    #[test]
    fn an_escaped_quote_does_not_end_a_string() {
        let out = laid_out(r#"{"a":"say \"hi\", now"}"#, Shape::Brackets);
        assert_eq!(out, "{\n  \"a\": \"say \\\"hi\\\", now\"\n}");
    }

    #[test]
    fn an_empty_container_stays_on_its_own_line() {
        let out = laid_out(r#"{"a":{},"b":[]}"#, Shape::Brackets);
        assert_eq!(out, "{\n  \"a\": {},\n  \"b\": []\n}");
    }

    #[test]
    fn a_number_is_not_rewritten() {
        let out = laid_out(
            r#"{"big":12345678901234567890,"e":1e3,"z":1.50}"#,
            Shape::Brackets,
        );
        assert!(
            out.contains("12345678901234567890") && out.contains("1e3") && out.contains("1.50"),
            "a request body is sent to somebody else's server as the reader spelled it: {out:?}"
        );
    }

    #[test]
    fn a_body_already_laid_out_is_left_as_it_is() {
        let once = laid_out(r#"{"a":1,"b":[1,2]}"#, Shape::Brackets);
        let twice = laid_out(&once, Shape::Brackets);
        assert_eq!(
            once, twice,
            "laying out a laid-out body must change nothing"
        );
    }

    /// A token written after a line comment is a token inside it. Putting the
    /// next one on that line would comment out the body from there on, which
    /// is a formatter changing what the body says.
    #[test]
    fn nothing_follows_a_line_comment_onto_its_line() {
        let out = laid_out(
            "{ // a note about a, with a } in it\n\"a\":1, /* and {another} */ \"b\":2 }",
            Shape::Brackets,
        );
        assert_eq!(
            out,
            "{\n  // a note about a, with a } in it\n  \"a\": 1,\n  /* and {another} */ \"b\": 2\n}"
        );
    }

    #[test]
    fn a_body_with_comments_is_laid_out_the_same_way_twice() {
        let body = "{ // one\n\"a\":1, // two\n\"b\":2 }";
        let once = laid_out(body, Shape::Brackets);
        assert_eq!(laid_out(&once, Shape::Brackets), once);
    }

    #[test]
    fn a_line_comment_at_the_very_end_ends_the_body() {
        let out = laid_out("{\"a\":1} // trailing", Shape::Brackets);
        assert_eq!(out, "{\n  \"a\": 1\n} // trailing");
    }

    #[test]
    fn an_empty_body_lays_out_as_nothing() {
        assert_eq!(laid_out("", Shape::Brackets), "");
        assert_eq!(laid_out("   \n  ", Shape::Brackets), "");
        assert_eq!(laid_out("", Shape::Xml), "");
    }

    #[test]
    fn text_outside_any_bracket_is_kept() {
        let out = laid_out("not json at all", Shape::Brackets);
        assert_eq!(out, "not json at all");
    }

    #[test]
    fn a_whole_xml_body_is_laid_out() {
        let out = laid_out("<a><b>one</b><c><d/></c></a>", Shape::Xml);
        assert_eq!(out, "<a>\n  <b>one</b>\n  <c>\n    <d/>\n  </c>\n</a>");
    }

    #[test]
    fn an_xml_body_missing_its_closing_tag_is_still_laid_out() {
        let out = laid_out("<a><b>one</b>", Shape::Xml);
        assert_eq!(out, "<a>\n  <b>one</b>");
    }

    #[test]
    fn a_tag_the_reader_has_not_closed_takes_the_rest_with_it() {
        let out = laid_out("<a><b class=\"x\"", Shape::Xml);
        assert_eq!(out, "<a>\n  <b class=\"x\"");
    }

    #[test]
    fn an_angle_bracket_inside_an_attribute_does_not_end_the_tag() {
        let out = laid_out("<a title=\"1 > 0\"><b/></a>", Shape::Xml);
        assert_eq!(out, "<a title=\"1 > 0\">\n  <b/>\n</a>");
    }

    #[test]
    fn a_comment_and_a_doctype_nest_nothing() {
        let out = laid_out(
            "<!DOCTYPE html><!-- a </note> --><html><body/></html>",
            Shape::Xml,
        );
        assert_eq!(
            out,
            "<!DOCTYPE html>\n<!-- a </note> -->\n<html>\n  <body/>\n</html>"
        );
    }

    #[test]
    fn a_void_element_nests_nothing() {
        let out = laid_out("<p>a<br>b<img src=\"x\">c</p>", Shape::Xml);
        assert_eq!(out, "<p>\n  a\n  <br>\n  b\n  <img src=\"x\">\n  c\n</p>");
    }

    /// Codex found this one: pairing an opening tag with whatever closing tag
    /// came third indents everything after a mismatched pair wrongly.
    #[test]
    fn a_closing_tag_for_another_element_does_not_close_this_one() {
        let out = laid_out("<a>x</b><c><d/></c>", Shape::Xml);
        assert_eq!(out, "<a>\n  x\n</b>\n<c>\n  <d/>\n</c>");
    }

    #[test]
    fn a_name_is_read_without_its_attributes_or_its_slash() {
        let out = laid_out("<a href=\"x\">one</a>", Shape::Xml);
        assert_eq!(out, "<a href=\"x\">one</a>");
    }

    /// In HTML the text between tags is what the reader sees. Breaking
    /// `<b>a</b><i>b</i>` over two lines puts a space between two words that
    /// had none; closing `<a> x </a>` up takes one away. Neither is
    /// formatting.
    #[test]
    fn an_html_element_holding_text_is_written_out_exactly_as_it_was() {
        let out = laid_out("<div><p><b>a</b><i>b</i></p><hr></div>", Shape::Html);
        assert_eq!(out, "<div>\n  <p><b>a</b><i>b</i></p>\n  <hr>\n</div>");
    }

    #[test]
    fn html_whitespace_around_text_is_left_where_it_was() {
        let out = laid_out("<div><a> x </a></div>", Shape::Html);
        assert_eq!(
            out, "<div><a> x </a></div>",
            "`a` is written in the run of the words around it, so the line is left alone"
        );
    }

    #[test]
    fn an_element_closed_at_once_stays_on_its_line() {
        let out = laid_out("<a><b></b><c/></a>", Shape::Xml);
        assert_eq!(out, "<a>\n  <b></b>\n  <c/>\n</a>");
    }

    #[test]
    fn whitespace_between_two_block_elements_is_layout() {
        let out = laid_out("<div> <section></section> <hr> </div>", Shape::Html);
        assert_eq!(out, "<div>\n  <section></section>\n  <hr>\n</div>");
    }

    #[test]
    fn an_html_container_of_elements_is_still_laid_out() {
        let out = laid_out("<div><section><hr></section></div>", Shape::Html);
        assert_eq!(out, "<div>\n  <section>\n    <hr>\n  </section>\n</div>");
    }

    /// Codex found this one too: a `<` inside a script is a comparison, and
    /// reading it as a tag turns the rest of the document into nonsense.
    #[test]
    fn a_less_than_inside_a_script_is_not_a_tag() {
        let out = laid_out(
            "<body><script>if (a < b) { x() }</script><hr></body>",
            Shape::Html,
        );
        assert_eq!(
            out,
            "<body>\n  <script>if (a < b) { x() }</script>\n  <hr>\n</body>"
        );
    }

    #[test]
    fn a_style_block_is_not_read_as_markup() {
        let out = laid_out(
            "<head><style>a > b { color: red }</style></head>",
            Shape::Html,
        );
        assert_eq!(
            out,
            "<head>\n  <style>a > b { color: red }</style>\n</head>"
        );
    }

    #[test]
    fn an_html_body_already_laid_out_is_left_as_it_is() {
        let once = laid_out("<div><p><b>a</b>text</p><hr></div>", Shape::Html);
        assert_eq!(laid_out(&once, Shape::Html), once);
    }

    #[test]
    fn an_unclosed_html_element_lays_out_as_far_as_it_goes() {
        let out = laid_out("<div><p>a", Shape::Html);
        assert_eq!(out, "<div>\n  <p>\n    a");
    }

    #[test]
    fn cdata_is_kept_whole() {
        let out = laid_out("<a><![CDATA[ <b> not a tag </b> ]]></a>", Shape::Xml);
        assert_eq!(out, "<a>\n  <![CDATA[ <b> not a tag </b> ]]>\n</a>");
    }

    #[test]
    fn a_tagged_body_already_laid_out_is_left_as_it_is() {
        let once = laid_out("<a><b>one</b><c><d/></c></a>", Shape::Xml);
        let twice = laid_out(&once, Shape::Xml);
        assert_eq!(once, twice);
    }

    #[test]
    fn the_shape_follows_the_content_type() {
        assert_eq!(shape_of(RawBodyContentType::Json), Some(Shape::Brackets));
        assert_eq!(
            shape_of(RawBodyContentType::JavaScript),
            Some(Shape::Brackets)
        );
        assert_eq!(shape_of(RawBodyContentType::Xml), Some(Shape::Xml));
        assert_eq!(shape_of(RawBodyContentType::Html), Some(Shape::Html));
        assert_eq!(
            shape_of(RawBodyContentType::Text),
            None,
            "plain text has no structure to lay out"
        );
    }

    #[test]
    fn the_header_says_which_shape_and_nothing_else_does() {
        assert_eq!(shape_named_by("application/json"), Some(Shape::Brackets));
        assert_eq!(
            shape_named_by("text/html; charset=utf-8"),
            Some(Shape::Html)
        );
        assert_eq!(shape_named_by("application/xml"), Some(Shape::Xml));
        assert_eq!(
            shape_named_by("text/plain"),
            None,
            "prose that happens to open with a brace is still prose"
        );
        assert_eq!(shape_named_by(""), None);
    }
}
