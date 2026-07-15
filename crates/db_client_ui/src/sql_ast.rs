#![allow(dead_code)]

use std::ops::Range;

use db_client::DatabaseDriver;
use sqlparser::ast::Statement;
use sqlparser::dialect::{
    ClickHouseDialect, Dialect, MySqlDialect, PostgreSqlDialect, SQLiteDialect,
};
use sqlparser::parser::Parser;

/// Returns the sqlparser dialect matching a connection's driver.
///
/// `GenericDialect` is used as a placeholder for any future driver that gains
/// its own `DatabaseDriver` variant before a matching sqlparser dialect exists.
/// Redis, MongoDB, Cassandra, and Aerospike have no SQL grammar at all, so
/// they deliberately have no dialect (MongoDB's console accepts its own tiny
/// SELECT/INSERT/UPDATE/DELETE subset — see `db_client::mongo_provider` —
/// Cassandra's CQL — see `db_client::cassandra_provider` — neither of
/// which is SQL, so neither is parsed by sqlparser; Aerospike has no query
/// language at all and is accessed through its own Get/Put/Scan form).
pub(crate) fn dialect_for_driver(driver: DatabaseDriver) -> Option<Box<dyn Dialect>> {
    match driver {
        DatabaseDriver::MySQL => Some(Box::new(MySqlDialect {})),
        DatabaseDriver::PostgreSQL => Some(Box::new(PostgreSqlDialect {})),
        DatabaseDriver::SQLite => Some(Box::new(SQLiteDialect {})),
        DatabaseDriver::ClickHouse => Some(Box::new(ClickHouseDialect {})),
        DatabaseDriver::Redis => None,
        DatabaseDriver::MongoDB => None,
        DatabaseDriver::Cassandra => None,
        DatabaseDriver::Aerospike => None,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScanState {
    Normal,
    SingleQuoted,
    DoubleQuoted,
    Backtick,
    LineComment,
    BlockComment,
}

/// Splits SQL text into top-level statement byte-ranges, split on `;`.
///
/// Unlike a naive `str::find(';')`, this tracks quoting/comment state so a
/// semicolon inside a string literal, a quoted identifier, or a comment does
/// not end the statement early. Doubled quotes (`''`, `""`, `` `` ``) and
/// backslash escapes inside a quoted section are treated as part of the
/// quoted content, not as closing it.
pub(crate) fn statement_spans(text: &str) -> Vec<Range<usize>> {
    let bytes = text.as_bytes();
    let mut spans = Vec::new();
    let mut state = ScanState::Normal;
    let mut start = 0usize;
    let mut index = 0usize;

    while index < bytes.len() {
        let byte = bytes[index];
        match state {
            ScanState::Normal => match byte {
                b'\'' => state = ScanState::SingleQuoted,
                b'"' => state = ScanState::DoubleQuoted,
                b'`' => state = ScanState::Backtick,
                b'#' => state = ScanState::LineComment,
                b'-' if bytes.get(index + 1) == Some(&b'-')
                    && bytes.get(index + 2).is_none_or(|&b| b <= b' ') =>
                {
                    state = ScanState::LineComment;
                    index += 1;
                }
                b'/' if bytes.get(index + 1) == Some(&b'*') => {
                    state = ScanState::BlockComment;
                    index += 1;
                }
                b';' => {
                    spans.push(start..index);
                    start = index + 1;
                }
                _ => {}
            },
            ScanState::SingleQuoted | ScanState::DoubleQuoted | ScanState::Backtick => {
                let quote = match state {
                    ScanState::SingleQuoted => b'\'',
                    ScanState::DoubleQuoted => b'"',
                    _ => b'`',
                };
                if byte == b'\\' && state != ScanState::Backtick {
                    // Backslash-escapes the next byte in MySQL-style strings; skip it
                    // so an escaped quote character is not mistaken for the closer.
                    index += 1;
                } else if byte == quote {
                    if bytes.get(index + 1) == Some(&quote) {
                        // Doubled quote is an escaped literal quote, not a closer.
                        index += 1;
                    } else {
                        state = ScanState::Normal;
                    }
                }
            }
            ScanState::LineComment => {
                if byte == b'\n' {
                    state = ScanState::Normal;
                }
            }
            ScanState::BlockComment => {
                if byte == b'*' && bytes.get(index + 1) == Some(&b'/') {
                    state = ScanState::Normal;
                    index += 1;
                }
            }
        }
        index += 1;
    }

    if start < bytes.len() {
        if matches!(
            state,
            ScanState::SingleQuoted | ScanState::DoubleQuoted | ScanState::Backtick
        ) {
            // Unterminated quote: the scanner never returned to Normal, so any
            // ';' it swallowed after `start` may really be a statement
            // boundary. Re-split the tail on ';' so one malformed statement
            // cannot merge every following statement into a single blob.
            let mut sub_start = start;
            let mut sub_index = start;
            while sub_index < bytes.len() {
                if bytes[sub_index] == b';' {
                    spans.push(sub_start..sub_index);
                    sub_start = sub_index + 1;
                }
                sub_index += 1;
            }
            if sub_start < bytes.len() {
                spans.push(sub_start..bytes.len());
            }
        } else {
            spans.push(start..bytes.len());
        }
    }
    spans
}

/// Pretty-prints every statement in `text` (keyword case, indentation, line
/// breaks), preserving statement boundaries. Returns `None` — leaving the
/// buffer untouched — if the driver has no SQL dialect, `text` has no
/// non-empty statements, or ANY statement fails to parse: a console buffer is
/// edited live and is often mid-edit or intentionally incomplete, so a
/// formatter that "does its best" on unparseable input risks silently
/// mangling text the user hasn't finished typing yet.
pub(crate) fn format_sql(text: &str, driver: DatabaseDriver) -> Option<String> {
    let dialect = dialect_for_driver(driver)?;
    let format_dialect = match driver {
        DatabaseDriver::PostgreSQL => sqlformat::Dialect::PostgreSql,
        _ => sqlformat::Dialect::Generic,
    };
    let options = sqlformat::FormatOptions {
        uppercase: Some(true),
        dialect: format_dialect,
        ..Default::default()
    };

    let mut formatted_statements = Vec::new();
    for span in statement_spans(text) {
        let trimmed = text[span].trim();
        if trimmed.is_empty() {
            continue;
        }
        Parser::parse_sql(dialect.as_ref(), trimmed).ok()?;
        formatted_statements.push(sqlformat::format(
            trimmed,
            &sqlformat::QueryParams::None,
            &options,
        ));
    }

    if formatted_statements.is_empty() {
        return None;
    }
    Some(format!("{};\n", formatted_statements.join(";\n\n")))
}

/// Finds the byte-range of the statement containing `offset`, per [`statement_spans`].
pub(crate) fn statement_span_at(text: &str, offset: usize) -> Option<Range<usize>> {
    let offset = offset.min(text.len());
    statement_spans(text)
        .into_iter()
        .find(|span| span.contains(&offset) || span.end == offset)
}

/// Parses the single SQL statement containing `cursor_offset`, or `None` if the
/// driver has no SQL dialect, the statement is incomplete/malformed, or the
/// span does not resolve to exactly one statement. `None` is the signal for
/// callers to fall back to the heuristic offset-based scanner.
pub(crate) fn try_parse_statement_at(
    text: &str,
    driver: DatabaseDriver,
    cursor_offset: usize,
) -> Option<Statement> {
    try_parse_statement_at_with_span(text, driver, cursor_offset).map(|(statement, _)| statement)
}

/// Same as [`try_parse_statement_at`], but also returns the exact byte range
/// within `text` that was parsed (the statement's span with surrounding
/// whitespace trimmed off) -- callers that need to map a location inside the
/// returned AST back to an absolute offset in `text` need this trimmed range,
/// since the AST's own spans are relative to it, not to the untrimmed span.
pub(crate) fn try_parse_statement_at_with_span(
    text: &str,
    driver: DatabaseDriver,
    cursor_offset: usize,
) -> Option<(Statement, Range<usize>)> {
    let dialect = dialect_for_driver(driver)?;
    let span = statement_span_at(text, cursor_offset)?;
    let raw = text.get(span.clone())?;
    let leading = raw.len() - raw.trim_start().len();
    let statement_text = raw.trim();
    if statement_text.is_empty() {
        return None;
    }
    let trimmed_span = (span.start + leading)..(span.start + leading + statement_text.len());
    let mut statements = Parser::parse_sql(dialect.as_ref(), statement_text).ok()?;
    if statements.len() != 1 {
        return None;
    }
    statements.pop().map(|statement| (statement, trimmed_span))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dialect_is<D: Dialect + 'static>(driver: DatabaseDriver) -> bool {
        dialect_for_driver(driver)
            .map(|dialect| (*dialect).type_id() == std::any::TypeId::of::<D>())
            .unwrap_or(false)
    }

    #[test]
    fn dialect_for_driver_maps_each_sql_driver_to_its_own_dialect() {
        assert!(dialect_is::<MySqlDialect>(DatabaseDriver::MySQL));
        assert!(dialect_is::<PostgreSqlDialect>(DatabaseDriver::PostgreSQL));
        assert!(dialect_is::<SQLiteDialect>(DatabaseDriver::SQLite));
        assert!(dialect_is::<ClickHouseDialect>(DatabaseDriver::ClickHouse));
    }

    #[test]
    fn dialect_for_driver_returns_none_for_redis() {
        assert!(dialect_for_driver(DatabaseDriver::Redis).is_none());
    }

    #[test]
    fn statement_spans_splits_multiple_statements_on_semicolon() {
        let text = "SELECT 1; SELECT 2;";
        let spans = statement_spans(text);
        assert_eq!(spans.len(), 2);
        assert_eq!(&text[spans[0].clone()], "SELECT 1");
        assert_eq!(&text[spans[1].clone()], " SELECT 2");
    }

    #[test]
    fn statement_spans_ignores_semicolon_inside_string_literal() {
        let text = "SELECT 'a;b' FROM t;";
        let spans = statement_spans(text);
        assert_eq!(spans.len(), 1);
        assert_eq!(&text[spans[0].clone()], "SELECT 'a;b' FROM t");
    }

    #[test]
    fn statement_spans_ignores_semicolon_inside_backtick_identifier() {
        let text = "SELECT `weird;name` FROM t;";
        let spans = statement_spans(text);
        assert_eq!(spans.len(), 1);
        assert_eq!(&text[spans[0].clone()], "SELECT `weird;name` FROM t");
    }

    #[test]
    fn statement_spans_ignores_semicolon_inside_double_quoted_identifier() {
        let text = "SELECT \"weird;name\" FROM t;";
        let spans = statement_spans(text);
        assert_eq!(spans.len(), 1);
        assert_eq!(&text[spans[0].clone()], "SELECT \"weird;name\" FROM t");
    }

    #[test]
    fn statement_spans_ignores_semicolon_inside_line_comment() {
        let text = "SELECT 1 -- trailing ; comment\nFROM t;";
        let spans = statement_spans(text);
        assert_eq!(spans.len(), 1);
    }

    #[test]
    fn statement_spans_keeps_trailing_statement_without_terminator() {
        let text = "SELECT 1; SELECT 2";
        let spans = statement_spans(text);
        assert_eq!(spans.len(), 2);
        assert_eq!(&text[spans[1].clone()], " SELECT 2");
    }

    #[test]
    fn statement_spans_ignores_semicolon_inside_hash_line_comment() {
        let text = "SELECT 1 # trailing ; comment\nFROM t;";
        let spans = statement_spans(text);
        assert_eq!(spans.len(), 1);
    }

    #[test]
    fn statement_spans_double_dash_without_trailing_space_is_not_a_comment() {
        // MySQL requires whitespace after `--`; `1--2` is arithmetic, not a
        // comment, so the following ';' still terminates the statement.
        let text = "SELECT 1--2; SELECT 3;";
        let spans = statement_spans(text);
        assert_eq!(spans.len(), 2);
    }

    #[test]
    fn statement_spans_recovers_boundary_after_unterminated_quote() {
        // An unterminated single quote must not swallow every following
        // statement into one blob; a trailing well-formed statement keeps its
        // own span. (Fails pre-recovery: yields a single span.)
        let text = "SELECT 'oops; SELECT 1;";
        let spans = statement_spans(text);
        assert_eq!(spans.len(), 2);
        assert_eq!(&text[spans[1].clone()], " SELECT 1");
    }

    #[test]
    fn try_parse_statement_at_resolves_complex_nested_query() {
        let text = "SELECT qpa.pair_id, q1.lang_id \
             FROM (SELECT qdt.currency_ID, qdt.lang_id FROM instruments.qdt AS qdt \
                   WHERE qdt.currency_ID IN (SELECT id FROM instruments.ids)) q1 \
             JOIN instruments.qpa AS qpa ON q1.currency_ID = qpa.cur1 \
             WHERE qpa.pair_id IN (SELECT pair_id FROM instruments.pairs);";
        let cursor = text.find("q1.lang_id").expect("cursor token present");
        let statement = try_parse_statement_at(text, DatabaseDriver::MySQL, cursor);
        assert!(statement.is_some());
    }

    #[test]
    fn try_parse_statement_at_returns_none_for_incomplete_statement() {
        let text = "SELECT s.op FROM";
        let statement = try_parse_statement_at(text, DatabaseDriver::MySQL, text.len());
        assert!(statement.is_none());
    }

    #[test]
    fn try_parse_statement_at_returns_none_for_malformed_statement() {
        let text = "SELECT FROM WHERE;";
        let statement = try_parse_statement_at(text, DatabaseDriver::MySQL, 0);
        assert!(statement.is_none());
    }

    #[test]
    fn try_parse_statement_at_returns_none_for_redis() {
        let text = "SELECT 1;";
        let statement = try_parse_statement_at(text, DatabaseDriver::Redis, 0);
        assert!(statement.is_none());
    }

    #[test]
    fn format_sql_uppercases_keywords_and_breaks_clauses_onto_their_own_lines() {
        let text = "select id, name from users where id = 1 order by name";
        let formatted = format_sql(text, DatabaseDriver::MySQL).expect("valid SQL formats");
        assert_eq!(
            formatted,
            "SELECT\n  id,\n  name\nFROM\n  users\nWHERE\n  id = 1\nORDER BY\n  name;\n"
        );
    }

    #[test]
    fn format_sql_formats_each_statement_independently_and_preserves_boundaries() {
        let text = "select 1; select 2";
        let formatted = format_sql(text, DatabaseDriver::MySQL).expect("valid SQL formats");
        assert_eq!(formatted, "SELECT\n  1;\n\nSELECT\n  2;\n");
    }

    #[test]
    fn format_sql_returns_none_and_does_not_touch_the_buffer_on_a_parse_failure() {
        let text = "select id, from where;";
        assert!(format_sql(text, DatabaseDriver::MySQL).is_none());
    }

    #[test]
    fn format_sql_returns_none_for_an_incomplete_statement_being_typed() {
        let text = "select id, name from";
        assert!(format_sql(text, DatabaseDriver::MySQL).is_none());
    }

    #[test]
    fn format_sql_returns_none_for_redis_which_has_no_sql_dialect() {
        let text = "GET foo";
        assert!(format_sql(text, DatabaseDriver::Redis).is_none());
    }

    #[test]
    fn format_sql_returns_none_for_empty_or_whitespace_only_text() {
        assert!(format_sql("", DatabaseDriver::MySQL).is_none());
        assert!(format_sql("   \n  ", DatabaseDriver::MySQL).is_none());
    }

    #[test]
    fn try_parse_statement_at_only_parses_the_statement_under_the_cursor() {
        let text = "SELECT 1; SELECT FROM WHERE;";
        let first = try_parse_statement_at(text, DatabaseDriver::MySQL, 0);
        assert!(first.is_some());
        let second_cursor = text.find("SELECT FROM").expect("second statement present");
        let second = try_parse_statement_at(text, DatabaseDriver::MySQL, second_cursor);
        assert!(second.is_none());
    }
}
