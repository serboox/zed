use std::path::Path;

use sqlparser::dialect::MySqlDialect;
use sqlparser::tokenizer::{Location, Token, TokenWithSpan, Tokenizer, Whitespace};

/// Looks up the `driver` of the connection a query console document belongs
/// to, so the caller can pick the right keyword set for tokenizing it.
///
/// Query consoles are saved as `{sanitized_label}-{id8}.sql` under
/// `~/.config/zed/db_client/queries/` (see `connection_query_path` in
/// `db_client_ui`'s `panel.rs`), where `id8` is the first 8 hex characters of
/// the connection's UUID with dashes removed. This re-derives the connection
/// from the document's file URI by matching that suffix against
/// `~/.config/zed/db_connections.json`, entirely outside the LSP protocol —
/// the client (Zed) has no notion of "database driver" to hand over
/// explicitly, so this is the only signal available.
pub fn connection_driver_for_document(uri: &str) -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let zed_config_dir = Path::new(&home).join(".config/zed");
    connection_driver_for_document_in(uri, &zed_config_dir)
}

/// Core of `connection_driver_for_document`, taking the Zed config directory
/// explicitly so tests do not depend on process-global `$HOME` (which is
/// unsafe to mutate across parallel test threads).
fn connection_driver_for_document_in(uri: &str, zed_config_dir: &Path) -> Option<String> {
    let path = uri.strip_prefix("file://")?;
    let file_stem = Path::new(path).file_stem()?.to_str()?;
    let id8 = file_stem.rsplit('-').next()?;
    if id8.len() != 8 || !id8.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }

    let connections_path = zed_config_dir.join("db_connections.json");
    let contents = std::fs::read_to_string(connections_path).ok()?;
    let root: serde_json::Value = serde_json::from_str(&contents).ok()?;
    let connections = root.get("connections")?.as_array()?;
    connections.iter().find_map(|connection| {
        let id = connection.get("id")?.as_str()?;
        let simple_id = id.replace('-', "");
        if simple_id.get(..8)?.eq_ignore_ascii_case(id8) {
            connection.get("driver")?.as_str().map(str::to_string)
        } else {
            None
        }
    })
}

/// LSP semantic token type, in the exact order advertised in the server's
/// `SemanticTokensLegend`. The index into this list is the LSP token type id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemanticTokenKind {
    Keyword,
    String,
    Number,
    Comment,
    Operator,
    Variable,
}

impl SemanticTokenKind {
    /// LSP legend, in the order matching `token_type_index`. Keep in sync with
    /// the capabilities advertised in `main.rs`.
    pub const LEGEND: [&'static str; 6] = [
        "keyword", "string", "number", "comment", "operator", "variable",
    ];

    pub fn token_type_index(self) -> u32 {
        match self {
            SemanticTokenKind::Keyword => 0,
            SemanticTokenKind::String => 1,
            SemanticTokenKind::Number => 2,
            SemanticTokenKind::Comment => 3,
            SemanticTokenKind::Operator => 4,
            SemanticTokenKind::Variable => 5,
        }
    }
}

/// One highlighted span, already split so it never crosses a line boundary
/// (LSP semantic tokens cannot span multiple lines).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawToken {
    /// 0-based line number.
    pub start_line: u32,
    /// UTF-16 code unit offset of the token's start within its line.
    pub start_char_utf16: u32,
    /// Length of the token in UTF-16 code units.
    pub length_utf16: u32,
    pub kind: SemanticTokenKind,
}

/// Splits `text` into its lines, preserving exact content (no trailing `\n`),
/// indexed the same way sqlparser's 1-based `Location::line` refers to them
/// (`lines()[line - 1]`).
fn split_lines(text: &str) -> Vec<&str> {
    text.split('\n').collect()
}

/// Converts a 1-based, character-counted `Location` into a 0-based line
/// index and a UTF-16 code-unit column, by re-deriving the offset from the
/// actual source text rather than assuming `column` is already a UTF-16
/// offset (sqlparser counts `char`s, which differs from UTF-16 for any
/// character outside the Basic Multilingual Plane).
fn location_to_utf16(lines: &[&str], location: Location) -> (u32, u32) {
    let line_index = location.line.saturating_sub(1) as usize;
    let line_text = lines.get(line_index).copied().unwrap_or("");
    let char_column = location.column.saturating_sub(1) as usize;
    let utf16_column = line_text
        .chars()
        .take(char_column)
        .map(char::len_utf16)
        .sum::<usize>() as u32;
    (line_index as u32, utf16_column)
}

/// CQL keywords with no equivalent in sqlparser's ANSI/MySQL-oriented
/// `Keyword` enum, so `Word::keyword` comes back `NoKeyword` for them even
/// though they are reserved words in Cassandra/Scylla's CQL grammar.
/// Checked case-insensitively against the raw token text.
const CQL_EXTRA_KEYWORDS: &[&str] = &[
    "KEYSPACE",
    "KEYSPACES",
    "TTL",
    "WRITETIME",
    "COUNTER",
    "FROZEN",
    "TOKEN",
    "ALLOW",
    "FILTERING",
    "PAGING",
    "CONSISTENCY",
    "BATCH",
    "PARTITIONER",
    "COMPACT",
    "ENTRIES",
];

/// Classifies a token, returning `None` for whitespace that carries no
/// semantic meaning (plain spaces/tabs/newlines) and should not be emitted.
///
/// `is_cql` extends keyword recognition with CQL-only reserved words (see
/// `CQL_EXTRA_KEYWORDS`) that sqlparser's tokenizer, being ANSI/MySQL
/// oriented, does not know about and would otherwise leave classified as a
/// plain variable.
fn classify(token: &Token, is_cql: bool) -> Option<SemanticTokenKind> {
    use sqlparser::keywords::Keyword;

    match token {
        Token::Word(word) => {
            let is_keyword = word.keyword != Keyword::NoKeyword
                || (is_cql
                    && CQL_EXTRA_KEYWORDS
                        .iter()
                        .any(|kw| kw.eq_ignore_ascii_case(&word.value)));
            Some(if is_keyword {
                SemanticTokenKind::Keyword
            } else {
                SemanticTokenKind::Variable
            })
        }
        Token::Number(..) => Some(SemanticTokenKind::Number),
        Token::SingleQuotedString(_)
        | Token::DoubleQuotedString(_)
        | Token::TripleSingleQuotedString(_)
        | Token::TripleDoubleQuotedString(_)
        | Token::DollarQuotedString(_)
        | Token::SingleQuotedByteStringLiteral(_)
        | Token::DoubleQuotedByteStringLiteral(_)
        | Token::TripleSingleQuotedByteStringLiteral(_)
        | Token::TripleDoubleQuotedByteStringLiteral(_)
        | Token::SingleQuotedRawStringLiteral(_)
        | Token::DoubleQuotedRawStringLiteral(_)
        | Token::TripleSingleQuotedRawStringLiteral(_)
        | Token::TripleDoubleQuotedRawStringLiteral(_)
        | Token::NationalStringLiteral(_)
        | Token::EscapedStringLiteral(_)
        | Token::UnicodeStringLiteral(_)
        | Token::HexStringLiteral(_) => Some(SemanticTokenKind::String),
        Token::Whitespace(Whitespace::Space | Whitespace::Newline | Whitespace::Tab) => None,
        Token::Whitespace(
            Whitespace::SingleLineComment { .. } | Whitespace::MultiLineComment(_),
        ) => Some(SemanticTokenKind::Comment),
        Token::EOF => None,
        _ => Some(SemanticTokenKind::Operator),
    }
}

/// Splits a raw sqlparser span into one `RawToken` per physical line it
/// covers (LSP semantic tokens must not cross line boundaries), deriving
/// exact per-line boundaries from the real source text rather than the
/// token's (possibly unescaped, re-serialized) display text.
fn split_span_by_line(
    lines: &[&str],
    start: Location,
    end: Location,
    kind: SemanticTokenKind,
) -> Vec<RawToken> {
    if start.line == end.line {
        let (line, start_col) = location_to_utf16(lines, start);
        let (_, end_col) = location_to_utf16(lines, end);
        if end_col <= start_col {
            return Vec::new();
        }
        return vec![RawToken {
            start_line: line,
            start_char_utf16: start_col,
            length_utf16: end_col - start_col,
            kind,
        }];
    }

    let mut tokens = Vec::new();
    let (start_line, start_col) = location_to_utf16(lines, start);
    let first_line_text = lines.get(start_line as usize).copied().unwrap_or("");
    let first_line_len = first_line_text.encode_utf16().count() as u32;
    if first_line_len > start_col {
        tokens.push(RawToken {
            start_line,
            start_char_utf16: start_col,
            length_utf16: first_line_len - start_col,
            kind,
        });
    }

    let (end_line, end_col) = location_to_utf16(lines, end);
    for line in (start_line + 1)..end_line {
        let line_text = lines.get(line as usize).copied().unwrap_or("");
        let line_len = line_text.encode_utf16().count() as u32;
        if line_len > 0 {
            tokens.push(RawToken {
                start_line: line,
                start_char_utf16: 0,
                length_utf16: line_len,
                kind,
            });
        }
    }

    if end_col > 0 {
        tokens.push(RawToken {
            start_line: end_line,
            start_char_utf16: 0,
            length_utf16: end_col,
            kind,
        });
    }

    tokens
}

/// Tokenizes `text` as MySQL-dialect SQL and returns semantic tokens, tolerating
/// malformed input: if the tokenizer errors partway through, the tokens
/// collected up to that point are still returned instead of nothing.
///
/// `is_cql` is set for buffers attached to a Cassandra/Scylla connection
/// console; it only affects keyword classification (see `classify`), not
/// tokenization itself — sqlparser has no CQL dialect, so the underlying
/// tokenizer stays MySQL-oriented. One consequence: a bare UUID literal
/// (`8f14e45f-...`) is split into Number/Word/Minus tokens instead of being
/// recognized as a single value, since sqlparser reads the hyphens as the
/// subtraction operator. This is a cosmetic fragmentation of the highlight,
/// not a parsing error — the query text itself is untouched.
pub fn tokenize(text: &str, is_cql: bool) -> Vec<RawToken> {
    let dialect = MySqlDialect {};
    let mut tokenizer = Tokenizer::new(&dialect, text);
    let mut spans: Vec<TokenWithSpan> = Vec::new();
    // Deliberately ignore the Result: per sqlparser's own documentation, `buf`
    // already contains every token successfully parsed before the error, and
    // we want to highlight as much as we can rather than give up entirely.
    let _ = tokenizer.tokenize_with_location_into_buf(&mut spans);

    let lines = split_lines(text);
    let mut tokens = Vec::new();
    for token_with_span in &spans {
        let Some(kind) = classify(&token_with_span.token, is_cql) else {
            continue;
        };
        tokens.extend(split_span_by_line(
            &lines,
            token_with_span.span.start,
            token_with_span.span.end,
            kind,
        ));
    }
    tokens
}

/// Encodes tokens into the LSP semantic tokens relative-delta wire format:
/// `[deltaLine, deltaStartChar, length, tokenType, tokenModifiers]` per token,
/// flattened into one array. Tokens are sorted by position first, since
/// callers must not assume they already arrive in order.
pub fn encode_relative(tokens: &[RawToken]) -> Vec<u32> {
    let mut sorted: Vec<&RawToken> = tokens.iter().collect();
    sorted.sort_by_key(|token| (token.start_line, token.start_char_utf16));

    let mut data = Vec::with_capacity(sorted.len() * 5);
    let mut previous_line = 0u32;
    let mut previous_start = 0u32;
    for token in sorted {
        let delta_line = token.start_line - previous_line;
        let delta_start = if delta_line == 0 {
            token.start_char_utf16 - previous_start
        } else {
            token.start_char_utf16
        };
        data.push(delta_line);
        data.push(delta_start);
        data.push(token.length_utf16);
        data.push(token.kind.token_type_index());
        data.push(0); // token modifiers bitset, unused
        previous_line = token.start_line;
        previous_start = token.start_char_utf16;
    }
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keyword_tokens_on_line(tokens: &[RawToken], text: &str, line: u32) -> Vec<String> {
        let lines = split_lines(text);
        tokens
            .iter()
            .filter(|token| token.start_line == line && token.kind == SemanticTokenKind::Keyword)
            .map(|token| {
                let line_text = lines[line as usize];
                let start = utf16_prefix_char_len(line_text, token.start_char_utf16);
                let end =
                    utf16_prefix_char_len(line_text, token.start_char_utf16 + token.length_utf16);
                line_text[start..end].to_string()
            })
            .collect()
    }

    /// Converts a UTF-16 code-unit count back into a byte offset within `line`,
    /// so the test can slice the original `&str` for a human-readable assertion.
    fn utf16_prefix_char_len(line: &str, utf16_count: u32) -> usize {
        let mut seen_utf16 = 0u32;
        for (byte_index, ch) in line.char_indices() {
            if seen_utf16 >= utf16_count {
                return byte_index;
            }
            seen_utf16 += ch.len_utf16() as u32;
        }
        line.len()
    }

    const BUG_REPORT_SQL: &str = "SELECT *\nFROM app_push_report.app_content_mod_stats\nWHERE row_ID =\\''.$content_res['row_ID'].'\\' AND mod_int=\\''.$user_modulo.'\\' AND os=\\'android\\';';\n\nSELECT *\nFROM app_push_report.app_content_mod_stats;\n";

    #[test]
    fn second_select_after_malformed_php_embedded_query_is_still_a_keyword() {
        let tokens = tokenize(BUG_REPORT_SQL, false);

        // Line 0 is the first `SELECT`; the second query (after the malformed
        // PHP-embedded WHERE clause and a blank line) starts on line 4.
        let first_select = keyword_tokens_on_line(&tokens, BUG_REPORT_SQL, 0);
        assert_eq!(first_select, vec!["SELECT".to_string()]);

        let second_select = keyword_tokens_on_line(&tokens, BUG_REPORT_SQL, 4);
        assert_eq!(
            second_select,
            vec!["SELECT".to_string()],
            "the second SELECT must be classified as a keyword, not swallowed into a runaway string \
             the way the tree-sitter grammar's ANSI-only escape handling does"
        );
    }

    #[test]
    fn simple_valid_sql_is_tokenized_without_regressions() {
        let text = "SELECT * FROM t WHERE id = 'x';";
        let tokens = tokenize(text, false);

        // sqlparser's keyword list is intentionally broad (it includes words
        // like ID/NAME/VALUE that are contextual keywords in some dialects),
        // so only assert the three unambiguous clause keywords are present.
        let keywords = keyword_tokens_on_line(&tokens, text, 0);
        for expected in ["SELECT", "FROM", "WHERE"] {
            assert!(
                keywords.contains(&expected.to_string()),
                "expected {expected:?} among keyword tokens, got {keywords:?}"
            );
        }

        let has_string = tokens
            .iter()
            .any(|token| token.kind == SemanticTokenKind::String);
        assert!(
            has_string,
            "the 'x' string literal must be classified as a string token"
        );
    }

    #[test]
    fn cql_extra_keywords_are_classified_only_when_is_cql_is_true() {
        let text = "SELECT * FROM ks.t WHERE token(pk) > 0 AND ttl(val) < 3600 ALLOW FILTERING;";

        let plain_keywords = keyword_tokens_on_line(&tokenize(text, false), text, 0);
        assert!(
            !plain_keywords
                .iter()
                .any(|w| w.eq_ignore_ascii_case("token")),
            "TOKEN must not be a keyword in plain SQL mode, got {plain_keywords:?}"
        );
        assert!(
            !plain_keywords
                .iter()
                .any(|w| w.eq_ignore_ascii_case("allow")),
            "ALLOW must not be a keyword in plain SQL mode, got {plain_keywords:?}"
        );

        let cql_keywords = keyword_tokens_on_line(&tokenize(text, true), text, 0);
        for expected in ["token", "ttl", "ALLOW", "FILTERING"] {
            assert!(
                cql_keywords
                    .iter()
                    .any(|w| w.eq_ignore_ascii_case(expected)),
                "expected {expected:?} among CQL keyword tokens, got {cql_keywords:?}"
            );
        }
    }

    #[test]
    fn connection_driver_for_document_matches_by_id8_suffix() {
        let config_dir = std::env::temp_dir().join(format!(
            "sql_semantic_tokens_lsp_test_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&config_dir).expect("create test config dir");
        std::fs::write(
            config_dir.join("db_connections.json"),
            r#"{
                "folders": [],
                "connections": [
                    {"id": "18e19a37-6549-42d9-9dbd-9e843d6e7c4f", "driver": "Cassandra"},
                    {"id": "84cef9dd-1367-4925-9bfe-5038393ff3d7", "driver": "MySQL"}
                ]
            }"#,
        )
        .expect("write test db_connections.json");

        let cassandra_uri = "file:///home/user/.config/zed/db_client/queries/Scylla-18e19a37.sql";
        assert_eq!(
            connection_driver_for_document_in(cassandra_uri, &config_dir),
            Some("Cassandra".to_string())
        );

        let mysql_uri = "file:///home/user/.config/zed/db_client/queries/Prod-84cef9dd.sql";
        assert_eq!(
            connection_driver_for_document_in(mysql_uri, &config_dir),
            Some("MySQL".to_string())
        );

        let unknown_uri = "file:///home/user/.config/zed/db_client/queries/Ghost-deadbeef.sql";
        assert_eq!(
            connection_driver_for_document_in(unknown_uri, &config_dir),
            None
        );

        std::fs::remove_dir_all(&config_dir).expect("clean up test config dir");
    }

    #[test]
    fn encode_relative_round_trips_absolute_positions() {
        let tokens = vec![
            RawToken {
                start_line: 0,
                start_char_utf16: 0,
                length_utf16: 6,
                kind: SemanticTokenKind::Keyword,
            },
            RawToken {
                start_line: 0,
                start_char_utf16: 7,
                length_utf16: 1,
                kind: SemanticTokenKind::Operator,
            },
            RawToken {
                start_line: 2,
                start_char_utf16: 3,
                length_utf16: 4,
                kind: SemanticTokenKind::Keyword,
            },
        ];
        let data = encode_relative(&tokens);
        assert_eq!(
            data,
            vec![
                0, 0, 6, 0, 0, // SELECT at (0,0), length 6, type keyword
                0, 7, 1, 4, 0, // * at (0,7), length 1, type operator
                2, 3, 4, 0, 0, // FROM at (2,3), length 4, type keyword
            ]
        );
    }
}
