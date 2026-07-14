use std::ops::Range;

use collections::HashSet;
use db_client::DatabaseDriver;
use sqlparser::ast::{
    Expr, ObjectName, Query, SetExpr, Spanned, Statement, TableFactor, visit_expressions,
    visit_relations,
};
use sqlparser::parser::{Parser, ParserError};
use sqlparser::tokenizer::{Location, Span};

use crate::sql_ast::{dialect_for_driver, statement_spans};
use crate::sql_binder::{
    BindCtx, NavigationTarget, SchemaLookup, database_and_table, offset_for_location,
    resolve_navigation,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiagnosticLevel {
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SqlDiagnostic {
    pub(crate) range: Range<usize>,
    pub(crate) level: DiagnosticLevel,
    pub(crate) message: String,
}

/// Validates every statement in `text` against `driver`'s dialect and
/// `schema`'s cache.
///
/// A statement that fails to parse produces one `Warning` diagnostic
/// (never `Error`: an approximated dialect or an unrecognized vendor
/// extension must never look like a hard failure). A statement that parses
/// is checked semantically -- unknown real tables and unknown qualified
/// columns -- but only against databases/tables the cache actually has data
/// for; a database or table the cache has never fetched produces no
/// diagnostic at all, since that is an absence of information, not a known
/// error. Drivers with no SQL grammar (Redis) always produce nothing.
pub(crate) fn validate(
    text: &str,
    driver: DatabaseDriver,
    default_database: Option<&str>,
    schema: &dyn SchemaLookup,
) -> Vec<SqlDiagnostic> {
    if driver == DatabaseDriver::MongoDB {
        return validate_mongo_shell(text);
    }
    let Some(dialect) = dialect_for_driver(driver) else {
        return Vec::new();
    };
    let mut diagnostics = Vec::new();
    for span in statement_spans(text) {
        let Some(statement_text) = text.get(span.clone()) else {
            continue;
        };
        if statement_text.trim().is_empty() {
            continue;
        }
        match Parser::parse_sql(dialect.as_ref(), statement_text) {
            Ok(statements) => {
                for statement in &statements {
                    check_semantics(
                        statement,
                        statement_text,
                        span.start,
                        default_database,
                        schema,
                        &mut diagnostics,
                    );
                }
            }
            Err(err) => {
                if driver == DatabaseDriver::ClickHouse
                    && retry_after_blanking_aliased_final(
                        dialect.as_ref(),
                        statement_text,
                        span.start,
                        default_database,
                        schema,
                        &mut diagnostics,
                    )
                {
                    continue;
                }
                let local_range =
                    syntax_error_range(statement_text, &err).unwrap_or(0..statement_text.len());
                diagnostics.push(SqlDiagnostic {
                    range: local_range.start + span.start..local_range.end + span.start,
                    level: DiagnosticLevel::Warning,
                    message: err.to_string(),
                });
            }
        }
    }
    diagnostics
}

/// Validates each mongo shell statement in `text` with the same tiny parser
/// `MongoProvider::execute_query` runs at execution time, so a syntax mistake
/// is flagged in the editor before Ctrl+Enter, not after a failed round trip.
/// Mongo shell has no real dialect for `sqlparser`, so this bypasses it
/// entirely rather than misparsing shell calls as SQL.
fn validate_mongo_shell(text: &str) -> Vec<SqlDiagnostic> {
    let mut diagnostics = Vec::new();
    for span in statement_spans(text) {
        let Some(statement_text) = text.get(span.clone()) else {
            continue;
        };
        if statement_text.trim().is_empty() {
            continue;
        }
        if let Err(error) = db_client::mongo_provider::parse_mongo_shell_statement(statement_text) {
            diagnostics.push(SqlDiagnostic {
                range: span,
                level: DiagnosticLevel::Warning,
                message: error.to_string(),
            });
        }
    }
    diagnostics
}

/// ClickHouse requires `FINAL` to follow a table's alias (`FROM t AS e
/// FINAL`), but `sqlparser`'s `ClickHouseDialect` only accepts it directly
/// after the bare table name with no alias at all (`FROM t FINAL`) -- every
/// aliased form is a parse error there, even though it is exactly the form
/// ClickHouse itself requires once a query joins more than one table. Rather
/// than let that upstream gap flag ordinary ClickHouse queries as broken,
/// this repeatedly reparses after blanking out (with equal-length spaces, so
/// every other token keeps its original line/column) *only* the exact
/// `FINAL` token the parser itself just rejected -- never a blanket sweep of
/// every `FINAL` in the statement -- and only when that token falls inside
/// the top-level `FROM`-clause's table list, never after a `WHERE`/`GROUP
/// BY`/etc. keyword. Both restrictions matter: a blanket sweep could turn an
/// unrelated genuine mistake that happens to contain the word "final" (e.g.
/// `WHERE a FINAL`, a plain syntax error) into a silently-accepted query.
/// Returns `true` once a retry recovers a clean parse, in which case the
/// caller must not also push the original syntax-error diagnostic.
fn retry_after_blanking_aliased_final(
    dialect: &dyn sqlparser::dialect::Dialect,
    statement_text: &str,
    statement_offset: usize,
    default_database: Option<&str>,
    schema: &dyn SchemaLookup,
    out: &mut Vec<SqlDiagnostic>,
) -> bool {
    // Computed once against the original text: blanking never changes any
    // other token's position, so the FROM-clause's boundaries never move
    // between retries.
    let Some(from_clause) = top_level_from_clause_range(statement_text) else {
        return false;
    };
    let mut buffer = statement_text.as_bytes().to_vec();
    // Bounded by how many `FINAL` tokens a statement could plausibly contain
    // (each iteration permanently blanks one); this is just a defensive
    // backstop -- well-formed input never comes close to it.
    for _ in 0..16 {
        let Ok(candidate) = std::str::from_utf8(&buffer) else {
            return false;
        };
        match Parser::parse_sql(dialect, candidate) {
            Ok(statements) => {
                for statement in &statements {
                    // `statement_text` (the original, unblanked text) is
                    // passed here on purpose: blanking only overwrites
                    // `FINAL`'s letters with spaces of the same length, so
                    // every other token keeps the exact line/column it has
                    // in the original, and ranges computed against the
                    // original text are still correct for the reparsed
                    // AST's spans.
                    check_semantics(
                        statement,
                        statement_text,
                        statement_offset,
                        default_database,
                        schema,
                        out,
                    );
                }
                return true;
            }
            Err(err) => {
                let Some(token_range) = final_token_error_range(candidate, &err) else {
                    return false;
                };
                if token_range.start < from_clause.start || token_range.end > from_clause.end {
                    return false;
                }
                let Some(slice) = buffer.get_mut(token_range) else {
                    return false;
                };
                slice.fill(b' ');
            }
        }
    }
    false
}

/// If `err` is specifically "found: FINAL" -- the exact wording `sqlparser`
/// reports when it chokes on that literal keyword -- the byte range of that
/// token in `text`, per the error's own reported line/column. Returns `None`
/// for every other error shape, so the caller only ever blanks the one token
/// the parser itself just rejected, never a heuristic guess.
fn final_token_error_range(text: &str, err: &ParserError) -> Option<Range<usize>> {
    let message = err.to_string();
    if !message.contains("found: FINAL at Line") {
        return None;
    }
    let location = parser_error_location(&message)?;
    let start = offset_for_location(text, location)?;
    let end = start + "FINAL".len();
    if text.get(start..end)?.eq_ignore_ascii_case("FINAL") {
        Some(start..end)
    } else {
        None
    }
}

/// Byte range of the top-level `FROM` clause's table/join list: from just
/// after the first top-level `FROM` keyword up to whichever comes first --
/// the next top-level clause keyword that can only follow a `FROM` clause,
/// or the end of the statement. "Top-level" skips anything inside a quoted
/// string/identifier, so a keyword spelled out in a string literal is never
/// mistaken for a real clause boundary. Returns `None` when there is no
/// top-level `FROM` at all.
fn top_level_from_clause_range(text: &str) -> Option<Range<usize>> {
    const CLAUSE_TERMINATORS: &[&str] = &[
        "where",
        "group",
        "order",
        "having",
        "limit",
        "settings",
        "window",
        "qualify",
        "union",
        "except",
        "intersect",
        "into",
        "format",
    ];
    let from_start = find_top_level_keyword(text, "from", 0)?;
    let clause_start = from_start + "from".len();
    let clause_end = CLAUSE_TERMINATORS
        .iter()
        .filter_map(|terminator| find_top_level_keyword(text, terminator, clause_start))
        .min()
        .unwrap_or(text.len());
    Some(clause_start..clause_end)
}

/// Byte offset of the first case-insensitive, word-bounded occurrence of
/// `keyword` in `text` at or after `from_index`, skipping any single/double/
/// backtick-quoted span so a keyword spelled out inside a string literal or
/// quoted identifier is never mistaken for a real one. Bytes `>= 0x80`
/// (any non-ASCII UTF-8 byte) count as identifier bytes for the boundary
/// check, so a match is never accepted right next to a non-ASCII character.
fn find_top_level_keyword(text: &str, keyword: &str, from_index: usize) -> Option<usize> {
    let lower = text.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let is_identifier_byte =
        |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_' || byte >= 0x80;
    let mut index = from_index;
    let mut quote: Option<u8> = None;
    while index < bytes.len() {
        let byte = bytes[index];
        if let Some(active) = quote {
            if byte == active {
                quote = None;
            }
            index += 1;
            continue;
        }
        if matches!(byte, b'\'' | b'"' | b'`') {
            quote = Some(byte);
            index += 1;
            continue;
        }
        if lower[index..].starts_with(keyword) {
            let end = index + keyword.len();
            let before_is_boundary = index == 0 || !is_identifier_byte(bytes[index - 1]);
            let after_is_boundary = end >= bytes.len() || !is_identifier_byte(bytes[end]);
            if before_is_boundary && after_is_boundary {
                return Some(index);
            }
        }
        index += 1;
    }
    None
}

/// Extracts `Line: N, Column: M` from a `ParserError`'s display message
/// (most "unexpected token" errors embed one).
fn parser_error_location(message: &str) -> Option<Location> {
    let line_marker = "Line: ";
    let line_start = message.find(line_marker)? + line_marker.len();
    let rest = &message[line_start..];
    let comma = rest.find(',')?;
    let line: u64 = rest[..comma].trim().parse().ok()?;
    let column_marker = "Column: ";
    let column_start = rest.find(column_marker)? + column_marker.len();
    let column: u64 = rest[column_start..]
        .trim_end_matches(|c: char| !c.is_ascii_digit())
        .parse()
        .ok()?;
    Some(Location { line, column })
}

/// Extracts a precise range from a `ParserError`'s message when it embeds a
/// `Line: N, Column: M` location (most "unexpected token" errors do); falls
/// back to `None` (the caller then flags the whole statement) for errors
/// like an unexpected EOF, which have no specific token to point at.
fn syntax_error_range(statement_text: &str, err: &ParserError) -> Option<Range<usize>> {
    let location = parser_error_location(&err.to_string())?;
    let start = offset_for_location(statement_text, location)?;
    let end = statement_text[start..]
        .find(|c: char| c.is_whitespace() || ",()".contains(c))
        .map(|relative| start + relative)
        .unwrap_or(statement_text.len());
    Some(start..end.max(start + 1).min(statement_text.len()))
}

fn check_semantics(
    statement: &Statement,
    statement_text: &str,
    statement_offset: usize,
    default_database: Option<&str>,
    schema: &dyn SchemaLookup,
    out: &mut Vec<SqlDiagnostic>,
) {
    let mut cte_names = HashSet::default();
    collect_cte_names(statement, &mut cte_names);

    let _ = visit_relations(
        statement,
        |name: &ObjectName| -> std::ops::ControlFlow<()> {
            check_table_reference(
                name,
                &cte_names,
                statement_text,
                statement_offset,
                default_database,
                schema,
                out,
            );
            std::ops::ControlFlow::Continue(())
        },
    );

    let ctx = BindCtx {
        schema,
        default_database,
    };
    let _ = visit_expressions(statement, |expr: &Expr| -> std::ops::ControlFlow<()> {
        check_qualified_column(statement, expr, &ctx, statement_text, statement_offset, out);
        std::ops::ControlFlow::Continue(())
    });
}

fn check_table_reference(
    name: &ObjectName,
    cte_names: &HashSet<String>,
    statement_text: &str,
    statement_offset: usize,
    default_database: Option<&str>,
    schema: &dyn SchemaLookup,
    out: &mut Vec<SqlDiagnostic>,
) {
    let (database_part, table) = database_and_table(name);
    if table.is_empty() {
        return;
    }
    // A CTE reference looks identical to a real table reference at parse
    // time (sqlparser does not disambiguate them); skip anything that
    // matches a CTE name declared anywhere in the statement rather than risk
    // flagging a valid CTE use as an unknown table.
    if cte_names.contains(&table.to_ascii_lowercase()) {
        return;
    }
    let Some(database) = database_part.or_else(|| default_database.map(str::to_string)) else {
        return;
    };
    if !schema.has_schema_for_database(&database) {
        return;
    }
    if schema.table_exists(&database, &table) {
        return;
    }
    let Some(range) = span_to_range(statement_text, name.span()) else {
        return;
    };
    out.push(SqlDiagnostic {
        range: range.start + statement_offset..range.end + statement_offset,
        level: DiagnosticLevel::Warning,
        message: format!("Unknown table `{database}.{table}`"),
    });
}

fn check_qualified_column(
    statement: &Statement,
    expr: &Expr,
    ctx: &BindCtx,
    statement_text: &str,
    statement_offset: usize,
    out: &mut Vec<SqlDiagnostic>,
) {
    // Only a plain two-part `qualifier.column` is checked here -- a bare
    // column is deliberately never flagged (it may validly reference a
    // SELECT-list output alias in ORDER BY/GROUP BY, which this validator
    // cannot distinguish from a real table column without real risk of a
    // false positive), and a three-or-more-part reference is rare enough in
    // the supported dialects that it is left unchecked rather than guessed.
    let Expr::CompoundIdentifier(idents) = expr else {
        return;
    };
    if idents.len() != 2 {
        return;
    }
    let qualifier = &idents[0];
    let column_ident = &idents[1];
    let Some(NavigationTarget::Column {
        database,
        table,
        column,
        ..
    }) = resolve_navigation(statement, column_ident.span.start, ctx)
    else {
        // Unresolvable alias, ambiguous reference, or a derived-table
        // projection with no traceable real source -- never flagged, since
        // "I could not determine what this refers to" is not the same claim
        // as "this definitely does not exist".
        return;
    };
    let Some(database) = database.or_else(|| ctx.default_database.map(str::to_string)) else {
        return;
    };
    if !ctx.schema.has_columns_for_table(&database, &table) {
        return;
    }
    if ctx.schema.table_has_column(&database, &table, &column) {
        return;
    }
    let Some(range) = span_to_range(statement_text, column_ident.span) else {
        return;
    };
    out.push(SqlDiagnostic {
        range: range.start + statement_offset..range.end + statement_offset,
        level: DiagnosticLevel::Warning,
        message: format!("Unknown column `{}.{}`", qualifier.value, column),
    });
}

fn span_to_range(statement_text: &str, span: Span) -> Option<Range<usize>> {
    let start = offset_for_location(statement_text, span.start)?;
    let end = offset_for_location(statement_text, span.end)?;
    Some(start..end.max(start))
}

/// Collects every CTE alias name declared anywhere in `statement` --
/// including inside derived-table subqueries and an `INSERT ... SELECT`'s
/// source -- so a real table reference is never confused with a CTE
/// reference, which is syntactically identical at parse time.
fn collect_cte_names(statement: &Statement, names: &mut HashSet<String>) {
    match statement {
        Statement::Query(query) => collect_cte_names_from_query(query, names),
        Statement::Insert(insert) => {
            if let Some(source) = &insert.source {
                collect_cte_names_from_query(source, names);
            }
        }
        _ => {}
    }
}

fn collect_cte_names_from_query(query: &Query, names: &mut HashSet<String>) {
    if let Some(with) = &query.with {
        for cte in &with.cte_tables {
            names.insert(cte.alias.name.value.to_ascii_lowercase());
            collect_cte_names_from_query(&cte.query, names);
        }
    }
    collect_cte_names_from_set_expr(&query.body, names);
}

fn collect_cte_names_from_set_expr(set_expr: &SetExpr, names: &mut HashSet<String>) {
    match set_expr {
        SetExpr::Select(select) => {
            for table_with_joins in &select.from {
                collect_cte_names_from_table_factor(&table_with_joins.relation, names);
                for join in &table_with_joins.joins {
                    collect_cte_names_from_table_factor(&join.relation, names);
                }
            }
        }
        SetExpr::Query(query) => collect_cte_names_from_query(query, names),
        SetExpr::SetOperation { left, right, .. } => {
            collect_cte_names_from_set_expr(left, names);
            collect_cte_names_from_set_expr(right, names);
        }
        _ => {}
    }
}

fn collect_cte_names_from_table_factor(factor: &TableFactor, names: &mut HashSet<String>) {
    if let TableFactor::Derived { subquery, .. } = factor {
        collect_cte_names_from_query(subquery, names);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeSchema {
        tables_by_database: std::collections::HashMap<String, Vec<String>>,
        columns_by_table: std::collections::HashMap<(String, String), Vec<String>>,
    }

    impl FakeSchema {
        fn and_table(mut self, database: &str, table: &str, columns: &[&str]) -> Self {
            self.tables_by_database
                .entry(database.to_string())
                .or_default()
                .push(table.to_string());
            self.columns_by_table.insert(
                (database.to_string(), table.to_string()),
                columns.iter().map(|c| c.to_string()).collect(),
            );
            self
        }

        fn with_known_empty_database(database: &str) -> Self {
            let mut schema = FakeSchema::default();
            schema
                .tables_by_database
                .insert(database.to_string(), Vec::new());
            schema
        }
    }

    impl SchemaLookup for FakeSchema {
        fn table_has_column(&self, database: &str, table: &str, column: &str) -> bool {
            self.columns_by_table
                .get(&(database.to_string(), table.to_string()))
                .is_some_and(|columns| columns.iter().any(|c| c.eq_ignore_ascii_case(column)))
        }

        fn has_schema_for_database(&self, database: &str) -> bool {
            self.tables_by_database.contains_key(database)
        }

        fn table_exists(&self, database: &str, table: &str) -> bool {
            self.tables_by_database
                .get(database)
                .is_some_and(|tables| tables.iter().any(|t| t.eq_ignore_ascii_case(table)))
        }

        fn has_columns_for_table(&self, database: &str, table: &str) -> bool {
            self.columns_by_table
                .contains_key(&(database.to_string(), table.to_string()))
        }
    }

    #[test]
    fn clickhouse_specific_syntax_produces_zero_diagnostics() {
        let schema = FakeSchema::default();
        let snippets = [
            "SELECT * FROM t FINAL",
            "SELECT * FROM t ARRAY JOIN arr AS a",
            "ALTER TABLE t ADD COLUMN x Nullable(String)",
            "CREATE TABLE t (id UInt32, name String) ENGINE = MergeTree() ORDER BY id",
            "SELECT * FROM t WHERE x IN (SELECT y FROM t2) SETTINGS max_threads=4",
            "SELECT topK(5)(x) FROM t",
            "SELECT * FROM t LIMIT 5 BY category",
            "INSERT INTO t VALUES (1, 'a')",
            "ALTER TABLE t DROP INDEX idx",
            "SELECT * FROM t WHERE toDate(created) = today()",
            "WITH x AS (SELECT 1) SELECT * FROM x",
            "CREATE TABLE t (id UInt32, tags Array(String)) ENGINE = MergeTree ORDER BY id",
            "SELECT * FROM t PREWHERE id > 0 WHERE name = 'x'",
            "OPTIMIZE TABLE t FINAL",
        ];
        for snippet in snippets {
            let diagnostics = validate(snippet, DatabaseDriver::ClickHouse, None, &schema);
            assert!(
                diagnostics.is_empty(),
                "expected no diagnostics for {snippet:?}, got {diagnostics:?}"
            );
        }
    }

    // sqlparser's ClickHouseDialect only accepts `FINAL` directly after a
    // bare table name with no alias; real ClickHouse requires it to follow
    // the alias whenever one is given (`FROM t AS e FINAL`), so every one of
    // these must be recovered by `retry_after_blanking_aliased_final`
    // instead of surfacing a false "syntax error" warning.
    #[test]
    fn clickhouse_final_after_an_alias_is_not_flagged_as_a_syntax_error() {
        let schema = FakeSchema::default();
        let snippets = [
            "SELECT * FROM t e FINAL",
            "SELECT * FROM t AS e FINAL",
            "SELECT * FROM db.t AS e FINAL WHERE e.id > 0",
            "SELECT * FROM db.t e FINAL JOIN db.t2 f FINAL ON e.id = f.id",
        ];
        for snippet in snippets {
            let diagnostics = validate(snippet, DatabaseDriver::ClickHouse, None, &schema);
            assert!(
                diagnostics.is_empty(),
                "expected the aliased FINAL fallback to clear {snippet:?}, got {diagnostics:?}"
            );
        }
    }

    #[test]
    fn clickhouse_final_fallback_still_checks_semantics_against_the_reparsed_statement() {
        let schema = FakeSchema::default().and_table("db", "events", &["id", "created"]);
        let diagnostics = validate(
            "SELECT e.missing FROM db.events e FINAL WHERE e.id > 0",
            DatabaseDriver::ClickHouse,
            None,
            &schema,
        );
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("e.missing"));
    }

    // A genuine syntax error unrelated to FINAL must still be reported, not
    // silently swallowed by the fallback.
    #[test]
    fn clickhouse_genuine_syntax_error_is_still_flagged_when_it_contains_final() {
        let schema = FakeSchema::default();
        let diagnostics = validate(
            "SELECT * FROM t AS e FINAL WHERE (a = 1",
            DatabaseDriver::ClickHouse,
            None,
            &schema,
        );
        assert_eq!(diagnostics.len(), 1);
    }

    // `FINAL` used as a bare (broken) predicate in a WHERE clause must still
    // be flagged -- the fallback must only recover the specific case of
    // `FINAL` following a table alias in the FROM clause, never any stray
    // `FINAL` anywhere the parser happens to choke on it.
    #[test]
    fn clickhouse_final_used_incorrectly_outside_the_from_clause_is_still_flagged() {
        let schema = FakeSchema::default();
        let diagnostics = validate(
            "SELECT * FROM t WHERE a FINAL",
            DatabaseDriver::ClickHouse,
            None,
            &schema,
        );
        assert_eq!(
            diagnostics.len(),
            1,
            "a FINAL outside the FROM clause must not be silently blanked away"
        );
    }

    #[test]
    fn clickhouse_array_join_and_qualified_columns_are_not_falsely_flagged() {
        let schema = FakeSchema::default().and_table("db", "events", &["id", "tags", "created"]);
        let diagnostics = validate(
            "SELECT e.id FROM db.events e ARRAY JOIN e.tags AS tag WHERE e.created > today()",
            DatabaseDriver::ClickHouse,
            Some("db"),
            &schema,
        );
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn broken_statement_produces_one_warning_at_the_reported_location() {
        let text = "CREATE TALBE t (a int)";
        let schema = FakeSchema::default();
        let diagnostics = validate(text, DatabaseDriver::MySQL, None, &schema);
        assert_eq!(diagnostics.len(), 1);
        let diagnostic = &diagnostics[0];
        assert_eq!(diagnostic.level, DiagnosticLevel::Warning);
        assert_eq!(&text[diagnostic.range.clone()], "TALBE");
    }

    #[test]
    fn broken_statement_without_a_reported_location_flags_the_whole_statement() {
        let text = "SELECT * FROM t WHERE (a = 1";
        let schema = FakeSchema::default();
        let diagnostics = validate(text, DatabaseDriver::MySQL, None, &schema);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].range, 0..text.len());
    }

    #[test]
    fn unknown_table_is_flagged_when_the_database_is_cached() {
        let text = "SELECT * FROM db.missing";
        let schema = FakeSchema::default().and_table("db", "orders", &["id"]);
        let diagnostics = validate(text, DatabaseDriver::MySQL, None, &schema);
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("db.missing"));
        assert_eq!(&text[diagnostics[0].range.clone()], "db.missing");
    }

    #[test]
    fn unknown_column_is_flagged_on_a_known_table() {
        let text = "SELECT o.missing FROM db.orders o";
        let schema = FakeSchema::default().and_table("db", "orders", &["id"]);
        let diagnostics = validate(text, DatabaseDriver::MySQL, None, &schema);
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("o.missing"));
        assert_eq!(&text[diagnostics[0].range.clone()], "missing");
    }

    #[test]
    fn valid_query_against_a_fully_cached_schema_has_zero_diagnostics() {
        let text = "SELECT o.id FROM db.orders o WHERE o.id = 1";
        let schema = FakeSchema::default().and_table("db", "orders", &["id"]);
        assert!(validate(text, DatabaseDriver::MySQL, None, &schema).is_empty());
    }

    #[test]
    fn no_cache_data_for_the_database_suppresses_semantic_checks() {
        let text = "SELECT * FROM unexplored.anything";
        let schema = FakeSchema::default();
        assert!(validate(text, DatabaseDriver::MySQL, None, &schema).is_empty());
    }

    #[test]
    fn a_known_empty_database_still_flags_a_genuinely_unknown_table() {
        let text = "SELECT * FROM db.missing";
        let schema = FakeSchema::with_known_empty_database("db");
        let diagnostics = validate(text, DatabaseDriver::MySQL, None, &schema);
        assert_eq!(diagnostics.len(), 1);
    }

    #[test]
    fn uncached_columns_for_a_known_table_suppress_the_column_check() {
        let text = "SELECT o.anything FROM db.orders o";
        let schema = {
            let mut schema = FakeSchema::default();
            schema
                .tables_by_database
                .insert("db".to_string(), vec!["orders".to_string()]);
            schema
        };
        assert!(validate(text, DatabaseDriver::MySQL, None, &schema).is_empty());
    }

    #[test]
    fn a_cte_reference_is_never_flagged_as_an_unknown_table() {
        let text = "WITH recent AS (SELECT id FROM db.orders) SELECT * FROM recent";
        let schema = FakeSchema::default().and_table("db", "orders", &["id"]);
        assert!(validate(text, DatabaseDriver::MySQL, None, &schema).is_empty());
    }

    #[test]
    fn redis_never_produces_a_diagnostic() {
        let schema = FakeSchema::default();
        assert!(validate("not sql at all (((", DatabaseDriver::Redis, None, &schema).is_empty());
        assert!(
            validate(
                "SELECT * FROM db.missing",
                DatabaseDriver::Redis,
                None,
                &schema
            )
            .is_empty()
        );
    }

    #[test]
    fn mongo_shell_commands_are_validated_by_the_shell_parser_not_sqlparser() {
        let schema = FakeSchema::default();
        assert!(
            validate(
                "db.users.find({status: 'active'})",
                DatabaseDriver::MongoDB,
                None,
                &schema,
            )
            .is_empty()
        );

        let diagnostics = validate("db.eval()", DatabaseDriver::MongoDB, None, &schema);
        assert_eq!(diagnostics.len(), 1);
        assert!(
            diagnostics[0]
                .message
                .contains("Unsupported mongo shell command")
        );

        // A plain SQL statement is not a mongo shell command either.
        let diagnostics = validate(
            "SELECT * FROM users",
            DatabaseDriver::MongoDB,
            None,
            &schema,
        );
        assert_eq!(diagnostics.len(), 1);
    }

    #[test]
    fn real_production_query_with_full_schema_cache_has_zero_diagnostics() {
        let text = "
            INSERT INTO ec_fmedia.quotes_pair_translate (pair_ID, lang_id, shortname, pair_name_export_trans, name, nickname, synonym, pair_name_autonews)
            SELECT qpa.pair_id,
                   q1.lang_id,
                   CONCAT(q1.currency_short_name, '/', q2.currency_short_name),
                   CONCAT(q1.fullname, ' ', q2.fullname),
                   CONCAT(q1.fullname, ' ', q2.fullname),
                   '', '', ''
            FROM (SELECT qdt.currency_ID, qca.currency_short_name, qdt.lang_id, GROUP_CONCAT(qdt.fullname SEPARATOR ' ') AS fullname
                  FROM ec_fmedia.quotes_currency_attr qca
                  LEFT JOIN ec_fmedia.quotes_currency_dat_trans qdt ON qca.currency_ID = qdt.currency_ID
                  WHERE qca.currency_ID IN (SELECT cur1 FROM ec_fmedia.quotes_pair_attr WHERE pair_id IN (SELECT pair_id FROM ec_fmedia.quotes_pair_attr WHERE pair_type IN ('currency')))
                  GROUP BY qdt.lang_id, qdt.currency_ID) q1
            JOIN (SELECT qdt.currency_ID, qca.currency_short_name, qdt.lang_id, GROUP_CONCAT(qdt.fullname SEPARATOR ' ') AS fullname
                  FROM ec_fmedia.quotes_currency_attr qca
                  LEFT JOIN ec_fmedia.quotes_currency_dat_trans qdt ON qca.currency_ID = qdt.currency_ID
                  WHERE qca.currency_ID IN (SELECT cur2 FROM ec_fmedia.quotes_pair_attr WHERE pair_id IN (SELECT pair_id FROM ec_fmedia.quotes_pair_attr WHERE pair_type IN ('currency')))
                  GROUP BY qdt.lang_id, qdt.currency_ID) q2
                 ON q1.lang_id = q2.lang_id
            JOIN ec_fmedia.quotes_pair_attr qpa ON q1.currency_ID = qpa.cur1 AND q2.currency_ID = qpa.cur2
            WHERE qpa.pair_id IN (SELECT pair_id FROM ec_fmedia.quotes_pair_attr WHERE pair_type IN ('currency'))
            ORDER BY pair_ID, lang_ID
            ON DUPLICATE KEY UPDATE shortname = VALUES(shortname),
                                    pair_name_export_trans = VALUES(pair_name_export_trans),
                                    name = VALUES(pair_name_export_trans)
        ";
        let schema = FakeSchema::default()
            .and_table(
                "ec_fmedia",
                "quotes_pair_translate",
                &[
                    "pair_ID",
                    "lang_id",
                    "shortname",
                    "pair_name_export_trans",
                    "name",
                    "nickname",
                    "synonym",
                    "pair_name_autonews",
                ],
            )
            .and_table(
                "ec_fmedia",
                "quotes_currency_attr",
                &["currency_ID", "currency_short_name"],
            )
            .and_table(
                "ec_fmedia",
                "quotes_currency_dat_trans",
                &["currency_ID", "lang_id", "fullname"],
            )
            .and_table(
                "ec_fmedia",
                "quotes_pair_attr",
                &["pair_id", "cur1", "cur2", "pair_type"],
            );
        let diagnostics = validate(text, DatabaseDriver::MySQL, None, &schema);
        assert!(
            diagnostics.is_empty(),
            "expected zero diagnostics on a valid real-shaped query, got {diagnostics:?}"
        );
    }
}
