use std::collections::{HashMap, HashSet};
use std::ops::{ControlFlow, Range};

use sqlparser::ast::{Expr, Query, SetExpr, Spanned, Statement, TableFactor, Visit, Visitor};
use sqlparser::dialect::{
    BigQueryDialect, ClickHouseDialect, Dialect, HiveDialect, MySqlDialect, SnowflakeDialect,
};
use sqlparser::parser::{Parser, ParserError};
use sqlparser::tokenizer::{Location, Span, Token, Tokenizer};

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

/// A piece of a statement that can run on its own: a subquery, a CTE, a branch
/// of a `UNION`, the `SELECT` of an `INSERT`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NestedQuery {
    /// Where it is in the text it was read from.
    pub(crate) range: Range<usize>,
    pub(crate) label: String,
    /// It refers to a table of the query around it, so on its own the server
    /// would not know the name.
    pub(crate) correlated: bool,
}

/// The pieces of the statement `text` that hold `cursor`, the innermost first,
/// the whole statement left out. Empty when the cursor is in none of them, when
/// the text is not one statement that the grammar reads, and when there is no
/// grammar.
pub(crate) fn nested_queries_at(
    text: &str,
    cursor: usize,
    dialect: Option<&dyn Dialect>,
) -> Vec<NestedQuery> {
    let Some(dialect) = dialect else {
        return Vec::new();
    };
    read_nested_queries(text, dialect)
        .unwrap_or_default()
        .into_iter()
        .filter(|nested| nested.range.start <= cursor && cursor <= nested.range.end)
        .collect()
}

fn read_nested_queries(text: &str, dialect: &dyn Dialect) -> Option<Vec<NestedQuery>> {
    let tokens = Tokenizer::new(dialect, text)
        .tokenize_with_location()
        .ok()?;
    let index = LocationIndex::new(text);
    let code: Vec<(Token, Range<usize>)> = tokens
        .iter()
        .filter(|token| !matches!(token.token, Token::Whitespace(_)))
        .filter_map(|token| {
            let range = index.offset(token.span.start)?..index.offset(token.span.end)?;
            Some((token.token.clone(), range))
        })
        .collect();
    let whole = code.first()?.1.start..code.last()?.1.end;
    let mut parser = Parser::new(dialect).with_tokens_with_locations(tokens);
    let statement = parser.parse_statement().ok()?;
    if parser.peek_token().token != Token::EOF {
        return None;
    }
    let mut collector = NestedQueryCollector::default();
    let _ = statement.visit(&mut collector);

    let mut found: Vec<(Span, String, bool, u8)> = Vec::new();
    for query in &collector.queries {
        let label = collector
            .names
            .get(&query.span)
            .cloned()
            .unwrap_or_else(|| "Subquery".to_string());
        let priority = if collector.names.contains_key(&query.span) {
            2
        } else {
            0
        };
        found.push((query.span, label, query.correlated, priority));
    }
    for branch in &collector.branches {
        let correlated = collector
            .queries
            .get(branch.owner)
            .is_some_and(|owner| owner.correlated);
        let label = format!("Branch {} of UNION", branch.position);
        found.push((branch.span, label, correlated, 1));
    }
    found.sort_by_key(|(_, _, _, priority)| std::cmp::Reverse(*priority));

    let mut nested: Vec<NestedQuery> = Vec::new();
    for (span, label, correlated, _) in found {
        let (Some(start), Some(end)) = (index.offset(span.start), index.offset(span.end)) else {
            continue;
        };
        let range = start..with_its_closing_parentheses(&code, start, end);
        if range.is_empty()
            || range == whole
            || nested.iter().any(|existing| existing.range == range)
        {
            continue;
        }
        nested.push(NestedQuery {
            range,
            label,
            correlated,
        });
    }
    nested.sort_by_key(|nested| (nested.range.len(), std::cmp::Reverse(nested.range.start)));
    Some(nested)
}

/// `end` moved forward over the `)` that close the `(` the range opened. The
/// span the parser reports for a query stops before the `)` of a subquery it
/// ends with, which would hand the server a query with an open parenthesis.
fn with_its_closing_parentheses(code: &[(Token, Range<usize>)], start: usize, end: usize) -> usize {
    let mut end = end;
    let mut open = 0isize;
    for (token, range) in code {
        if range.start < start {
            continue;
        }
        if range.end > end && open <= 0 {
            break;
        }
        match token {
            Token::LParen => open += 1,
            Token::RParen => open -= 1,
            _ => {}
        }
        end = end.max(range.end);
    }
    end
}

/// Turns the line and column the parser reports into a byte offset, without
/// reading the text from the top each time.
struct LocationIndex<'a> {
    text: &'a str,
    line_starts: Vec<usize>,
}

impl<'a> LocationIndex<'a> {
    fn new(text: &'a str) -> Self {
        let line_starts = std::iter::once(0)
            .chain(text.match_indices('\n').map(|(index, _)| index + 1))
            .collect();
        Self { text, line_starts }
    }

    fn offset(&self, location: Location) -> Option<usize> {
        let line = usize::try_from(location.line).ok()?.checked_sub(1)?;
        let start = *self.line_starts.get(line)?;
        let rest = self.text.get(start..)?;
        let line_text = &rest[..rest.find('\n').unwrap_or(rest.len())];
        let column = usize::try_from(location.column).ok()?.saturating_sub(1);
        Some(
            line_text
                .char_indices()
                .nth(column)
                .map_or(start + line_text.len(), |(index, _)| start + index),
        )
    }
}

#[derive(Default)]
struct QueryFrame {
    index: usize,
    defined: HashSet<String>,
    used: HashSet<String>,
    free_below: HashSet<String>,
}

struct QueryFound {
    span: Span,
    correlated: bool,
}

struct UnionBranch {
    span: Span,
    owner: usize,
    position: usize,
}

/// Walks a statement and records every query in it, with what it is called and
/// whether it needs a table that only the query around it has. A qualifier such
/// as the `a` of `a.id` is free in a query when no `FROM` of that query or of
/// the ones inside it names it; a query with a free qualifier is correlated.
#[derive(Default)]
struct NestedQueryCollector {
    frames: Vec<QueryFrame>,
    queries: Vec<QueryFound>,
    names: HashMap<Span, String>,
    branches: Vec<UnionBranch>,
}

fn collect_union_leaves<'a>(body: &'a SetExpr, leaves: &mut Vec<&'a SetExpr>) {
    match body {
        SetExpr::SetOperation { left, right, .. } => {
            collect_union_leaves(left, leaves);
            collect_union_leaves(right, leaves);
        }
        leaf => leaves.push(leaf),
    }
}

impl Visitor for NestedQueryCollector {
    type Break = ();

    fn pre_visit_statement(&mut self, statement: &Statement) -> ControlFlow<()> {
        let source = match statement {
            Statement::Insert(insert) => insert
                .source
                .as_deref()
                .map(|query| (query, "SELECT of INSERT")),
            Statement::CreateTable(create) => create
                .query
                .as_deref()
                .map(|query| (query, "SELECT of CREATE TABLE")),
            Statement::CreateView(create) => Some((&*create.query, "SELECT of CREATE VIEW")),
            _ => None,
        };
        if let Some((query, label)) = source {
            self.names.insert(query.span(), label.to_string());
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_table_factor(&mut self, factor: &TableFactor) -> ControlFlow<()> {
        let defined = match factor {
            TableFactor::Table { name, alias, .. } => alias
                .as_ref()
                .map(|alias| alias.name.value.clone())
                .or_else(|| {
                    name.0
                        .last()
                        .and_then(|part| part.as_ident())
                        .map(|ident| ident.value.clone())
                }),
            TableFactor::Derived {
                subquery, alias, ..
            } => {
                let label = match alias {
                    Some(alias) => format!("Derived table `{}`", alias.name.value),
                    None => "Derived table".to_string(),
                };
                self.names.insert(subquery.span(), label);
                alias.as_ref().map(|alias| alias.name.value.clone())
            }
            _ => None,
        };
        if let (Some(defined), Some(frame)) = (defined, self.frames.last_mut()) {
            frame.defined.insert(defined.to_lowercase());
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<()> {
        if let (Expr::CompoundIdentifier(parts), Some(frame)) = (expr, self.frames.last_mut())
            && let Some(qualifier) = parts.len().checked_sub(2).and_then(|at| parts.get(at))
        {
            frame.used.insert(qualifier.value.to_lowercase());
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<()> {
        let index = self.queries.len();
        let mut frame = QueryFrame {
            index,
            ..QueryFrame::default()
        };
        if let Some(with) = &query.with {
            for cte in &with.cte_tables {
                frame.defined.insert(cte.alias.name.value.to_lowercase());
                self.names
                    .insert(cte.query.span(), format!("CTE `{}`", cte.alias.name.value));
            }
        }
        let mut leaves = Vec::new();
        collect_union_leaves(&query.body, &mut leaves);
        if leaves.len() > 1 {
            for (position, leaf) in leaves.into_iter().enumerate() {
                self.branches.push(UnionBranch {
                    span: leaf.span(),
                    owner: index,
                    position: position + 1,
                });
            }
        }
        self.queries.push(QueryFound {
            span: query.span(),
            correlated: false,
        });
        self.frames.push(frame);
        ControlFlow::Continue(())
    }

    fn post_visit_query(&mut self, _query: &Query) -> ControlFlow<()> {
        let Some(frame) = self.frames.pop() else {
            return ControlFlow::Continue(());
        };
        let free: HashSet<String> = frame
            .used
            .union(&frame.free_below)
            .filter(|qualifier| !frame.defined.contains(*qualifier))
            .cloned()
            .collect();
        if let Some(found) = self.queries.get_mut(frame.index) {
            found.correlated = !free.is_empty();
        }
        if let Some(parent) = self.frames.last_mut() {
            parent.free_below.extend(free);
        }
        ControlFlow::Continue(())
    }
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

    fn nested(text: &str, cursor_at: &str) -> Vec<(String, String, bool)> {
        let cursor = text
            .find(cursor_at)
            .expect("the cursor marker is in the text");
        nested_queries_at(text, cursor, Some(&MySqlDialect {}))
            .into_iter()
            .map(|nested| {
                (
                    nested.label,
                    text[nested.range].to_string(),
                    nested.correlated,
                )
            })
            .collect()
    }

    #[test]
    fn a_subquery_under_the_cursor_is_offered_and_the_rest_of_the_statement_is_not() {
        let text = "SELECT * FROM a WHERE id IN (SELECT id FROM b WHERE x = 1)";
        assert_eq!(
            nested(text, "id FROM b"),
            [(
                "Subquery".to_string(),
                "SELECT id FROM b WHERE x = 1".to_string(),
                false
            )]
        );
        assert!(nested(text, "a WHERE").is_empty());
        assert!(nested(text, "* FROM").is_empty());
    }

    #[test]
    fn the_innermost_query_comes_first() {
        let text = "SELECT * FROM a WHERE id IN (SELECT id FROM b WHERE k IN (SELECT k FROM c))";
        let found = nested(text, "k FROM c");
        let queries: Vec<&str> = found.iter().map(|(_, query, _)| query.as_str()).collect();
        assert_eq!(
            queries,
            [
                "SELECT k FROM c",
                "SELECT id FROM b WHERE k IN (SELECT k FROM c)"
            ]
        );
    }

    #[test]
    fn a_cte_a_derived_table_and_a_union_branch_are_named() {
        let cte = "WITH totals AS (SELECT a, SUM(b) AS s FROM t GROUP BY a) SELECT * FROM totals";
        assert_eq!(
            nested(cte, "SUM(b)")[0].0,
            "CTE `totals`",
            "{:?}",
            nested(cte, "SUM(b)")
        );
        let derived = "SELECT * FROM (SELECT a FROM t) q WHERE q.a > 1";
        assert_eq!(nested(derived, "a FROM t")[0].0, "Derived table `q`");
        let union = "SELECT a FROM t1 UNION ALL SELECT a FROM t2";
        assert_eq!(
            nested(union, "t2"),
            [(
                "Branch 2 of UNION".to_string(),
                "SELECT a FROM t2".to_string(),
                false
            )]
        );
    }

    #[test]
    fn the_select_of_an_insert_or_a_view_is_offered() {
        let insert = "INSERT INTO t (a) SELECT a FROM u WHERE a > 1";
        assert_eq!(
            nested(insert, "a FROM u"),
            [(
                "SELECT of INSERT".to_string(),
                "SELECT a FROM u WHERE a > 1".to_string(),
                false
            )]
        );
        assert!(nested(insert, "(a)").is_empty());
        let view = "CREATE VIEW v AS SELECT a FROM u";
        assert_eq!(nested(view, "a FROM u")[0].0, "SELECT of CREATE VIEW");
    }

    #[test]
    fn a_subquery_that_uses_a_table_of_the_query_around_it_is_marked() {
        let text = "SELECT * FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.a_id = a.id)";
        assert!(nested(text, "1 FROM b")[0].2, "it needs `a`");
        let alone = "SELECT * FROM a WHERE id IN (SELECT b.id FROM b WHERE b.ok = 1)";
        assert!(!nested(alone, "b.id")[0].2, "it needs nothing of `a`");
        let aliased = "SELECT * FROM a x WHERE EXISTS (SELECT 1 FROM b y WHERE y.a_id = x.id)";
        assert!(nested(aliased, "1 FROM b")[0].2);
    }

    #[test]
    fn a_reference_two_levels_out_marks_every_query_in_between() {
        let text = "SELECT * FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.x IN (SELECT c.x FROM c WHERE c.a_id = a.id))";
        let found = nested(text, "c.x FROM c");
        assert_eq!(found.len(), 2);
        assert!(found[0].2, "the inner one needs `a`");
        assert!(found[1].2, "so the one around it cannot run alone either");
    }

    #[test]
    fn a_name_the_subquery_defines_for_itself_is_not_a_free_reference() {
        let text = "SELECT * FROM a WHERE id IN (SELECT t.id FROM (SELECT id FROM b) t)";
        assert!(!nested(text, "t.id")[0].2);
        let cte = "WITH c AS (SELECT 1 AS n) SELECT * FROM c WHERE c.n IN (SELECT c.n FROM c)";
        assert!(!nested(cte, "c.n FROM")[0].2);
    }

    #[test]
    fn every_piece_offered_reads_as_a_statement_of_its_own() {
        let dialect = MySqlDialect {};
        for (text, marker) in [
            (
                "SELECT * FROM a WHERE id IN (SELECT id FROM b WHERE k IN (SELECT k FROM c))",
                "k FROM c",
            ),
            (
                "WITH x AS (SELECT a FROM t WHERE a IN (SELECT 1)) SELECT * FROM x",
                "SELECT 1",
            ),
            (
                "SELECT * FROM (SELECT a FROM (SELECT a FROM t) u) v",
                "a FROM t",
            ),
            (
                "INSERT INTO t (a) SELECT a FROM u WHERE a IN (SELECT 1 UNION SELECT 2)",
                "SELECT 2",
            ),
            (
                "SELECT a FROM t WHERE a IN (SELECT 1) UNION SELECT a FROM u",
                "SELECT a FROM t",
            ),
        ] {
            let cursor = text.rfind(marker).expect("marker");
            let pieces = nested_queries_at(text, cursor, Some(&dialect));
            assert!(!pieces.is_empty(), "{text:?} at {marker:?}");
            for piece in pieces {
                let sql = &text[piece.range.clone()];
                let tokens = Tokenizer::new(&dialect, sql)
                    .tokenize_with_location()
                    .expect("tokens");
                let mut parser = Parser::new(&dialect).with_tokens_with_locations(tokens);
                parser
                    .parse_statement()
                    .unwrap_or_else(|error| panic!("{sql:?} from {text:?}: {error}"));
                assert_eq!(
                    parser.peek_token().token,
                    Token::EOF,
                    "{sql:?} is one statement"
                );
            }
        }
    }

    #[test]
    fn comments_around_the_statement_do_not_make_it_a_nested_query_of_itself() {
        let text = "-- a note\nSELECT a FROM t -- trailing\n";
        assert!(nested(text, "a FROM").is_empty());
    }

    #[test]
    fn nothing_is_offered_for_text_the_grammar_does_not_read() {
        assert!(nested("SELECT * FROM a WHERE id IN (SELECT id FROM", "id FROM").is_empty());
        assert!(
            nested_queries_at("SELECT 1 UNION SELECT 2", 20, None).is_empty(),
            "no grammar, no pieces"
        );
        assert!(nested("SELECT 1; SELECT 2", "SELECT 2").is_empty());
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
