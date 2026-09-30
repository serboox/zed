use std::ops::Range;

use sqlparser::ast::Statement;
use sqlparser::dialect::{
    BigQueryDialect, ClickHouseDialect, Dialect, HiveDialect, MySqlDialect, SnowflakeDialect,
};
use sqlparser::parser::{Parser, ParserError};
use sqlparser::tokenizer::{Location, Token, Tokenizer};

use crate::sql_binder::offset_for_location;

/// A statement longer than this is not read for implied boundaries: every cut
/// reads the rest of it again, so the work grows with the square of its length.
const MAX_SEGMENT_BYTES_READ_FOR_BOUNDARIES: usize = 256 * 1024;

/// The words a statement can begin with. A line that begins with one of them,
/// where the statement before it has stopped, is the start of the next
/// statement. Words that also continue a statement from a new line (`DESC`
/// after `ORDER BY a`, `LOCK IN SHARE MODE`, a routine's `BEGIN`) are left out.
const STATEMENT_KEYWORDS: &[&str] = &[
    "SELECT", "WITH", "INSERT", "REPLACE", "UPDATE", "DELETE", "MERGE", "CREATE", "ALTER", "DROP",
    "TRUNCATE", "RENAME", "SHOW", "DESCRIBE", "EXPLAIN", "SET", "USE", "CALL", "GRANT", "REVOKE",
    "START", "COMMIT", "ROLLBACK", "VALUES", "PRAGMA", "ANALYZE", "OPTIMIZE", "VACUUM", "LOAD",
    "COPY",
];

/// The separators of `text` with the ones the grammar implies added to the `;`
/// that are written: a statement that has no `;` ends where the next one
/// begins, which the parser knows and a search for `;` cannot.
///
/// A separator is the offset of the byte before a statement begins, the way a
/// `;` is the offset of the byte after a statement ends, so everything that
/// reads separators reads these the same way. An implied one is always a line
/// break, which is ASCII and so never inside a multi-byte character.
/// `dialect` is `None` for a language with no SQL grammar, where only what is
/// written counts.
pub(crate) fn separators_with_implied(
    text: &str,
    semicolons: &[usize],
    dialect: Option<&dyn Dialect>,
) -> Vec<usize> {
    let Some(dialect) = dialect else {
        return semicolons.to_vec();
    };
    let mut separators = Vec::with_capacity(semicolons.len());
    let mut begin = 0;
    for end in semicolons.iter().copied().chain([text.len()]) {
        if let Some(segment) = text.get(begin..end) {
            for line_start in implied_starts(segment, dialect) {
                separators.push(begin + line_start - 1);
            }
        }
        if end < text.len() {
            separators.push(end);
        }
        begin = end + 1;
    }
    separators
}

/// Whether `#` starts a comment in `dialect`; in PostgreSQL and SQLite it is an
/// operator or part of a name. With no dialect nothing is known, and `#` is
/// read as a comment, as it always was.
pub(crate) fn hash_starts_a_comment(dialect: Option<&dyn Dialect>) -> bool {
    dialect.is_none_or(|dialect| {
        dialect.is::<MySqlDialect>()
            || dialect.is::<ClickHouseDialect>()
            || dialect.is::<SnowflakeDialect>()
            || dialect.is::<BigQueryDialect>()
            || dialect.is::<HiveDialect>()
    })
}

/// The offsets, within `segment`, of the lines that start a statement of their
/// own although nothing written says so.
///
/// Each line that begins like a statement is asked, with the real grammar, in
/// order from the top:
/// - everything above it reads as one whole statement: it cannot be part of
///   it, so the line starts the next one (a word the grammar would take for an
///   alias, like the `DELETE` in `SELECT 2 DELETE FROM t`, changes nothing) --
///   except where the grammar lets the statement go on, as `CREATE TABLE t
///   (...)` does with a `SELECT`;
/// - what is above is unfinished: the line goes on with it when all the text
///   from the top reads as one statement (`INSERT INTO t` and the `SELECT`
///   below it, a `UNION`, a `WITH`, an `IN (`); when the grammar stops or
///   chokes on this very line, it starts the next statement, and the
///   unfinished one runs alone for the server to say what is wrong with it;
/// - anything else, a syntax the grammar does not know included: nothing is
///   cut, which is what was done before this existed.
fn implied_starts(segment: &str, dialect: &dyn Dialect) -> Vec<usize> {
    if segment.len() > MAX_SEGMENT_BYTES_READ_FOR_BOUNDARIES
        || !has_a_line_that_could_start_a_statement(segment, dialect)
    {
        return Vec::new();
    }
    let mut starts = Vec::new();
    let mut begin = 0;
    while let Some(line_start) = segment
        .get(begin..)
        .and_then(|rest| next_implied_start(rest, dialect))
    {
        begin += line_start;
        starts.push(begin);
    }
    starts
}

/// Whether any line after the first one with code on it begins with a word a
/// statement can begin with. Checked first, so that a statement that has no
/// such line is never tokenized.
fn has_a_line_that_could_start_a_statement(text: &str, dialect: &dyn Dialect) -> bool {
    let mut lines = text.split('\n');
    let has_code = lines
        .by_ref()
        .any(|line| !line.trim().is_empty() && !is_comment_only(line, dialect));
    has_code && lines.any(|line| starts_a_statement(line, dialect))
}

fn starts_a_statement(line: &str, dialect: &dyn Dialect) -> bool {
    let line = line.trim_start();
    let mut words = line
        .split(|character: char| !(character.is_alphanumeric() || character == '_'))
        .filter(|word| !word.is_empty());
    let Some(first) = words.next() else {
        return false;
    };
    if line
        .chars()
        .next()
        .is_some_and(|character| !character.is_alphabetic())
    {
        return false;
    }
    let first = first.to_ascii_uppercase();
    if !STATEMENT_KEYWORDS.contains(&first.as_str()) {
        return false;
    }
    let second = words.next().map(str::to_ascii_uppercase);
    match (first.as_str(), second.as_deref()) {
        // Index hints: `FROM t USE INDEX (i)`.
        ("USE", Some("INDEX" | "KEY")) => false,
        ("WITH", _) => begins_a_common_table_expression(line, dialect),
        _ => true,
    }
}

/// `WITH RECURSIVE`, `WITH name AS`, `WITH name (columns) AS`, as opposed to
/// `WITH ROLLUP`, `WITH CHECK OPTION`, `WITH GRANT OPTION` and the like.
fn begins_a_common_table_expression(line: &str, _dialect: &dyn Dialect) -> bool {
    let after_with = line.trim_start().get("WITH".len()..).unwrap_or_default();
    let rest = after_with.trim_start();
    let word_length = rest
        .find(|character: char| !(character.is_alphanumeric() || character == '_'))
        .unwrap_or(rest.len());
    let word = &rest[..word_length];
    if word.eq_ignore_ascii_case("RECURSIVE") {
        return true;
    }
    if word.is_empty() {
        return false;
    }
    let after_name = rest[word_length..].trim_start();
    after_name.starts_with('(')
        || after_name
            .get(..2)
            .is_some_and(|keyword| keyword.eq_ignore_ascii_case("AS"))
}

fn is_comment_only(line: &str, dialect: &dyn Dialect) -> bool {
    let line = line.trim();
    line.starts_with("--")
        || (line.starts_with('#') && hash_starts_a_comment(Some(dialect)))
        || (line.starts_with("/*") && line.ends_with("*/"))
}

fn next_implied_start(text: &str, dialect: &dyn Dialect) -> Option<usize> {
    let stopped_at = where_a_statement_stops(text, dialect)
        .and_then(|location| offset_for_location(text, location));
    let mut line_start = 0;
    for line in text.split_inclusive('\n') {
        let this_line_start = line_start;
        line_start += line.len();
        if this_line_start == 0 || !starts_a_statement(line, dialect) {
            continue;
        }
        let head = text.get(..this_line_start)?;
        let cuts_here = match read_whole(head, dialect) {
            // Everything above is a statement already, so this line cannot be
            // part of it -- unless the grammar lets it go on with this word.
            Read::Statement(statement) => !goes_on_with(&statement, line),
            Read::Unfinished => match stopped_at {
                Some(at) if (this_line_start..line_start).contains(&at) => {
                    first_word_of_line_at(text, this_line_start) == Some(at)
                        || text
                            .get(this_line_start..)
                            .is_some_and(|tail| reads_as_a_statement(tail, dialect))
                }
                _ => false,
            },
        };
        if cuts_here {
            let with_its_comments = start_of_comments_above(text, this_line_start, dialect);
            return (with_its_comments > 0).then_some(with_its_comments);
        }
    }
    None
}

/// Where the statement read from the start of `text` stopped -- where the
/// grammar ended it, or where it could not go on. `None` when it ran to the
/// end, or when it could not start, or when nothing is known.
fn where_a_statement_stops(text: &str, dialect: &dyn Dialect) -> Option<Location> {
    let tokens = Tokenizer::new(dialect, text)
        .tokenize_with_location()
        .ok()?;
    let mut parser = Parser::new(dialect).with_tokens_with_locations(tokens);
    let first = parser.peek_token().span.start;
    let stopped_at = match parser.parse_statement() {
        Ok(_) => {
            let next = parser.peek_token();
            if next.token == Token::EOF {
                return None;
            }
            next.span.start
        }
        Err(error) => location_in(&error)?,
    };
    (stopped_at != first).then_some(stopped_at)
}

enum Read {
    Statement(Statement),
    Unfinished,
}

/// Whether all of `text` is one statement.
fn read_whole(text: &str, dialect: &dyn Dialect) -> Read {
    let Ok(tokens) = Tokenizer::new(dialect, text).tokenize_with_location() else {
        return Read::Unfinished;
    };
    let mut parser = Parser::new(dialect).with_tokens_with_locations(tokens);
    match parser.parse_statement() {
        Ok(statement) if parser.peek_token().token == Token::EOF => Read::Statement(statement),
        _ => Read::Unfinished,
    }
}

/// MySQL and others let `CREATE TABLE t (...)` go on with a `SELECT` of its
/// own, with no `AS` between them.
fn goes_on_with(statement: &Statement, line: &str) -> bool {
    let first_word = line
        .trim_start()
        .split(|character: char| !character.is_alphanumeric())
        .next()
        .unwrap_or_default();
    matches!(statement, Statement::CreateTable(_))
        && ["SELECT", "WITH", "VALUES"]
            .iter()
            .any(|keyword| keyword.eq_ignore_ascii_case(first_word))
}

fn reads_as_a_statement(text: &str, dialect: &dyn Dialect) -> bool {
    let Ok(tokens) = Tokenizer::new(dialect, text).tokenize_with_location() else {
        return false;
    };
    Parser::new(dialect)
        .with_tokens_with_locations(tokens)
        .parse_statement()
        .is_ok()
}

/// The offset of the first thing on the line that starts at `line_start`.
fn first_word_of_line_at(text: &str, line_start: usize) -> Option<usize> {
    let line = text.get(line_start..)?;
    let indent = line.len() - line.trim_start_matches([' ', '\t', '\r']).len();
    Some(line_start + indent)
}

/// A comment that sits right above a statement, with no blank line between,
/// belongs to that statement, the way it does after a `;`.
fn start_of_comments_above(text: &str, line_start: usize, dialect: &dyn Dialect) -> usize {
    let mut start = line_start;
    while start > 0 {
        let Some(above) = text.get(..start - 1) else {
            break;
        };
        let above_start = above.rfind('\n').map_or(0, |index| index + 1);
        let Some(line) = text.get(above_start..start - 1) else {
            break;
        };
        if !is_comment_only(line, dialect) {
            break;
        }
        start = above_start;
    }
    start
}

/// Where the parser says it gave up. The parser puts it in the message, after
/// `at Line: `, and nowhere else; a test holds the format, so that a newer
/// parser that changes it fails loudly instead of quietly cutting nothing.
fn location_in(error: &ParserError) -> Option<Location> {
    let (ParserError::ParserError(message) | ParserError::TokenizerError(message)) = error else {
        return None;
    };
    let tail = message.rsplit(" at Line: ").next()?;
    let (line, column) = tail.split_once(", Column: ")?;
    let column: String = column.chars().take_while(char::is_ascii_digit).collect();
    Some(Location::new(
        line.trim().parse().ok()?,
        column.parse().ok()?,
    ))
}

/// The byte ranges of the statements of `text`, the text between one separator
/// and the next, separators left out. The last range runs to the end of the
/// text, and a range can be blank.
///
/// For what is shown while the text is edited: after a quote that never closes
/// the `;` it swallowed end statements again, so that one malformed statement
/// cannot merge every statement after it into a blob. Nothing runs from these.
pub(crate) fn statement_spans(text: &str, dialect: Option<&dyn Dialect>) -> Vec<Range<usize>> {
    let mut spans = Vec::new();
    let mut start = 0;
    for separator in separators(text, dialect, true) {
        spans.push(start..separator);
        start = separator + 1;
    }
    if start < text.len() {
        spans.push(start..text.len());
    }
    spans
}

/// Every separator of `text`: the `;` that are written, and the line breaks the
/// grammar says end a statement that has no `;`.
///
/// For what runs: a quote that never closes swallows the `;` after it, and they
/// are not boundaries, so that a dangling quote cannot turn the text after it
/// into statements of their own that would be sent to the server.
pub(crate) fn statement_separators(text: &str, dialect: Option<&dyn Dialect>) -> Vec<usize> {
    separators(text, dialect, false)
}

fn separators(
    text: &str,
    dialect: Option<&dyn Dialect>,
    recover_after_unterminated_quote: bool,
) -> Vec<usize> {
    let (mut semicolons, ends_inside_a_quote) =
        unquoted_semicolon_offsets(text, hash_starts_a_comment(dialect));
    if recover_after_unterminated_quote && ends_inside_a_quote {
        semicolons = semicolons_read_without_quotes(text, semicolons);
    }
    separators_with_implied(text, &semicolons, dialect)
}

// Byte offsets of every top-level `;` in `text`, and whether the text ends
// inside a quote that never closed -- i.e. semicolons that are
// NOT inside a string literal, a quoted identifier, or a comment. This is a
// small SQL tokenizer (not a parser): it walks the text tracking which
// construct it is inside so a `;` that is part of a value, an identifier, or a
// comment is never mistaken for a statement boundary. Recognized constructs:
//   * `'...'` / `"..."` string literals, honoring both the SQL doubled-quote
//     escape (`''`/`""`) and a backslash escape (`\'`), so a `;` embedded in a
//     value (e.g. a PHP-serialized string like `a:9:{i:60;i:1;...}`) does not
//     split.
//   * `` `...` `` quoted identifiers, honoring the doubled-backtick escape
//     (`` `` ``); there is no backslash escape inside backtick identifiers.
//   * `--` line comments, and `#` ones when `hash_comments` is set (skipped to
//     end of line), and `/* ... */` block comments (skipped to the matching
//     `*/`). MySQL block comments do not nest, and `/*! ... */` conditional
//     comments share the same delimiters, so both skip identically here.
// The `--` case follows MySQL's rule that the second dash must be followed by
// whitespace, a control char, or end-of-input, so an operator like `5--3`
// stays an expression.
// Operating on bytes is safe: every delimiter checked (`'`, `"`, `;`, `` ` ``,
// `\`, `-`, `#`, `/`, `*`, `\n`) is ASCII, so it can never appear as part of a
// multi-byte UTF-8 continuation sequence.
fn unquoted_semicolon_offsets(text: &str, hash_comments: bool) -> (Vec<usize>, bool) {
    enum State {
        Normal,
        // Inside a `'`- or `"`-quoted string; holds the closing quote byte.
        String(u8),
        Identifier,
        LineComment,
        BlockComment,
    }
    let bytes = text.as_bytes();
    let mut offsets = Vec::new();
    let mut state = State::Normal;
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        match state {
            State::Normal => match byte {
                b'\'' | b'"' => {
                    state = State::String(byte);
                    index += 1;
                }
                b'`' => {
                    state = State::Identifier;
                    index += 1;
                }
                b'#' if hash_comments => {
                    state = State::LineComment;
                    index += 1;
                }
                b'-' if bytes.get(index + 1) == Some(&b'-')
                    && bytes.get(index + 2).is_none_or(|&b| b <= b' ') =>
                {
                    state = State::LineComment;
                    index += 2;
                }
                b'/' if bytes.get(index + 1) == Some(&b'*') => {
                    state = State::BlockComment;
                    index += 2;
                }
                b';' => {
                    offsets.push(index);
                    index += 1;
                }
                _ => index += 1,
            },
            State::String(quote) => {
                if byte == b'\\' && index + 1 < bytes.len() {
                    index += 2;
                } else if byte == quote {
                    if bytes.get(index + 1) == Some(&quote) {
                        index += 2;
                    } else {
                        state = State::Normal;
                        index += 1;
                    }
                } else {
                    index += 1;
                }
            }
            State::Identifier => {
                if byte == b'`' {
                    if bytes.get(index + 1) == Some(&b'`') {
                        index += 2;
                    } else {
                        state = State::Normal;
                        index += 1;
                    }
                } else {
                    index += 1;
                }
            }
            State::LineComment => {
                if byte == b'\n' {
                    state = State::Normal;
                }
                index += 1;
            }
            State::BlockComment => {
                if byte == b'*' && bytes.get(index + 1) == Some(&b'/') {
                    state = State::Normal;
                    index += 2;
                } else {
                    index += 1;
                }
            }
        }
    }
    (
        offsets,
        matches!(state, State::String(_) | State::Identifier),
    )
}

// The `;` after the last boundary that a quote which never closed swallowed,
// added back, read without regard for quotes.
fn semicolons_read_without_quotes(text: &str, mut offsets: Vec<usize>) -> Vec<usize> {
    let from = offsets.last().map_or(0, |offset| offset + 1);
    offsets.extend(
        text.bytes()
            .enumerate()
            .skip(from)
            .filter(|(_, byte)| *byte == b';')
            .map(|(index, _)| index),
    );
    offsets
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::dialect::PostgreSqlDialect;

    fn cuts(text: &str) -> Vec<usize> {
        implied_starts(text, &MySqlDialect {})
    }

    fn line_of(text: &str, offset: usize) -> usize {
        text[..offset].matches('\n').count()
    }

    #[test]
    fn the_parser_puts_where_it_gave_up_in_the_message_in_the_form_the_cut_reads() {
        let dialect = MySqlDialect {};
        let tokens = Tokenizer::new(&dialect, "INSERT INTO t\nSHOW TABLES")
            .tokenize_with_location()
            .expect("tokens");
        let mut parser = Parser::new(&dialect).with_tokens_with_locations(tokens);
        let error = parser
            .parse_statement()
            .expect_err("an insert with nothing to insert");
        let location = location_in(&error).expect("a place in the message");
        assert_eq!((location.line, location.column), (2, 1), "{error}");
    }

    #[test]
    fn a_statement_without_a_semicolon_ends_where_the_next_one_begins() {
        let text = "SELECT * FROM b\nSELECT * FROM c";
        let starts = cuts(text);
        assert_eq!(starts.len(), 1);
        assert_eq!(line_of(text, starts[0]), 1);
        assert_eq!(&text[starts[0]..], "SELECT * FROM c");
    }

    #[test]
    fn blank_lines_between_them_do_not_matter() {
        let text = "SELECT * FROM b\n\n\nSELECT * FROM c\n\nDELETE FROM d WHERE x = 1";
        let starts = cuts(text);
        assert_eq!(starts.len(), 2, "{starts:?}");
        assert_eq!(&text[starts[0]..starts[0] + 6], "SELECT");
        assert_eq!(&text[starts[1]..starts[1] + 6], "DELETE");
    }

    #[test]
    fn what_a_statement_continues_with_is_not_another_statement() {
        for text in [
            "INSERT INTO t\nSELECT 1",
            "INSERT INTO t (a)\nSELECT a FROM u",
            "SELECT 1\nUNION ALL\nSELECT 2",
            "SELECT a FROM t WHERE a IN (\nSELECT b FROM u\n)",
            "WITH x AS (SELECT 1)\nSELECT * FROM x",
            "CREATE VIEW v AS\nSELECT 1",
            "EXPLAIN\nSELECT 1",
            "UPDATE t\nSET a = 1\nWHERE b = 2",
            "SELECT a,\n\n       b\nFROM t",
            "ALTER TABLE t\nDROP COLUMN x",
            "SELECT a FROM t\nGROUP BY a\nWITH ROLLUP",
            "SELECT a FROM t\nORDER BY a\nDESC",
            "SELECT a FROM t\nUSE INDEX (i)\nWHERE a = 1",
            "SELECT * FROM t\nLOCK IN SHARE MODE",
        ] {
            assert!(cuts(text).is_empty(), "{text:?} is one statement");
        }
    }

    #[test]
    fn an_unfinished_statement_above_a_finished_one_is_cut_off_from_it() {
        let text = "SELECT * FROM\n\nSELECT * FROM c";
        let starts = cuts(text);
        assert_eq!(starts.len(), 1, "{starts:?}");
        assert_eq!(&text[starts[0]..], "SELECT * FROM c");
    }

    #[test]
    fn an_insert_with_nothing_to_insert_is_cut_off_from_the_statement_below_it() {
        let text = "INSERT INTO t\n\nSHOW TABLES";
        let starts = cuts(text);
        assert_eq!(starts.len(), 1, "{starts:?}");
        assert_eq!(&text[starts[0]..], "SHOW TABLES");
    }

    #[test]
    fn a_word_the_grammar_takes_for_an_alias_does_not_hide_the_next_statement() {
        let text = "SELECT a FROM t\nSHOW TABLES";
        let starts = cuts(text);
        assert_eq!(starts.len(), 1, "{starts:?}");
        assert_eq!(&text[starts[0]..], "SHOW TABLES");
    }

    #[test]
    fn a_statement_word_the_grammar_takes_for_an_alias_still_starts_a_statement() {
        let text = "SELECT 2\nDELETE FROM t WHERE a = 1";
        let starts = cuts(text);
        assert_eq!(starts.len(), 1, "{starts:?}");
        assert_eq!(&text[starts[0]..], "DELETE FROM t WHERE a = 1");
    }

    #[test]
    fn a_continuation_does_not_hide_the_statement_after_it() {
        let text = "INSERT INTO t\nSELECT a\nFROM u\nWHERE a IN (\nSELECT 1\n)\nDELETE FROM x";
        let starts = cuts(text);
        assert_eq!(starts.len(), 1, "{starts:?}");
        assert_eq!(&text[starts[0]..], "DELETE FROM x");
    }

    #[test]
    fn a_chain_of_statements_is_cut_at_each_of_them() {
        let text = "SELECT 1\nSELECT 2\nSELECT 3";
        assert_eq!(cuts(text).len(), 2);
    }

    #[test]
    fn a_comment_right_above_a_statement_goes_with_it() {
        let text = "SELECT 1\n-- the second\n# another\nSELECT 2";
        let starts = cuts(text);
        assert_eq!(starts.len(), 1, "{starts:?}");
        assert_eq!(&text[starts[0]..], "-- the second\n# another\nSELECT 2");
        let text = "SELECT 1\n-- about nothing\n\nSELECT 2";
        let starts = cuts(text);
        assert_eq!(&text[starts[0]..], "SELECT 2");
    }

    #[test]
    fn a_table_created_from_a_select_below_it_is_one_statement() {
        for text in [
            "CREATE TABLE t (a INT)\nSELECT 1 AS a",
            "CREATE TABLE t\nSELECT * FROM u",
            "CREATE TABLE t (a INT)\nAS\nSELECT 1",
        ] {
            assert!(cuts(text).is_empty(), "{text:?} is one statement");
        }
    }

    #[test]
    fn syntax_the_grammar_does_not_know_is_left_alone() {
        let text = "SELECT x\nFROM t\nQUALIFY row_number() OVER () = 1";
        assert!(cuts(text).is_empty());
        let text = "FROBNICATE the_thing\nSELECT 1";
        assert!(
            cuts(text).is_empty(),
            "nothing is known about the first line"
        );
    }

    #[test]
    fn two_statements_on_one_line_are_left_alone() {
        assert!(cuts("SELECT 1 SELECT 2").is_empty());
    }

    #[test]
    fn a_line_that_is_only_a_word_inside_a_string_or_comment_is_not_a_statement() {
        let text = "SELECT 'a\nSELECT b' AS x\n-- SELECT 2\nFROM t";
        assert!(cuts(text).is_empty(), "{:?}", cuts(text));
    }

    #[test]
    fn a_separator_stands_on_the_line_break_before_the_statement() {
        let text = "SELECT 1;\nSELECT 2\nSELECT 3;\nSELECT 4";
        let semicolons = [
            text.find(';').expect("the first"),
            text.rfind(';').expect("the second"),
        ];
        let separators = separators_with_implied(text, &semicolons, Some(&MySqlDialect {}));
        assert_eq!(separators.len(), 3, "{separators:?}");
        for separator in &separators {
            assert!(matches!(text.as_bytes()[*separator], b';' | b'\n'));
        }
        assert!(separators.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn statement_spans_splits_multiple_statements_on_semicolon() {
        let text = "SELECT 1; SELECT 2;";
        let spans = statement_spans(text, Some(&MySqlDialect {}));
        assert_eq!(spans.len(), 2);
        assert_eq!(&text[spans[0].clone()], "SELECT 1");
        assert_eq!(&text[spans[1].clone()], " SELECT 2");
    }

    #[test]
    fn statement_spans_ignores_semicolon_inside_string_literal() {
        let text = "SELECT 'a;b' FROM t;";
        let spans = statement_spans(text, Some(&MySqlDialect {}));
        assert_eq!(spans.len(), 1);
        assert_eq!(&text[spans[0].clone()], "SELECT 'a;b' FROM t");
    }

    #[test]
    fn statement_spans_ignores_semicolon_inside_backtick_identifier() {
        let text = "SELECT `weird;name` FROM t;";
        let spans = statement_spans(text, Some(&MySqlDialect {}));
        assert_eq!(spans.len(), 1);
        assert_eq!(&text[spans[0].clone()], "SELECT `weird;name` FROM t");
    }

    #[test]
    fn statement_spans_ignores_semicolon_inside_double_quoted_identifier() {
        let text = "SELECT \"weird;name\" FROM t;";
        let spans = statement_spans(text, Some(&MySqlDialect {}));
        assert_eq!(spans.len(), 1);
        assert_eq!(&text[spans[0].clone()], "SELECT \"weird;name\" FROM t");
    }

    #[test]
    fn statement_spans_ignores_semicolon_inside_line_comment() {
        let text = "SELECT 1 -- trailing ; comment\nFROM t;";
        let spans = statement_spans(text, Some(&MySqlDialect {}));
        assert_eq!(spans.len(), 1);
    }

    #[test]
    fn statement_spans_keeps_trailing_statement_without_terminator() {
        let text = "SELECT 1; SELECT 2";
        let spans = statement_spans(text, Some(&MySqlDialect {}));
        assert_eq!(spans.len(), 2);
        assert_eq!(&text[spans[1].clone()], " SELECT 2");
    }

    #[test]
    fn statement_spans_ignores_semicolon_inside_hash_line_comment() {
        let text = "SELECT 1 # trailing ; comment\nFROM t;";
        let spans = statement_spans(text, Some(&MySqlDialect {}));
        assert_eq!(spans.len(), 1);
    }

    #[test]
    fn statement_spans_double_dash_without_trailing_space_is_not_a_comment() {
        // MySQL requires whitespace after `--`; `1--2` is arithmetic, not a
        // comment, so the following ';' still terminates the statement.
        let text = "SELECT 1--2; SELECT 3;";
        let spans = statement_spans(text, Some(&MySqlDialect {}));
        assert_eq!(spans.len(), 2);
    }

    #[test]
    fn statement_spans_recovers_boundary_after_unterminated_quote() {
        // An unterminated single quote must not swallow every following
        // statement into one blob; a trailing well-formed statement keeps its
        // own span. (Fails pre-recovery: yields a single span.)
        let text = "SELECT 'oops; SELECT 1;";
        let spans = statement_spans(text, Some(&MySqlDialect {}));
        assert_eq!(spans.len(), 2);
        assert_eq!(&text[spans[1].clone()], " SELECT 1");
    }

    #[test]
    fn a_hash_is_an_operator_not_a_comment_in_postgresql() {
        let text = "SELECT a #> '{b}' FROM t; SELECT 2;";
        let spans = statement_spans(text, Some(&PostgreSqlDialect {}));
        assert_eq!(spans.len(), 2, "{spans:?}");
        assert_eq!(&text[spans[1].clone()], " SELECT 2");
        assert_eq!(statement_spans(text, Some(&MySqlDialect {})).len(), 1);
    }

    #[test]
    fn statements_without_a_semicolon_are_spans_of_their_own() {
        let text = "SELECT 1\nSELECT 2;\nSELECT 3";
        let spans = statement_spans(text, Some(&MySqlDialect {}));
        let texts: Vec<&str> = spans.iter().map(|span| text[span.clone()].trim()).collect();
        assert_eq!(texts, ["SELECT 1", "SELECT 2", "SELECT 3"]);
        let spans = statement_spans(text, None);
        assert_eq!(spans.len(), 2, "no grammar, only what is written counts");
    }

    #[test]
    fn no_grammar_means_only_what_is_written_counts() {
        let text = "SELECT 1\nSELECT 2";
        assert!(separators_with_implied(text, &[], None).is_empty());
    }

    #[test]
    fn hash_is_a_comment_only_where_the_dialect_says_so() {
        assert!(hash_starts_a_comment(Some(&MySqlDialect {})));
        assert!(!hash_starts_a_comment(Some(&PostgreSqlDialect {})));
        assert!(hash_starts_a_comment(None));
        let text = "SELECT 1\n# the second\nSELECT 2";
        assert!(implied_starts(text, &PostgreSqlDialect {}).len() <= 1);
    }
}
