use std::collections::HashSet;
use std::ops::Range;
use std::sync::OnceLock;

use db_client::DatabaseDriver;

use crate::sql_ast::{dialect_for_driver, statement_spans};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum SqlTokenKind {
    Keyword,
    DataType,
    Function,
    String,
    Number,
    Comment,
    Operator,
    Identifier,
    Variable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SqlHighlightToken {
    pub range: Range<usize>,
    pub kind: SqlTokenKind,
}

/// Tokenizes SQL for syntax coloring, one statement at a time.
///
/// Each statement is tokenized over its own byte slice, so a malformed
/// statement (for example a pasted PHP-interpolated string with unbalanced
/// quotes) can only mis-color itself: no token range can extend past its
/// statement, and a later well-formed statement is always colored correctly.
/// This is the resilience the console needs, because tree-sitter would instead
/// paint the whole rest of the buffer as one string once a quote goes unclosed.
pub(crate) fn highlight_tokens(text: &str, driver: DatabaseDriver) -> Vec<SqlHighlightToken> {
    if dialect_for_driver(driver).is_none() {
        return Vec::new();
    }
    let mut tokens = Vec::new();
    for span in statement_spans(text) {
        tokenize_statement(&text[span.clone()], span.start, &mut tokens);
    }
    tokens
}

fn tokenize_statement(statement: &str, base: usize, out: &mut Vec<SqlHighlightToken>) {
    let bytes = statement.as_bytes();
    let len = bytes.len();
    let mut index = 0usize;

    while index < len {
        let byte = bytes[index];

        if byte.is_ascii_whitespace() {
            index += 1;
            continue;
        }

        if byte == b'#' {
            let start = index;
            while index < len && bytes[index] != b'\n' {
                index += 1;
            }
            push(out, base, start, index, SqlTokenKind::Comment);
            continue;
        }

        if byte == b'-'
            && bytes.get(index + 1) == Some(&b'-')
            && bytes.get(index + 2).is_none_or(|&b| b <= b' ')
        {
            let start = index;
            while index < len && bytes[index] != b'\n' {
                index += 1;
            }
            push(out, base, start, index, SqlTokenKind::Comment);
            continue;
        }

        if byte == b'/' && bytes.get(index + 1) == Some(&b'*') {
            let start = index;
            index += 2;
            while index < len && !(bytes[index] == b'*' && bytes.get(index + 1) == Some(&b'/')) {
                index += 1;
            }
            index = (index + 2).min(len);
            push(out, base, start, index, SqlTokenKind::Comment);
            continue;
        }

        if byte == b'\'' || byte == b'"' {
            let quote = byte;
            let start = index;
            index += 1;
            while index < len {
                let current = bytes[index];
                if current == b'\\' && index + 1 < len {
                    index += 2;
                    continue;
                }
                if current == quote {
                    if bytes.get(index + 1) == Some(&quote) {
                        index += 2;
                        continue;
                    }
                    index += 1;
                    break;
                }
                index += 1;
            }
            push(out, base, start, index, SqlTokenKind::String);
            continue;
        }

        if byte == b'`' {
            let start = index;
            index += 1;
            while index < len {
                if bytes[index] == b'`' {
                    if bytes.get(index + 1) == Some(&b'`') {
                        index += 2;
                        continue;
                    }
                    index += 1;
                    break;
                }
                index += 1;
            }
            push(out, base, start, index, SqlTokenKind::Identifier);
            continue;
        }

        if byte == b'@' || byte == b':' || byte == b'$' {
            let start = index;
            index += 1;
            while index < len && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_') {
                index += 1;
            }
            push(out, base, start, index, SqlTokenKind::Variable);
            continue;
        }

        if byte == b'?' {
            push(out, base, index, index + 1, SqlTokenKind::Variable);
            index += 1;
            continue;
        }

        if byte.is_ascii_digit() {
            let start = index;
            index += 1;
            while index < len && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'.') {
                index += 1;
            }
            push(out, base, start, index, SqlTokenKind::Number);
            continue;
        }

        if byte.is_ascii_alphabetic() || byte == b'_' || byte >= 0x80 {
            let start = index;
            index += 1;
            while index < len
                && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_' || bytes[index] >= 0x80)
            {
                index += 1;
            }
            let word = &statement[start..index];
            let kind = classify_word(word, bytes, index, len);
            push(out, base, start, index, kind);
            continue;
        }

        push(out, base, index, index + 1, SqlTokenKind::Operator);
        index += 1;
    }
}

fn classify_word(word: &str, bytes: &[u8], after: usize, len: usize) -> SqlTokenKind {
    let upper = word.to_ascii_uppercase();
    if is_data_type(&upper) {
        return SqlTokenKind::DataType;
    }
    if is_keyword(&upper) {
        return SqlTokenKind::Keyword;
    }
    let mut probe = after;
    while probe < len && bytes[probe].is_ascii_whitespace() {
        probe += 1;
    }
    if bytes.get(probe) == Some(&b'(') {
        return SqlTokenKind::Function;
    }
    SqlTokenKind::Identifier
}

fn is_keyword(upper: &str) -> bool {
    static KEYWORDS: OnceLock<HashSet<&'static str>> = OnceLock::new();
    KEYWORDS
        .get_or_init(|| {
            // A curated set of reserved words worth coloring as keywords.
            // sqlparser's ALL_KEYWORDS is deliberately not used: it also lists
            // common column names (ID, NAME, VALUE, ...) and aggregate
            // functions (COUNT, SUM, ...), which should stay identifiers or be
            // classified as functions by the `(` lookahead instead.
            [
                "SELECT", "FROM", "WHERE", "GROUP", "BY", "HAVING", "ORDER", "LIMIT",
                "OFFSET", "FETCH", "DISTINCT", "ALL", "AS", "WITH", "RECURSIVE", "UNION",
                "INTERSECT", "EXCEPT", "MINUS", "RETURNING", "QUALIFY", "WINDOW", "JOIN",
                "INNER", "LEFT", "RIGHT", "FULL", "OUTER", "CROSS", "NATURAL", "ON",
                "USING", "LATERAL", "AND", "OR", "NOT", "IN", "IS", "LIKE", "ILIKE",
                "RLIKE", "REGEXP", "BETWEEN", "EXISTS", "ANY", "SOME", "NULL", "TRUE",
                "FALSE", "UNKNOWN", "CASE", "WHEN", "THEN", "ELSE", "END", "IF", "ASC",
                "DESC", "NULLS", "INSERT", "INTO", "VALUES", "UPDATE", "SET", "DELETE",
                "TRUNCATE", "MERGE", "REPLACE", "UPSERT", "CREATE", "ALTER", "DROP",
                "RENAME", "TABLE", "VIEW", "INDEX", "DATABASE", "SCHEMA", "TRIGGER",
                "PROCEDURE", "FUNCTION", "SEQUENCE", "TEMPORARY", "COLUMN", "ADD",
                "MODIFY", "CHANGE", "CONSTRAINT", "PRIMARY", "FOREIGN", "KEY", "UNIQUE",
                "REFERENCES", "DEFAULT", "CHECK", "AUTO_INCREMENT", "IDENTITY",
                "GENERATED", "CASCADE", "RESTRICT", "ENGINE", "CHARSET", "COLLATE",
                "UNSIGNED", "ZEROFILL", "AFTER", "BEGIN", "START", "COMMIT", "ROLLBACK",
                "TRANSACTION", "SAVEPOINT", "LOCK", "UNLOCK", "GRANT", "REVOKE", "TO",
                "PRIVILEGES", "EXPLAIN", "ANALYZE", "DESCRIBE", "SHOW", "USE", "OVER",
                "PARTITION", "ROWS", "RANGE", "UNBOUNDED", "PRECEDING", "FOLLOWING",
            ]
            .into_iter()
            .collect()
        })
        .contains(upper)
}

fn is_data_type(upper: &str) -> bool {
    matches!(
        upper,
        "INT" | "INTEGER"
            | "TINYINT"
            | "SMALLINT"
            | "MEDIUMINT"
            | "BIGINT"
            | "DECIMAL"
            | "NUMERIC"
            | "FLOAT"
            | "DOUBLE"
            | "REAL"
            | "BIT"
            | "BOOL"
            | "BOOLEAN"
            | "CHAR"
            | "VARCHAR"
            | "BINARY"
            | "VARBINARY"
            | "TEXT"
            | "TINYTEXT"
            | "MEDIUMTEXT"
            | "LONGTEXT"
            | "BLOB"
            | "TINYBLOB"
            | "MEDIUMBLOB"
            | "LONGBLOB"
            | "ENUM"
            | "DATE"
            | "DATETIME"
            | "TIMESTAMP"
            | "TIME"
            | "YEAR"
            | "JSON"
            | "JSONB"
            | "UUID"
            | "SERIAL"
            | "BYTEA"
            | "MONEY"
    )
}

fn push(out: &mut Vec<SqlHighlightToken>, base: usize, start: usize, end: usize, kind: SqlTokenKind) {
    if end > start {
        out.push(SqlHighlightToken {
            range: (base + start)..(base + end),
            kind,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find_token<'a>(
        tokens: &'a [SqlHighlightToken],
        text: &str,
        value: &str,
        kind: SqlTokenKind,
    ) -> Option<&'a SqlHighlightToken> {
        tokens
            .iter()
            .find(|token| token.kind == kind && &text[token.range.clone()] == value)
    }

    #[test]
    fn tokenizes_basic_select() {
        let text = "SELECT id FROM users;";
        let tokens = highlight_tokens(text, DatabaseDriver::MySQL);
        assert!(find_token(&tokens, text, "SELECT", SqlTokenKind::Keyword).is_some());
        assert!(find_token(&tokens, text, "FROM", SqlTokenKind::Keyword).is_some());
        assert!(find_token(&tokens, text, "id", SqlTokenKind::Identifier).is_some());
        assert!(find_token(&tokens, text, "users", SqlTokenKind::Identifier).is_some());
    }

    #[test]
    fn classifies_types_functions_numbers_strings_and_comments() {
        let text = "-- note\nSELECT COUNT(*), CAST(x AS INT), 42, 'hi' FROM t;";
        let tokens = highlight_tokens(text, DatabaseDriver::MySQL);
        assert!(find_token(&tokens, text, "-- note", SqlTokenKind::Comment).is_some());
        assert!(find_token(&tokens, text, "COUNT", SqlTokenKind::Function).is_some());
        assert!(find_token(&tokens, text, "INT", SqlTokenKind::DataType).is_some());
        assert!(find_token(&tokens, text, "42", SqlTokenKind::Number).is_some());
        assert!(find_token(&tokens, text, "'hi'", SqlTokenKind::String).is_some());
    }

    #[test]
    fn returns_empty_for_non_sql_driver() {
        assert!(highlight_tokens("GET foo", DatabaseDriver::Redis).is_empty());
    }

    #[test]
    fn malformed_php_statement_does_not_bleed_into_later_statements() {
        // Middle statement is a pasted PHP-interpolated WHERE clause with
        // unbalanced quotes. The final clean statement must still tokenize
        // correctly, and no token may cross into it. A naive whole-buffer
        // tokenize would swallow everything after the first stray quote into
        // one string, mis-coloring the final SELECT.
        let text = concat!(
            "SELECT 1;\n",
            r"SELECT * FROM t WHERE row_ID =\''.$content_res['row_ID'].'\' AND os=\'android\';';",
            "\n",
            "SELECT id FROM users;",
        );
        let tokens = highlight_tokens(text, DatabaseDriver::MySQL);

        let final_start = text.rfind("SELECT id").expect("final statement present");
        assert!(
            tokens.iter().any(|token| token.kind == SqlTokenKind::Keyword
                && token.range.start == final_start),
            "final SELECT must be a keyword token starting at the final statement"
        );
        assert!(
            find_token(&tokens, text, "users", SqlTokenKind::Identifier).is_some(),
            "final statement's `users` must be an identifier"
        );
        assert!(
            tokens
                .iter()
                .all(|token| !(token.range.start < final_start && token.range.end > final_start)),
            "no token may straddle the final statement boundary (no bleed)"
        );
    }

    #[test]
    fn unterminated_quote_does_not_bleed_into_the_next_statement() {
        // Genuine fail-then-pass: statement 1 has an unclosed single quote, so
        // a naive whole-buffer tokenize paints everything from that quote to
        // EOF as one string and swallows the final statement. Per-statement
        // tokenization plus statement_spans' unterminated-quote recovery keeps
        // the final statement's tokens correct and bounded.
        let text = "SELECT 'unclosed FROM t;\nSELECT id FROM users;";
        let tokens = highlight_tokens(text, DatabaseDriver::MySQL);

        let final_start = text.rfind("SELECT id").expect("final statement present");
        assert!(
            tokens
                .iter()
                .any(|token| token.kind == SqlTokenKind::Keyword
                    && token.range.start == final_start),
            "final SELECT must be a keyword token, not swallowed by the unclosed quote"
        );
        assert!(
            find_token(&tokens, text, "users", SqlTokenKind::Identifier).is_some(),
            "final statement's `users` must be an identifier"
        );
        assert!(
            tokens
                .iter()
                .all(|token| !(token.range.start < final_start && token.range.end > final_start)),
            "no token may straddle into the final statement"
        );
    }
}
