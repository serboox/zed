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
/// Redis has no SQL grammar at all, so it deliberately has no dialect.
pub(crate) fn dialect_for_driver(driver: DatabaseDriver) -> Option<Box<dyn Dialect>> {
    match driver {
        DatabaseDriver::MySQL => Some(Box::new(MySqlDialect {})),
        DatabaseDriver::PostgreSQL => Some(Box::new(PostgreSqlDialect {})),
        DatabaseDriver::SQLite => Some(Box::new(SQLiteDialect {})),
        DatabaseDriver::ClickHouse => Some(Box::new(ClickHouseDialect {})),
        DatabaseDriver::Redis => None,
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
                b'-' if bytes.get(index + 1) == Some(&b'-') => {
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
        spans.push(start..bytes.len());
    }
    spans
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
    let dialect = dialect_for_driver(driver)?;
    let span = statement_span_at(text, cursor_offset)?;
    let statement_text = text.get(span)?.trim();
    if statement_text.is_empty() {
        return None;
    }
    let mut statements = Parser::parse_sql(dialect.as_ref(), statement_text).ok()?;
    if statements.len() != 1 {
        return None;
    }
    statements.pop()
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
    fn try_parse_statement_at_only_parses_the_statement_under_the_cursor() {
        let text = "SELECT 1; SELECT FROM WHERE;";
        let first = try_parse_statement_at(text, DatabaseDriver::MySQL, 0);
        assert!(first.is_some());
        let second_cursor = text.find("SELECT FROM").expect("second statement present");
        let second = try_parse_statement_at(text, DatabaseDriver::MySQL, second_cursor);
        assert!(second.is_none());
    }
}
