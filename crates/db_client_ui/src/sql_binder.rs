#![allow(dead_code)]

use std::ops::{ControlFlow, Range};

use collections::HashMap;
use db_client::DatabaseDriver;
use sqlparser::ast::{
    Assignment, AssignmentTarget, Delete, Expr, FromTable, Ident, Insert, JoinConstraint,
    JoinOperator, ObjectName, ObjectNamePart, OnInsert, Query, Select, SelectItem, SetExpr,
    Spanned, Statement, TableFactor, TableObject, TableWithJoins, Update, UpdateTableFromKind,
    Visit, Visitor, With, visit_expressions,
};
use sqlparser::tokenizer::{Location, Span};

use crate::sql_ast;

/// Read-only access to the cached table/column schema, so the binder never
/// needs a live connection -- exactly like the heuristic scanner it sits
/// alongside. Implemented for the real schema cache in `store.rs`; tests use
/// a small in-memory fixture instead.
pub(crate) trait SchemaLookup {
    fn table_has_column(&self, database: &str, table: &str, column: &str) -> bool;
    /// Whether `database`'s table list has actually been fetched into the
    /// cache -- distinguishes a genuinely unknown table from a database the
    /// cache simply hasn't looked at yet, so the validator never flags the
    /// latter as an error.
    fn has_schema_for_database(&self, database: &str) -> bool;
    /// Whether `table` is a real, cached table of `database`. Only
    /// meaningful when `has_schema_for_database` is true.
    fn table_exists(&self, database: &str, table: &str) -> bool;
    /// Whether `(database, table)`'s column list has actually been fetched
    /// into the cache -- distinguishes a table that genuinely lacks a column
    /// from one whose columns simply haven't been expanded yet.
    fn has_columns_for_table(&self, database: &str, table: &str) -> bool;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NavigationTarget {
    Table {
        database: Option<String>,
        table: String,
        span: Span,
    },
    Database {
        database: String,
        span: Span,
    },
    Column {
        database: Option<String>,
        table: String,
        column: String,
        span: Span,
    },
}

/// Parses the statement under `cursor_offset` and resolves the token there
/// against `schema` using a real AST scope-stack. Returns `None` when the
/// statement doesn't fully parse (most commonly because it is still being
/// typed) or the token doesn't resolve to a concrete table/column -- either
/// case is the caller's signal to fall back to the heuristic offset scanner.
///
/// On success, the returned range is the clicked token's absolute byte range
/// within `text` (not the statement-local coordinates the AST spans use).
pub(crate) fn resolve_navigation_at(
    text: &str,
    driver: DatabaseDriver,
    cursor_offset: usize,
    default_database: Option<&str>,
    schema: &dyn SchemaLookup,
) -> Option<(NavigationTarget, Range<usize>)> {
    let (statement, parsed_range) =
        sql_ast::try_parse_statement_at_with_span(text, driver, cursor_offset)?;
    let statement_text = text.get(parsed_range.clone())?;
    let local_offset = cursor_offset
        .saturating_sub(parsed_range.start)
        .min(statement_text.len());
    let target = location_for_offset(statement_text, local_offset);
    let ctx = BindCtx {
        schema,
        default_database,
    };
    let hit = resolve_navigation(&statement, target, &ctx)?;
    let span = hit_span(&hit);
    let start = parsed_range.start + offset_for_location(statement_text, span.start)?;
    let end = parsed_range.start + offset_for_location(statement_text, span.end)?;
    Some((hit, start..end))
}

fn hit_span(target: &NavigationTarget) -> Span {
    match target {
        NavigationTarget::Table { span, .. } => *span,
        NavigationTarget::Database { span, .. } => *span,
        NavigationTarget::Column { span, .. } => *span,
    }
}

pub(crate) fn resolve_navigation(
    statement: &Statement,
    target: Location,
    ctx: &BindCtx,
) -> Option<NavigationTarget> {
    match statement {
        Statement::Query(query) => bind_query(query, &HashMap::default(), target, ctx),
        Statement::Insert(insert) => bind_insert(insert, target, ctx),
        Statement::Update(update) => bind_update(update, target, ctx),
        Statement::Delete(delete) => bind_delete(delete, target, ctx),
        _ => None,
    }
}

pub(crate) struct BindCtx<'a> {
    pub(crate) schema: &'a dyn SchemaLookup,
    pub(crate) default_database: Option<&'a str>,
}

/// Converts a byte offset within `text` into the 1-based line/column
/// [`Location`] sqlparser's tokenizer assigns at that position (advancing
/// once per `char`, resetting the column on `\n`, matching the tokenizer's
/// own bookkeeping) so an AST node's span can be compared to a cursor offset
/// without re-lexing the statement.
fn location_for_offset(text: &str, offset: usize) -> Location {
    let mut line = 1u64;
    let mut column = 1u64;
    for ch in text[..offset.min(text.len())].chars() {
        if ch == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    Location { line, column }
}

/// The inverse of [`location_for_offset`]: the byte offset within `text` at
/// the given 1-based line/column, or `None` when the location falls outside
/// `text` (which would indicate a span from a different piece of text).
pub(crate) fn offset_for_location(text: &str, location: Location) -> Option<usize> {
    if location.line == 0 {
        return None;
    }
    let mut line = 1u64;
    let mut byte_offset = 0usize;
    let mut chars = text.char_indices();
    while line < location.line {
        let (index, ch) = chars.next()?;
        byte_offset = index + ch.len_utf8();
        if ch == '\n' {
            line += 1;
        }
    }
    let mut column = 1u64;
    while column < location.column {
        let Some((index, ch)) = chars.next() else {
            return Some(byte_offset);
        };
        if ch == '\n' {
            return Some(index);
        }
        byte_offset = index + ch.len_utf8();
        column += 1;
    }
    Some(byte_offset)
}

fn span_contains(span: Span, target: Location) -> bool {
    span.start <= target && target <= span.end
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TableBinding {
    Real {
        database: Option<String>,
        table: String,
    },
    /// A CTE or derived table.
    Derived {
        /// Its own projected columns, mapped (lowercased projected name) to
        /// the real source they resolve through to, for navigation's
        /// pass-through. A computed projection (e.g. an aggregate) maps to
        /// the first real column referenced inside it, mirroring the
        /// heuristic scanner's fallback; a projection with no traceable
        /// source at all (a bare unqualified identifier, a literal) is
        /// absent here even though it may still be a real, referenceable
        /// name -- see `names`.
        projections: HashMap<String, (Option<String>, String, String)>,
        /// Every name this alias actually exposes -- what a real query can
        /// reference through it -- regardless of whether navigation could
        /// trace it to a real source. Completion needs this full list;
        /// navigation only ever consults `projections`.
        names: Vec<String>,
    },
}

fn object_name_parts(name: &ObjectName) -> Vec<String> {
    name.0
        .iter()
        .map(|part| match part {
            ObjectNamePart::Identifier(ident) => ident.value.clone(),
            ObjectNamePart::Function(function) => function.name.value.clone(),
        })
        .collect()
}

/// The last two dot-separated parts of `name` as (database, table); a single
/// part is just the table, with no database qualifier.
pub(crate) fn database_and_table(name: &ObjectName) -> (Option<String>, String) {
    let parts = object_name_parts(name);
    match parts.len() {
        0 => (None, String::new()),
        1 => (None, parts[0].clone()),
        len => (Some(parts[len - 2].clone()), parts[len - 1].clone()),
    }
}

enum ObjectNameHit {
    Table {
        database: Option<String>,
        table: String,
        span: Span,
    },
    Database {
        database: String,
        span: Span,
    },
}

/// Checks whether `target` lands on `name`'s table segment (the last part) or
/// its database segment (the second-to-last part, when qualified) -- these
/// resolve to different DDL (a table vs. its owning database), so they must
/// stay distinct rather than collapsing the whole qualified name into one
/// span, matching the heuristic scanner's existing per-segment convention.
fn object_name_click(name: &ObjectName, target: Location) -> Option<ObjectNameHit> {
    let idents: Vec<&Ident> = name
        .0
        .iter()
        .map(|part| match part {
            ObjectNamePart::Identifier(ident) => ident,
            ObjectNamePart::Function(function) => &function.name,
        })
        .collect();
    let last = *idents.last()?;
    if span_contains(last.span, target) {
        let (database, table) = database_and_table(name);
        return Some(ObjectNameHit::Table {
            database,
            table,
            span: last.span,
        });
    }
    if idents.len() >= 2 {
        let database_ident = idents[idents.len() - 2];
        if span_contains(database_ident.span, target) {
            return Some(ObjectNameHit::Database {
                database: database_ident.value.clone(),
                span: database_ident.span,
            });
        }
    }
    None
}

/// Builds bindings for every CTE in `with`, in declaration order, so a later
/// CTE can reference an earlier one, mirroring SQL's own CTE visibility rule.
fn bind_ctes(with: &With, ctx: &BindCtx) -> HashMap<String, TableBinding> {
    let mut ctes = HashMap::default();
    for cte in &with.cte_tables {
        let (projections, names) = select_projections(&cte.query, &ctes, ctx);
        ctes.insert(
            cte.alias.name.value.to_ascii_lowercase(),
            TableBinding::Derived { projections, names },
        );
    }
    ctes
}

/// Resolves `target` inside `query`, including its own CTEs. `ctes` carries
/// bindings visible from an enclosing WITH clause; for a derived-table
/// subquery or a nested WHERE-subquery this is always empty, since neither
/// construct can see CTEs it wasn't itself given -- correlated references to
/// an outer query's own FROM tables are not supported here, matching the
/// heuristic scanner's existing one-scope-per-subquery convention.
fn bind_query(
    query: &Query,
    ctes: &HashMap<String, TableBinding>,
    target: Location,
    ctx: &BindCtx,
) -> Option<NavigationTarget> {
    let local_ctes = match &query.with {
        Some(with) => {
            let mut merged = ctes.clone();
            merged.extend(bind_ctes(with, ctx));
            for cte in &with.cte_tables {
                if span_contains(cte.query.span(), target) {
                    return bind_query(&cte.query, &merged, target, ctx);
                }
            }
            merged
        }
        None => ctes.clone(),
    };
    bind_set_expr(&query.body, &local_ctes, target, ctx)
}

fn bind_set_expr(
    set_expr: &SetExpr,
    ctes: &HashMap<String, TableBinding>,
    target: Location,
    ctx: &BindCtx,
) -> Option<NavigationTarget> {
    match set_expr {
        SetExpr::Select(select) => bind_select(select, ctes, target, ctx),
        SetExpr::Query(query) => bind_query(query, ctes, target, ctx),
        SetExpr::SetOperation { left, right, .. } => bind_set_expr(left, ctes, target, ctx)
            .or_else(|| bind_set_expr(right, ctes, target, ctx)),
        _ => None,
    }
}

/// Binds every `TableWithJoins` in `tables` into `local_tables`, returning
/// early the moment `target` lands on one of them (a table name or alias) --
/// shared by `SELECT`'s `FROM`, `UPDATE`'s target/`FROM`, and `DELETE`'s
/// `FROM`/`USING`, which all scope columns the same way once their tables are
/// known.
fn bind_tables(
    tables: &[TableWithJoins],
    ctes: &HashMap<String, TableBinding>,
    target: Location,
    ctx: &BindCtx,
    local_tables: &mut HashMap<String, TableBinding>,
) -> Option<NavigationTarget> {
    for table_with_joins in tables {
        if let Some(hit) = bind_table_with_joins(table_with_joins, ctes, target, ctx, local_tables)
        {
            return Some(hit);
        }
    }
    None
}

fn bind_select(
    select: &Select,
    ctes: &HashMap<String, TableBinding>,
    target: Location,
    ctx: &BindCtx,
) -> Option<NavigationTarget> {
    let mut local_tables: HashMap<String, TableBinding> = HashMap::default();
    if let Some(hit) = bind_tables(&select.from, ctes, target, ctx, &mut local_tables) {
        return Some(hit);
    }

    for item in &select.projection {
        let expr = match item {
            SelectItem::UnnamedExpr(expr) => Some(expr),
            SelectItem::ExprWithAlias { expr, .. } => Some(expr),
            SelectItem::ExprWithAliases { expr, .. } => Some(expr),
            _ => None,
        };
        if let Some(expr) = expr {
            if let Some(hit) = resolve_expr_at(expr, ctes, &local_tables, target, ctx) {
                return Some(hit);
            }
        }
    }

    if let Some(selection) = &select.selection {
        if let Some(hit) = resolve_expr_at(selection, ctes, &local_tables, target, ctx) {
            return Some(hit);
        }
    }
    if let Some(having) = &select.having {
        if let Some(hit) = resolve_expr_at(having, ctes, &local_tables, target, ctx) {
            return Some(hit);
        }
    }
    for table_with_joins in &select.from {
        for join in &table_with_joins.joins {
            if let Some(constraint_expr) = join_constraint_expr(&join.join_operator) {
                if let Some(hit) =
                    resolve_expr_at(constraint_expr, ctes, &local_tables, target, ctx)
                {
                    return Some(hit);
                }
            }
        }
    }
    None
}

fn join_constraint_expr(operator: &JoinOperator) -> Option<&Expr> {
    let constraint = match operator {
        JoinOperator::Join(constraint)
        | JoinOperator::Inner(constraint)
        | JoinOperator::Left(constraint)
        | JoinOperator::LeftOuter(constraint)
        | JoinOperator::Right(constraint)
        | JoinOperator::RightOuter(constraint)
        | JoinOperator::FullOuter(constraint)
        | JoinOperator::CrossJoin(constraint)
        | JoinOperator::Semi(constraint)
        | JoinOperator::LeftSemi(constraint)
        | JoinOperator::RightSemi(constraint)
        | JoinOperator::Anti(constraint)
        | JoinOperator::LeftAnti(constraint)
        | JoinOperator::RightAnti(constraint)
        | JoinOperator::StraightJoin(constraint) => constraint,
        JoinOperator::AsOf { constraint, .. } => constraint,
        _ => return None,
    };
    match constraint {
        JoinConstraint::On(expr) => Some(expr),
        _ => None,
    }
}

fn bind_table_with_joins(
    table_with_joins: &TableWithJoins,
    ctes: &HashMap<String, TableBinding>,
    target: Location,
    ctx: &BindCtx,
    local_tables: &mut HashMap<String, TableBinding>,
) -> Option<NavigationTarget> {
    if let Some(hit) =
        bind_table_factor(&table_with_joins.relation, ctes, target, ctx, local_tables)
    {
        return Some(hit);
    }
    for join in &table_with_joins.joins {
        if let Some(hit) = bind_table_factor(&join.relation, ctes, target, ctx, local_tables) {
            return Some(hit);
        }
    }
    None
}

fn bind_table_factor(
    factor: &TableFactor,
    ctes: &HashMap<String, TableBinding>,
    target: Location,
    ctx: &BindCtx,
    local_tables: &mut HashMap<String, TableBinding>,
) -> Option<NavigationTarget> {
    match factor {
        TableFactor::Table { name, alias, .. } => {
            let (database, table) = database_and_table(name);
            let key_from_name = table.to_ascii_lowercase();
            let is_cte = database.is_none() && ctes.contains_key(&key_from_name);

            if !is_cte {
                if let Some(hit) = object_name_click(name, target) {
                    return Some(match hit {
                        ObjectNameHit::Table {
                            database,
                            table,
                            span,
                        } => NavigationTarget::Table {
                            database,
                            table,
                            span,
                        },
                        ObjectNameHit::Database { database, span } => {
                            NavigationTarget::Database { database, span }
                        }
                    });
                }
                if let Some(alias) = alias {
                    if span_contains(alias.name.span, target) {
                        return Some(NavigationTarget::Table {
                            database,
                            table,
                            span: alias.name.span,
                        });
                    }
                }
            }

            if let Some((key, binding)) = collect_table_binding(factor, ctes, ctx) {
                local_tables.insert(key, binding);
            }
            None
        }
        TableFactor::Derived { subquery, .. } => {
            if span_contains(subquery.span(), target) {
                return bind_query(subquery, ctes, target, ctx);
            }
            if let Some((key, binding)) = collect_table_binding(factor, ctes, ctx) {
                local_tables.insert(key, binding);
            }
            None
        }
        TableFactor::NestedJoin {
            table_with_joins, ..
        } => bind_table_with_joins(table_with_joins, ctes, target, ctx, local_tables),
        _ => None,
    }
}

/// Builds the (alias-or-name, binding) pair for a `TableFactor` without
/// checking whether `target` falls on it -- used both while scanning for a
/// hit and while building a derived table's/CTE's own projection scope,
/// where no cursor position is relevant at all.
fn collect_table_binding(
    factor: &TableFactor,
    ctes: &HashMap<String, TableBinding>,
    ctx: &BindCtx,
) -> Option<(String, TableBinding)> {
    match factor {
        TableFactor::Table { name, alias, .. } => {
            let (database, table) = database_and_table(name);
            let key_from_name = table.to_ascii_lowercase();
            let binding = if database.is_none() {
                ctes.get(&key_from_name)
                    .cloned()
                    .unwrap_or(TableBinding::Real { database, table })
            } else {
                TableBinding::Real { database, table }
            };
            let alias_key = alias
                .as_ref()
                .map(|alias| alias.name.value.to_ascii_lowercase())
                .unwrap_or(key_from_name);
            Some((alias_key, binding))
        }
        TableFactor::Derived {
            subquery, alias, ..
        } => {
            let alias = alias.as_ref()?;
            let (projections, names) = select_projections(subquery, ctes, ctx);
            Some((
                alias.name.value.to_ascii_lowercase(),
                TableBinding::Derived { projections, names },
            ))
        }
        _ => None,
    }
}

/// Computes both views of `query`'s own SELECT list: a map from each
/// projected column (lowercased) to the real source it resolves through to
/// -- for navigation's pass-through, using only `query`'s own FROM/JOIN
/// tables and skipping anything with no identifiable source column (a
/// literal, `*`, or a pass-through of another derived table one level
/// further down), matching the heuristic scanner's own convention -- and the
/// full list of every name the projection actually exposes, for completion,
/// which has no such requirement: a bare unqualified column or an aliased
/// computed expression is still a real, referenceable name even though
/// navigation has nothing to trace it to.
fn select_projections(
    query: &Query,
    ctes: &HashMap<String, TableBinding>,
    ctx: &BindCtx,
) -> (
    HashMap<String, (Option<String>, String, String)>,
    Vec<String>,
) {
    let mut projections = HashMap::default();
    let mut names = Vec::new();
    let SetExpr::Select(select) = query.body.as_ref() else {
        return (projections, names);
    };

    let mut tables: HashMap<String, TableBinding> = HashMap::default();
    collect_tables_into(&select.from, ctes, ctx, &mut tables);

    for item in &select.projection {
        let (expr, explicit_name) = match item {
            SelectItem::UnnamedExpr(expr) => (Some(expr), None),
            SelectItem::ExprWithAlias { expr, alias } => (Some(expr), Some(alias.value.clone())),
            _ => (None, None),
        };
        let Some(expr) = expr else { continue };

        if let Some(name) = explicit_name.clone().or_else(|| bare_identifier_name(expr)) {
            names.push(name);
        }

        let Some((qualifier, column)) = first_qualified_reference(expr) else {
            continue;
        };
        let Some(TableBinding::Real { database, table }) =
            tables.get(&qualifier.to_ascii_lowercase())
        else {
            continue;
        };
        let projected_name = explicit_name.unwrap_or_else(|| column.clone());
        projections.insert(
            projected_name.to_ascii_lowercase(),
            (database.clone(), table.clone(), column),
        );
    }
    (projections, names)
}

/// The referenceable name of a bare (unaliased) projection expression: the
/// identifier itself for `a`, or the last segment for `t.a` -- a computed
/// expression with no alias (`CONCAT(a, b)`) has no name an outer query could
/// reference, so it yields `None`.
fn bare_identifier_name(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Identifier(ident) => Some(ident.value.clone()),
        Expr::CompoundIdentifier(parts) => parts.last().map(|ident| ident.value.clone()),
        _ => None,
    }
}

/// The first `qualifier.column` reference found anywhere inside `expr`. For a
/// plain pass-through item (`qdt.col`) this is the item itself; for a
/// computed expression (`GROUP_CONCAT(qdt.col SEPARATOR ' ')`) it is the
/// first source column referenced inside it, used as the best-effort
/// navigation target -- mirrors the heuristic scanner's own
/// `first_qualified_reference` fallback for the exact same case.
fn first_qualified_reference(expr: &Expr) -> Option<(String, String)> {
    let mut found: Option<(String, String)> = None;
    let _ = visit_expressions(expr, |candidate| {
        if found.is_none() {
            if let Expr::CompoundIdentifier(parts) = candidate {
                if let [ident, column] = parts.as_slice() {
                    found = Some((ident.value.clone(), column.value.clone()));
                } else if let Some((qualifier, column)) = parts
                    .len()
                    .checked_sub(2)
                    .and_then(|index| parts.get(index))
                    .zip(parts.last())
                {
                    found = Some((qualifier.value.clone(), column.value.clone()));
                }
            }
        }
        ControlFlow::<()>::Continue(())
    });
    found
}

enum ColumnHit {
    /// The cursor sits on a qualifier segment of a compound identifier
    /// (`alias` in `alias.col`), not the column itself -- resolves to
    /// nothing here, mirroring the heuristic scanner's own convention of
    /// leaving qualifier clicks to table/database navigation instead.
    Qualifier,
    Column {
        qualifier: Option<String>,
        column: String,
        span: Span,
    },
}

/// Depth-first search for the innermost embedded [`Query`] containing
/// `target` and, failing that, the specific column token containing it.
/// These are mutually exclusive by construction: if `target` sits inside a
/// nested query embedded in `expr` (e.g. `WHERE x IN (SELECT ...)`), every
/// identifier's span inside that nested query is itself contained by the
/// nested query's own span, so it can never *also* satisfy `target` unless
/// the nested query itself does.
struct ExprLocator {
    target: Location,
    nested_query: Option<Query>,
    column_hit: Option<ColumnHit>,
}

impl Visitor for ExprLocator {
    type Break = ();

    fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<Self::Break> {
        if span_contains(query.span(), self.target) {
            self.nested_query = Some(query.clone());
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<Self::Break> {
        match expr {
            Expr::Identifier(ident) => {
                if span_contains(ident.span, self.target) {
                    self.column_hit = Some(ColumnHit::Column {
                        qualifier: None,
                        column: ident.value.clone(),
                        span: ident.span,
                    });
                }
            }
            Expr::CompoundIdentifier(parts) => {
                for (index, part) in parts.iter().enumerate() {
                    if span_contains(part.span, self.target) {
                        self.column_hit = Some(if index + 1 == parts.len() {
                            ColumnHit::Column {
                                qualifier: parts
                                    .len()
                                    .checked_sub(2)
                                    .and_then(|qualifier_index| parts.get(qualifier_index))
                                    .map(|ident: &Ident| ident.value.clone()),
                                column: part.value.clone(),
                                span: part.span,
                            }
                        } else {
                            ColumnHit::Qualifier
                        });
                        break;
                    }
                }
            }
            _ => {}
        }
        ControlFlow::Continue(())
    }
}

fn resolve_expr_at(
    expr: &Expr,
    ctes: &HashMap<String, TableBinding>,
    local_tables: &HashMap<String, TableBinding>,
    target: Location,
    ctx: &BindCtx,
) -> Option<NavigationTarget> {
    let mut locator = ExprLocator {
        target,
        nested_query: None,
        column_hit: None,
    };
    let _ = expr.visit(&mut locator);

    if let Some(nested) = locator.nested_query {
        return bind_query(&nested, ctes, target, ctx);
    }
    match locator.column_hit? {
        ColumnHit::Qualifier => None,
        ColumnHit::Column {
            qualifier,
            column,
            span,
        } => resolve_column(qualifier.as_deref(), &column, span, local_tables, ctx),
    }
}

fn resolve_column(
    qualifier: Option<&str>,
    column: &str,
    span: Span,
    local_tables: &HashMap<String, TableBinding>,
    ctx: &BindCtx,
) -> Option<NavigationTarget> {
    match qualifier {
        Some(qualifier) => {
            let binding = local_tables.get(&qualifier.to_ascii_lowercase())?;
            match binding {
                TableBinding::Real { database, table } => Some(NavigationTarget::Column {
                    database: database.clone(),
                    table: table.clone(),
                    column: column.to_string(),
                    span,
                }),
                TableBinding::Derived { projections, .. } => {
                    let (database, table, real_column) =
                        projections.get(&column.to_ascii_lowercase())?;
                    Some(NavigationTarget::Column {
                        database: database.clone(),
                        table: table.clone(),
                        column: real_column.clone(),
                        span,
                    })
                }
            }
        }
        None => {
            let mut owners: Vec<NavigationTarget> = Vec::new();
            for binding in local_tables.values() {
                match binding {
                    TableBinding::Real { database, table } => {
                        let resolved_database = database
                            .clone()
                            .or_else(|| ctx.default_database.map(str::to_string));
                        if let Some(resolved_database) = resolved_database {
                            if ctx
                                .schema
                                .table_has_column(&resolved_database, table, column)
                            {
                                owners.push(NavigationTarget::Column {
                                    database: database.clone(),
                                    table: table.clone(),
                                    column: column.to_string(),
                                    span,
                                });
                            }
                        }
                    }
                    TableBinding::Derived { projections, .. } => {
                        if let Some((database, table, real_column)) =
                            projections.get(&column.to_ascii_lowercase())
                        {
                            owners.push(NavigationTarget::Column {
                                database: database.clone(),
                                table: table.clone(),
                                column: real_column.clone(),
                                span,
                            });
                        }
                    }
                }
            }
            if owners.len() == 1 {
                owners.pop()
            } else {
                None
            }
        }
    }
}

/// Resolves the INSERT target table name, a column in its explicit
/// `(col, col, ...)` list, the `SELECT`/`VALUES` body that supplies the
/// inserted rows, and a MySQL `ON DUPLICATE KEY UPDATE` clause -- the column
/// list and `ON DUPLICATE KEY UPDATE` always resolve against the INSERT
/// target table, never the trailing `SELECT`'s own `FROM` tables, matching
/// the heuristic scanner's existing convention.
fn bind_insert(insert: &Insert, target: Location, ctx: &BindCtx) -> Option<NavigationTarget> {
    if let TableObject::TableName(name) = &insert.table {
        if let Some(hit) = object_name_click(name, target) {
            return Some(match hit {
                ObjectNameHit::Table {
                    database,
                    table,
                    span,
                } => NavigationTarget::Table {
                    database,
                    table,
                    span,
                },
                ObjectNameHit::Database { database, span } => {
                    NavigationTarget::Database { database, span }
                }
            });
        }
    }
    let (insert_database, insert_table) = match &insert.table {
        TableObject::TableName(name) => database_and_table(name),
        TableObject::TableFunction(_) | TableObject::TableQuery(_) => (None, String::new()),
    };

    for column in &insert.columns {
        if let Some((column_name, span)) = flat_object_name_click(column, target) {
            if insert_table.is_empty() {
                return None;
            }
            return Some(NavigationTarget::Column {
                database: insert_database,
                table: insert_table,
                column: column_name,
                span,
            });
        }
    }

    if let Some(source) = &insert.source {
        if span_contains(source.span(), target) {
            return bind_query(source, &HashMap::default(), target, ctx);
        }
    }

    if let Some(OnInsert::DuplicateKeyUpdate(assignments)) = &insert.on {
        if !insert_table.is_empty() {
            return bind_on_duplicate_key_update(
                assignments,
                target,
                insert_database.as_deref(),
                &insert_table,
            );
        }
    }
    None
}

/// The last identifier segment of a plain (unqualified) name reference, such
/// as an entry in an `INSERT INTO table (col, col, ...)` list -- unlike
/// [`object_name_click`], this never distinguishes a table vs. database
/// segment, since a column-list entry is always resolved against one already
/// -known table.
fn flat_object_name_click(name: &ObjectName, target: Location) -> Option<(String, Span)> {
    let idents: Vec<&Ident> = name
        .0
        .iter()
        .map(|part| match part {
            ObjectNamePart::Identifier(ident) => ident,
            ObjectNamePart::Function(function) => &function.name,
        })
        .collect();
    let last = *idents.last()?;
    span_contains(last.span, target).then(|| (last.value.clone(), last.span))
}

/// A bare (unqualified) `Expr::Identifier` inside an expression -- used for
/// `ON DUPLICATE KEY UPDATE`'s `VALUES(col)` argument, which MySQL resolves
/// against the INSERT target table's column, never a `FROM`-clause table.
/// The `VALUES` function name itself is never visited here: it lives in
/// `Function.name` (an [`ObjectName`]), not as an `Expr` node, so only its
/// argument -- the actual column reference -- can ever match.
struct BareIdentLocator {
    target: Location,
    hit: Option<(String, Span)>,
}

impl Visitor for BareIdentLocator {
    type Break = ();

    fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<Self::Break> {
        if let Expr::Identifier(ident) = expr {
            if span_contains(ident.span, self.target) {
                self.hit = Some((ident.value.clone(), ident.span));
            }
        }
        ControlFlow::Continue(())
    }
}

/// Resolves a click inside a MySQL `ON DUPLICATE KEY UPDATE col = VALUES(col)`
/// clause against the fixed INSERT target table: both the assignment's own
/// target column and a bare column referenced in its value (including inside
/// a `VALUES(...)` call) resolve there, matching the heuristic scanner's
/// existing "any unqualified identifier in this clause" convention -- a
/// qualified reference is not valid MySQL syntax in this position and is
/// left unresolved, same as today.
fn bind_on_duplicate_key_update(
    assignments: &[Assignment],
    target: Location,
    database: Option<&str>,
    table: &str,
) -> Option<NavigationTarget> {
    for assignment in assignments {
        if let AssignmentTarget::ColumnName(name) = &assignment.target {
            if let Some((column, span)) = flat_object_name_click(name, target) {
                return Some(NavigationTarget::Column {
                    database: database.map(str::to_string),
                    table: table.to_string(),
                    column,
                    span,
                });
            }
        }
        let mut locator = BareIdentLocator { target, hit: None };
        let _ = assignment.value.visit(&mut locator);
        if let Some((column, span)) = locator.hit {
            return Some(NavigationTarget::Column {
                database: database.map(str::to_string),
                table: table.to_string(),
                column,
                span,
            });
        }
    }
    None
}

/// Resolves a click on an `ObjectName` used as a column reference (an
/// `UPDATE` assignment's target, which may be qualified with a table alias
/// like `t.col` or bare) the same way a `CompoundIdentifier`/`Identifier`
/// expression already resolves.
fn resolve_object_name_column(
    name: &ObjectName,
    local_tables: &HashMap<String, TableBinding>,
    target: Location,
    ctx: &BindCtx,
) -> Option<NavigationTarget> {
    let idents: Vec<&Ident> = name
        .0
        .iter()
        .map(|part| match part {
            ObjectNamePart::Identifier(ident) => ident,
            ObjectNamePart::Function(function) => &function.name,
        })
        .collect();
    let last = *idents.last()?;
    if !span_contains(last.span, target) {
        return None;
    }
    let qualifier = idents
        .len()
        .checked_sub(2)
        .and_then(|index| idents.get(index))
        .map(|ident| ident.value.as_str());
    resolve_column(qualifier, &last.value, last.span, local_tables, ctx)
}

/// Binds `UPDATE`'s target table (and, when present, its `FROM`/`USING`-style
/// source tables), then resolves a click on an assignment's target/value or
/// the `WHERE` clause through that scope -- the same scope-stack machinery
/// `SELECT` already uses, since an `UPDATE` target is just another table.
fn bind_update(update: &Update, target: Location, ctx: &BindCtx) -> Option<NavigationTarget> {
    let ctes = HashMap::default();
    let mut local_tables: HashMap<String, TableBinding> = HashMap::default();

    if let Some(hit) = bind_tables(
        std::slice::from_ref(&update.table),
        &ctes,
        target,
        ctx,
        &mut local_tables,
    ) {
        return Some(hit);
    }
    if let Some(from) = &update.from {
        let tables = match from {
            UpdateTableFromKind::BeforeSet(tables) | UpdateTableFromKind::AfterSet(tables) => {
                tables
            }
        };
        if let Some(hit) = bind_tables(tables, &ctes, target, ctx, &mut local_tables) {
            return Some(hit);
        }
    }

    for assignment in &update.assignments {
        if let AssignmentTarget::ColumnName(name) = &assignment.target {
            if let Some(hit) = resolve_object_name_column(name, &local_tables, target, ctx) {
                return Some(hit);
            }
        }
        if let Some(hit) = resolve_expr_at(&assignment.value, &ctes, &local_tables, target, ctx) {
            return Some(hit);
        }
    }
    if let Some(selection) = &update.selection {
        if let Some(hit) = resolve_expr_at(selection, &ctes, &local_tables, target, ctx) {
            return Some(hit);
        }
    }
    None
}

/// Binds `DELETE`'s `FROM`/`USING` tables, then resolves a click on the
/// `WHERE` clause through that scope.
fn bind_delete(delete: &Delete, target: Location, ctx: &BindCtx) -> Option<NavigationTarget> {
    let ctes = HashMap::default();
    let mut local_tables: HashMap<String, TableBinding> = HashMap::default();

    let from_tables = match &delete.from {
        FromTable::WithFromKeyword(tables) | FromTable::WithoutKeyword(tables) => tables,
    };
    if let Some(hit) = bind_tables(from_tables, &ctes, target, ctx, &mut local_tables) {
        return Some(hit);
    }
    if let Some(using) = &delete.using {
        if let Some(hit) = bind_tables(using, &ctes, target, ctx, &mut local_tables) {
            return Some(hit);
        }
    }
    if let Some(selection) = &delete.selection {
        if let Some(hit) = resolve_expr_at(selection, &ctes, &local_tables, target, ctx) {
            return Some(hit);
        }
    }
    None
}

/// What a completion-time scope lookup exposes for one alias: either a real
/// table (columns come from the schema cache, keyed by `table`) or a
/// CTE/derived table (columns are exactly its own projected/aliased names --
/// what a real query can actually reference through that alias -- unlike
/// navigation's pass-through to the underlying real column).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ScopeBinding {
    Real {
        database: Option<String>,
        table: String,
    },
    Derived {
        columns: Vec<String>,
    },
}

/// A [`SchemaLookup`] that answers every question with "no" -- scope
/// collection (unlike navigation's bare-column disambiguation) never needs to
/// check the schema cache, but the collection helpers it reuses require a
/// [`BindCtx`] by signature.
struct NoSchema;

impl SchemaLookup for NoSchema {
    fn table_has_column(&self, _database: &str, _table: &str, _column: &str) -> bool {
        false
    }

    fn has_schema_for_database(&self, _database: &str) -> bool {
        false
    }

    fn table_exists(&self, _database: &str, _table: &str) -> bool {
        false
    }

    fn has_columns_for_table(&self, _database: &str, _table: &str) -> bool {
        false
    }
}

/// Parses the statement under `cursor_offset` and returns every table/alias
/// binding visible at that position -- the innermost `SELECT`/`UPDATE`/
/// `DELETE`/`INSERT...SELECT` scope containing the cursor, walking through
/// CTEs and derived-table subqueries the same way navigation does. Returns
/// `None` when the statement doesn't fully parse, signalling the caller to
/// fall back to the heuristic scanner.
pub(crate) fn scope_bindings_at(
    text: &str,
    driver: DatabaseDriver,
    cursor_offset: usize,
) -> Option<Vec<(String, ScopeBinding)>> {
    let (statement, parsed_range) =
        sql_ast::try_parse_statement_at_with_span(text, driver, cursor_offset)?;
    let statement_text = text.get(parsed_range.clone())?;
    let local_offset = cursor_offset
        .saturating_sub(parsed_range.start)
        .min(statement_text.len());
    let target = location_for_offset(statement_text, local_offset);
    let ctx = BindCtx {
        schema: &NoSchema,
        default_database: None,
    };
    let local_tables = scope_tables_in_statement(&statement, target, &ctx)?;
    Some(
        local_tables
            .into_iter()
            .map(|(alias, binding)| (alias, scope_binding_from(binding)))
            .collect(),
    )
}

fn scope_binding_from(binding: TableBinding) -> ScopeBinding {
    match binding {
        TableBinding::Real { database, table } => ScopeBinding::Real { database, table },
        TableBinding::Derived { names, .. } => {
            let mut columns = names;
            columns.sort();
            columns.dedup();
            ScopeBinding::Derived { columns }
        }
    }
}

/// Collects every table/join relation in `tables` into `out`, without
/// checking whether `target` lands on any of them -- the pure-collection
/// counterpart to [`bind_tables`], which stops early on a click hit. Shared
/// by [`select_projections`] and the scope-lookup functions below, both of
/// which need "every table this clause makes visible" rather than a specific
/// click resolution.
fn collect_tables_into(
    tables: &[TableWithJoins],
    ctes: &HashMap<String, TableBinding>,
    ctx: &BindCtx,
    out: &mut HashMap<String, TableBinding>,
) {
    for table_with_joins in tables {
        if let Some((key, binding)) = collect_table_binding(&table_with_joins.relation, ctes, ctx) {
            out.insert(key, binding);
        }
        for join in &table_with_joins.joins {
            if let Some((key, binding)) = collect_table_binding(&join.relation, ctes, ctx) {
                out.insert(key, binding);
            }
        }
    }
}

/// The innermost scope's table bindings for whichever statement shape
/// contains `target`. `INSERT`'s own target-table/column-list scope isn't
/// returned here (a fixed single table has no "completion scope" of its own
/// beyond what the heuristic already offers); only its trailing `SELECT`
/// body, when the cursor is inside it, is.
fn scope_tables_in_statement(
    statement: &Statement,
    target: Location,
    ctx: &BindCtx,
) -> Option<HashMap<String, TableBinding>> {
    match statement {
        Statement::Query(query) => scope_tables_in_query(query, &HashMap::default(), target, ctx),
        Statement::Insert(insert) => {
            let source = insert.source.as_ref()?;
            if span_contains(source.span(), target) {
                scope_tables_in_query(source, &HashMap::default(), target, ctx)
            } else {
                None
            }
        }
        Statement::Update(update) => {
            let ctes = HashMap::default();
            let mut local_tables = HashMap::default();
            collect_tables_into(
                std::slice::from_ref(&update.table),
                &ctes,
                ctx,
                &mut local_tables,
            );
            if let Some(from) = &update.from {
                let tables = match from {
                    UpdateTableFromKind::BeforeSet(tables)
                    | UpdateTableFromKind::AfterSet(tables) => tables,
                };
                collect_tables_into(tables, &ctes, ctx, &mut local_tables);
            }
            Some(local_tables)
        }
        Statement::Delete(delete) => {
            let ctes = HashMap::default();
            let mut local_tables = HashMap::default();
            let from_tables = match &delete.from {
                FromTable::WithFromKeyword(tables) | FromTable::WithoutKeyword(tables) => tables,
            };
            collect_tables_into(from_tables, &ctes, ctx, &mut local_tables);
            if let Some(using) = &delete.using {
                collect_tables_into(using, &ctes, ctx, &mut local_tables);
            }
            Some(local_tables)
        }
        _ => None,
    }
}

fn scope_tables_in_query(
    query: &Query,
    ctes: &HashMap<String, TableBinding>,
    target: Location,
    ctx: &BindCtx,
) -> Option<HashMap<String, TableBinding>> {
    let local_ctes = match &query.with {
        Some(with) => {
            let mut merged = ctes.clone();
            merged.extend(bind_ctes(with, ctx));
            for cte in &with.cte_tables {
                if span_contains(cte.query.span(), target) {
                    return scope_tables_in_query(&cte.query, &merged, target, ctx);
                }
            }
            merged
        }
        None => ctes.clone(),
    };
    scope_tables_in_set_expr(&query.body, &local_ctes, target, ctx)
}

fn scope_tables_in_set_expr(
    set_expr: &SetExpr,
    ctes: &HashMap<String, TableBinding>,
    target: Location,
    ctx: &BindCtx,
) -> Option<HashMap<String, TableBinding>> {
    match set_expr {
        SetExpr::Select(select) => scope_tables_in_select(select, ctes, target, ctx),
        SetExpr::Query(query) => scope_tables_in_query(query, ctes, target, ctx),
        SetExpr::SetOperation { left, right, .. } => {
            scope_tables_in_set_expr(left, ctes, target, ctx)
                .or_else(|| scope_tables_in_set_expr(right, ctes, target, ctx))
        }
        _ => None,
    }
}

/// The innermost scope containing `target`: a deeper `SELECT` embedded in a
/// derived-table subquery or a `WHERE ... IN (subquery)`-style nested query
/// wins over this level's own scope, mirroring the same "deepest match wins"
/// rule navigation's [`resolve_expr_at`]/[`bind_table_factor`] already use.
/// When no deeper scope contains `target`, this level's own `FROM`/`JOIN`
/// tables are the answer -- this is what makes the function usable even when
/// the cursor sits on a not-yet-existing token (an incomplete projection or a
/// trailing `WHERE `), unlike navigation which requires an actual identifier
/// to resolve.
fn scope_tables_in_select(
    select: &Select,
    ctes: &HashMap<String, TableBinding>,
    target: Location,
    ctx: &BindCtx,
) -> Option<HashMap<String, TableBinding>> {
    for table_with_joins in &select.from {
        if let Some(deeper) =
            scope_tables_in_table_factor(&table_with_joins.relation, ctes, target, ctx)
        {
            return Some(deeper);
        }
        for join in &table_with_joins.joins {
            if let Some(deeper) = scope_tables_in_table_factor(&join.relation, ctes, target, ctx) {
                return Some(deeper);
            }
        }
    }

    for item in &select.projection {
        let expr = match item {
            SelectItem::UnnamedExpr(expr) => Some(expr),
            SelectItem::ExprWithAlias { expr, .. } => Some(expr),
            SelectItem::ExprWithAliases { expr, .. } => Some(expr),
            _ => None,
        };
        if let Some(expr) = expr {
            if let Some(deeper) = scope_tables_in_expr(expr, ctes, target, ctx) {
                return Some(deeper);
            }
        }
    }
    if let Some(selection) = &select.selection {
        if let Some(deeper) = scope_tables_in_expr(selection, ctes, target, ctx) {
            return Some(deeper);
        }
    }
    if let Some(having) = &select.having {
        if let Some(deeper) = scope_tables_in_expr(having, ctes, target, ctx) {
            return Some(deeper);
        }
    }
    for table_with_joins in &select.from {
        for join in &table_with_joins.joins {
            if let Some(constraint_expr) = join_constraint_expr(&join.join_operator) {
                if let Some(deeper) = scope_tables_in_expr(constraint_expr, ctes, target, ctx) {
                    return Some(deeper);
                }
            }
        }
    }

    let mut local_tables = HashMap::default();
    collect_tables_into(&select.from, ctes, ctx, &mut local_tables);
    Some(local_tables)
}

fn scope_tables_in_table_factor(
    factor: &TableFactor,
    ctes: &HashMap<String, TableBinding>,
    target: Location,
    ctx: &BindCtx,
) -> Option<HashMap<String, TableBinding>> {
    match factor {
        TableFactor::Derived { subquery, .. } => {
            if span_contains(subquery.span(), target) {
                scope_tables_in_query(subquery, ctes, target, ctx)
            } else {
                None
            }
        }
        TableFactor::NestedJoin {
            table_with_joins, ..
        } => {
            if let Some(deeper) =
                scope_tables_in_table_factor(&table_with_joins.relation, ctes, target, ctx)
            {
                return Some(deeper);
            }
            for join in &table_with_joins.joins {
                if let Some(deeper) =
                    scope_tables_in_table_factor(&join.relation, ctes, target, ctx)
                {
                    return Some(deeper);
                }
            }
            None
        }
        _ => None,
    }
}

fn scope_tables_in_expr(
    expr: &Expr,
    ctes: &HashMap<String, TableBinding>,
    target: Location,
    ctx: &BindCtx,
) -> Option<HashMap<String, TableBinding>> {
    let mut locator = ExprLocator {
        target,
        nested_query: None,
        column_hit: None,
    };
    let _ = expr.visit(&mut locator);
    let nested = locator.nested_query?;
    scope_tables_in_query(&nested, ctes, target, ctx)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mirrors `ActiveConnection`'s two-tier cache: a database's table list
    /// and a table's column list are each either "not fetched" (absent from
    /// the map) or "fetched" (present, possibly empty) -- tests need this
    /// distinction to prove the validator skips databases/tables it has no
    /// data for at all, rather than treating that as an error.
    #[derive(Default)]
    struct FakeSchema {
        tables_by_database: HashMap<String, Vec<String>>,
        columns_by_table: HashMap<(String, String), Vec<String>>,
    }

    impl FakeSchema {
        fn with_table(database: &str, table: &str, columns: &[&str]) -> Self {
            FakeSchema::default().and_table(database, table, columns)
        }

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

        /// Registers `database` as fetched, with no tables in it -- distinct
        /// from `FakeSchema::default()`, which has never fetched anything.
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
    fn simple_qualified_column_resolves_to_the_real_table() {
        let text = "SELECT t.col FROM db.table t";
        let schema = FakeSchema::default();
        let offset = text.rfind("col").expect("marker present");
        let (target, _) = resolve_navigation_at(text, DatabaseDriver::MySQL, offset, None, &schema)
            .expect("column resolves");
        match target {
            NavigationTarget::Column {
                database,
                table,
                column,
                ..
            } => {
                assert_eq!(database.as_deref(), Some("db"));
                assert_eq!(table, "table");
                assert_eq!(column, "col");
            }
            other => panic!("expected a column target, got {other:?}"),
        }
    }

    #[test]
    fn cte_column_resolves_through_to_the_real_source() {
        let text = "WITH cte AS (SELECT rt.id AS col FROM real_table rt) SELECT cte.col FROM cte";
        let schema = FakeSchema::default();
        let offset = text.rfind("col").expect("marker present");
        let (target, _) = resolve_navigation_at(text, DatabaseDriver::MySQL, offset, None, &schema)
            .expect("cte column resolves");
        match target {
            NavigationTarget::Column {
                database,
                table,
                column,
                ..
            } => {
                assert_eq!(database, None);
                assert_eq!(table, "real_table");
                assert_eq!(column, "id");
            }
            other => panic!("expected a column target, got {other:?}"),
        }
    }

    #[test]
    fn derived_table_passthrough_column_resolves_to_the_real_source() {
        let text = "SELECT q1.currency_short_name FROM \
            (SELECT qca.currency_short_name FROM ec_fmedia.quotes_currency_attr qca) q1";
        let schema = FakeSchema::default();
        let offset = text.find("q1.currency_short_name").expect("marker present") + "q1.".len();
        let (target, _) = resolve_navigation_at(text, DatabaseDriver::MySQL, offset, None, &schema)
            .expect("derived column resolves");
        match target {
            NavigationTarget::Column {
                database,
                table,
                column,
                ..
            } => {
                assert_eq!(database.as_deref(), Some("ec_fmedia"));
                assert_eq!(table, "quotes_currency_attr");
                assert_eq!(column, "currency_short_name");
            }
            other => panic!("expected a column target, got {other:?}"),
        }
    }

    #[test]
    fn derived_table_computed_column_resolves_to_first_referenced_real_column() {
        let text = "SELECT q1.fullname FROM \
            (SELECT GROUP_CONCAT(qdt.fullname SEPARATOR ' ') AS fullname \
             FROM ec_fmedia.quotes_currency_dat_trans qdt) q1";
        let schema = FakeSchema::default();
        let offset = text.find("q1.fullname").expect("marker present") + "q1.".len();
        let (target, _) = resolve_navigation_at(text, DatabaseDriver::MySQL, offset, None, &schema)
            .expect("computed derived column resolves via fallback");
        match target {
            NavigationTarget::Column {
                database,
                table,
                column,
                ..
            } => {
                assert_eq!(database.as_deref(), Some("ec_fmedia"));
                assert_eq!(table, "quotes_currency_dat_trans");
                assert_eq!(column, "fullname");
            }
            other => panic!("expected a column target, got {other:?}"),
        }
    }

    #[test]
    fn reused_alias_resolves_independently_in_outer_and_inner_scope() {
        let text = "SELECT outer_s.name FROM accounts AS outer_s WHERE outer_s.id IN \
            (SELECT inner_s.id FROM instruments.splits AS inner_s WHERE inner_s.operation = 1)";
        let schema = FakeSchema::default();

        let inner_offset = text
            .rfind("inner_s.operation")
            .expect("inner marker present")
            + "inner_s.".len();
        let (inner_target, _) =
            resolve_navigation_at(text, DatabaseDriver::MySQL, inner_offset, None, &schema)
                .expect("inner column resolves");
        match inner_target {
            NavigationTarget::Column { table, column, .. } => {
                assert_eq!(table, "splits");
                assert_eq!(column, "operation");
            }
            other => panic!("expected a column target, got {other:?}"),
        }

        let outer_offset =
            text.find("outer_s.name").expect("outer marker present") + "outer_s.".len();
        let (outer_target, _) =
            resolve_navigation_at(text, DatabaseDriver::MySQL, outer_offset, None, &schema)
                .expect("outer column resolves");
        match outer_target {
            NavigationTarget::Column { table, column, .. } => {
                assert_eq!(table, "accounts");
                assert_eq!(column, "name");
            }
            other => panic!("expected a column target, got {other:?}"),
        }
    }

    #[test]
    fn bare_column_resolves_uniquely_against_a_single_from_table() {
        let text = "SELECT flag FROM db.orders WHERE flag = 1";
        let schema = FakeSchema::with_table("db", "orders", &["flag"]);
        let offset = text.find("flag").expect("marker present");
        let (target, _) = resolve_navigation_at(text, DatabaseDriver::MySQL, offset, None, &schema)
            .expect("unique bare column resolves");
        match target {
            NavigationTarget::Column { table, column, .. } => {
                assert_eq!(table, "orders");
                assert_eq!(column, "flag");
            }
            other => panic!("expected a column target, got {other:?}"),
        }
    }

    #[test]
    fn bare_column_stays_unresolved_when_ambiguous_across_from_tables() {
        let text = "SELECT flag FROM db.orders o, db.shipments s WHERE flag = 1";
        let schema = FakeSchema::with_table("db", "orders", &["flag"]).and_table(
            "db",
            "shipments",
            &["flag"],
        );
        let offset = text.find("flag").expect("marker present");
        assert!(
            resolve_navigation_at(text, DatabaseDriver::MySQL, offset, None, &schema).is_none(),
            "an ambiguous bare column must not guess an owner"
        );
    }

    #[test]
    fn malformed_statement_returns_none_so_callers_fall_back_to_the_heuristic_scanner() {
        let text = "SELECT FROM WHERE;";
        let schema = FakeSchema::default();
        assert!(resolve_navigation_at(text, DatabaseDriver::MySQL, 0, None, &schema).is_none());
    }

    #[test]
    fn incomplete_statement_returns_none_so_callers_fall_back_to_the_heuristic_scanner() {
        let text = "SELECT s.op FROM";
        let schema = FakeSchema::default();
        assert!(
            resolve_navigation_at(text, DatabaseDriver::MySQL, text.len(), None, &schema).is_none()
        );
    }

    #[test]
    fn bare_table_name_click_resolves_via_the_binder() {
        let text = "SELECT 1 FROM db.orders o";
        let schema = FakeSchema::default();
        let offset = text.find("orders").expect("marker present");
        let (target, _) = resolve_navigation_at(text, DatabaseDriver::MySQL, offset, None, &schema)
            .expect("table name resolves");
        match target {
            NavigationTarget::Table {
                database, table, ..
            } => {
                assert_eq!(database.as_deref(), Some("db"));
                assert_eq!(table, "orders");
            }
            other => panic!("expected a table target, got {other:?}"),
        }
    }

    #[test]
    fn table_alias_click_resolves_via_the_binder() {
        let text = "SELECT 1 FROM db.orders o";
        let schema = FakeSchema::default();
        let offset = text.rfind(" o").expect("marker present") + 1;
        let (target, _) = resolve_navigation_at(text, DatabaseDriver::MySQL, offset, None, &schema)
            .expect("alias resolves to its own table");
        match target {
            NavigationTarget::Table {
                database, table, ..
            } => {
                assert_eq!(database.as_deref(), Some("db"));
                assert_eq!(table, "orders");
            }
            other => panic!("expected a table target, got {other:?}"),
        }
    }

    #[test]
    fn insert_target_table_click_resolves_via_the_binder() {
        let text = "INSERT INTO db.orders (id) VALUES (1)";
        let schema = FakeSchema::default();
        let offset = text.find("orders").expect("marker present");
        let (target, _) = resolve_navigation_at(text, DatabaseDriver::MySQL, offset, None, &schema)
            .expect("insert target table resolves");
        match target {
            NavigationTarget::Table {
                database, table, ..
            } => {
                assert_eq!(database.as_deref(), Some("db"));
                assert_eq!(table, "orders");
            }
            other => panic!("expected a table target, got {other:?}"),
        }
    }

    #[test]
    fn schema_lookup_never_needs_a_live_connection() {
        // `FakeSchema` has no notion of a connection at all -- resolving a
        // bare column against it proves navigation reads only the in-memory
        // fixture, never anything resembling a live database handle.
        let text = "SELECT flag FROM db.orders";
        let schema = FakeSchema::with_table("db", "orders", &["flag"]);
        let offset = text.find("flag").expect("marker present");
        assert!(
            resolve_navigation_at(text, DatabaseDriver::MySQL, offset, None, &schema).is_some()
        );
    }

    /// A real production-shaped query: `INSERT ... SELECT` with two derived
    /// tables (`q1`/`q2`, each with a computed `GROUP_CONCAT` column), three
    /// levels of nested `WHERE ... IN (...)`, and an `ON DUPLICATE KEY
    /// UPDATE` clause. Only the derived-table pass-through click is asserted
    /// here (INSERT target/column-list/`VALUES(...)` clicks are Wave 2's
    /// scope); this proves the binder handles the real shape end to end, not
    /// just the simplified per-feature fixtures above.
    #[test]
    fn resolves_a_derived_table_column_in_the_real_production_query() {
        let text = "INSERT INTO ec_fmedia.quotes_pair_translate (pair_ID, lang_id, shortname, pair_name_export_trans, name, nickname, synonym, pair_name_autonews)
SELECT qpa.pair_id,
       q1.lang_id,
       CONCAT(q1.currency_short_name, '/', q2.currency_short_name) AS pair_shortname,
       CONCAT(q1.fullname, ' ', q2.fullname)                       AS pair_fullname,
       CONCAT(q1.fullname, ' ', q2.fullname)                       AS pair_fullname,
       '',
       '',
       ''
FROM (SELECT qdt.currency_ID, qca.currency_short_name, qdt.lang_id, GROUP_CONCAT(qdt.fullname SEPARATOR ' ') AS fullname
      FROM ec_fmedia.quotes_currency_attr qca
               LEFT JOIN ec_fmedia.quotes_currency_dat_trans qdt ON qca.currency_ID = qdt.currency_ID
      WHERE qca.currency_ID IN (SELECT cur1
                                FROM ec_fmedia.quotes_pair_attr
                                WHERE pair_id IN
                                      (SELECT pair_id FROM ec_fmedia.quotes_pair_attr WHERE pair_type IN ('currency')))
      GROUP BY qdt.lang_id, qdt.currency_ID) q1
         JOIN (SELECT qdt.currency_ID,
                      qca.currency_short_name,
                      qdt.lang_id,
                      GROUP_CONCAT(qdt.fullname SEPARATOR ' ') AS fullname
               FROM ec_fmedia.quotes_currency_attr qca
                        LEFT JOIN ec_fmedia.quotes_currency_dat_trans qdt ON qca.currency_ID = qdt.currency_ID
               WHERE qca.currency_ID IN (SELECT cur2
                                         FROM ec_fmedia.quotes_pair_attr
                                         WHERE pair_id IN
                                               (SELECT pair_id
                                                FROM ec_fmedia.quotes_pair_attr
                                                WHERE pair_type IN ('currency')))
               GROUP BY qdt.lang_id, qdt.currency_ID) q2
              ON q1.lang_id = q2.lang_id
         JOIN ec_fmedia.quotes_pair_attr qpa
              ON q1.currency_ID = qpa.cur1 AND q2.currency_ID = qpa.cur2
WHERE qpa.pair_id IN (SELECT pair_id FROM ec_fmedia.quotes_pair_attr WHERE pair_type IN ('currency'))
ORDER BY pair_ID, lang_ID
ON DUPLICATE KEY UPDATE shortname              = VALUES(shortname),
                        pair_name_export_trans = VALUES(pair_name_export_trans),
                        name                   = VALUES(pair_name_export_trans);";
        let schema = FakeSchema::default();

        let offset = text.find("q1.currency_short_name").expect("marker present") + "q1.".len();
        let (target, _) = resolve_navigation_at(text, DatabaseDriver::MySQL, offset, None, &schema)
            .expect("derived-table column resolves in the real query");
        match target {
            NavigationTarget::Column {
                database,
                table,
                column,
                ..
            } => {
                assert_eq!(database.as_deref(), Some("ec_fmedia"));
                assert_eq!(table, "quotes_currency_attr");
                assert_eq!(column, "currency_short_name");
            }
            other => panic!("expected a column target, got {other:?}"),
        }
    }

    #[test]
    fn insert_column_list_entry_resolves_to_the_target_table() {
        let text = "INSERT INTO ec_fmedia.quotes_pair_translate (pair_ID, lang_id, shortname) \
            SELECT 1, 2, 3";
        let schema = FakeSchema::default();
        let offset = text.find("shortname").expect("marker present") + 2;
        let (target, _) = resolve_navigation_at(text, DatabaseDriver::MySQL, offset, None, &schema)
            .expect("insert column list entry resolves");
        match target {
            NavigationTarget::Column {
                database,
                table,
                column,
                ..
            } => {
                assert_eq!(database.as_deref(), Some("ec_fmedia"));
                assert_eq!(table, "quotes_pair_translate");
                assert_eq!(column, "shortname");
            }
            other => panic!("expected a column target, got {other:?}"),
        }
    }

    #[test]
    fn on_duplicate_key_update_target_column_resolves_to_the_insert_table() {
        let text = "INSERT INTO t (a, b) VALUES (1, 2) ON DUPLICATE KEY UPDATE a = VALUES(a)";
        let schema = FakeSchema::default();
        let offset = text.rfind("a = VALUES").expect("marker present");
        let (target, _) = resolve_navigation_at(text, DatabaseDriver::MySQL, offset, None, &schema)
            .expect("assignment target resolves");
        match target {
            NavigationTarget::Column { table, column, .. } => {
                assert_eq!(table, "t");
                assert_eq!(column, "a");
            }
            other => panic!("expected a column target, got {other:?}"),
        }
    }

    /// `VALUES(col)` must resolve to the INSERT target table's column even
    /// when a `FROM`-clause table in the same statement happens to have a
    /// same-named column -- MySQL's `VALUES(col)` always refers to the row
    /// being inserted, never a `FROM`-clause table, no matter how the two
    /// happen to overlap by name.

    #[test]
    fn values_function_argument_resolves_to_the_insert_target_not_a_same_named_from_column() {
        let text = "INSERT INTO shipments (status) SELECT s.status FROM staging s \
             ON DUPLICATE KEY UPDATE status = VALUES(status)";
        let schema = FakeSchema::with_table("db", "staging", &["status"]);
        let offset = text.rfind("VALUES(status)").expect("marker present") + "VALUES(".len();
        let (target, _) = resolve_navigation_at(text, DatabaseDriver::MySQL, offset, None, &schema)
            .expect("VALUES(col) argument resolves");
        match target {
            NavigationTarget::Column {
                database,
                table,
                column,
                ..
            } => {
                assert_eq!(database, None);
                assert_eq!(table, "shipments");
                assert_eq!(column, "status");
            }
            other => panic!("expected a column target, got {other:?}"),
        }
    }

    #[test]
    fn update_assignment_and_where_columns_resolve_through_the_target_table() {
        let text = "UPDATE db.orders t SET t.status = 'x' WHERE t.id = 1";
        let schema = FakeSchema::default();

        let status_offset = text.find("t.status").expect("marker present") + "t.".len();
        let (status_target, _) =
            resolve_navigation_at(text, DatabaseDriver::MySQL, status_offset, None, &schema)
                .expect("assignment target resolves");
        match status_target {
            NavigationTarget::Column { table, column, .. } => {
                assert_eq!(table, "orders");
                assert_eq!(column, "status");
            }
            other => panic!("expected a column target, got {other:?}"),
        }

        let id_offset = text.rfind("t.id").expect("marker present") + "t.".len();
        let (id_target, _) =
            resolve_navigation_at(text, DatabaseDriver::MySQL, id_offset, None, &schema)
                .expect("where column resolves");
        match id_target {
            NavigationTarget::Column { table, column, .. } => {
                assert_eq!(table, "orders");
                assert_eq!(column, "id");
            }
            other => panic!("expected a column target, got {other:?}"),
        }
    }

    #[test]
    fn delete_where_column_resolves_through_the_from_table() {
        let text = "DELETE FROM db.orders t WHERE t.status = 1";
        let schema = FakeSchema::default();
        let offset = text.rfind("t.status").expect("marker present") + "t.".len();
        let (target, _) = resolve_navigation_at(text, DatabaseDriver::MySQL, offset, None, &schema)
            .expect("where column resolves");
        match target {
            NavigationTarget::Column { table, column, .. } => {
                assert_eq!(table, "orders");
                assert_eq!(column, "status");
            }
            other => panic!("expected a column target, got {other:?}"),
        }
    }

    /// End-to-end proof that a real production-shaped query resolves several
    /// distinct DML-specific entities via the AST binder (not the heuristic
    /// fallback): the INSERT column list, the `ON DUPLICATE KEY UPDATE`
    /// target column, and its `VALUES(...)` argument -- alongside the
    /// derived-table/nested-subquery resolution Wave 1 already covered for
    /// this same query.
    #[test]
    fn resolves_insert_and_on_duplicate_key_update_columns_in_the_real_production_query() {
        let text = "INSERT INTO ec_fmedia.quotes_pair_translate (pair_ID, lang_id, shortname, pair_name_export_trans, name, nickname, synonym, pair_name_autonews)
SELECT qpa.pair_id,
       q1.lang_id,
       CONCAT(q1.currency_short_name, '/', q2.currency_short_name) AS pair_shortname,
       CONCAT(q1.fullname, ' ', q2.fullname)                       AS pair_fullname,
       CONCAT(q1.fullname, ' ', q2.fullname)                       AS pair_fullname,
       '',
       '',
       ''
FROM (SELECT qdt.currency_ID, qca.currency_short_name, qdt.lang_id, GROUP_CONCAT(qdt.fullname SEPARATOR ' ') AS fullname
      FROM ec_fmedia.quotes_currency_attr qca
               LEFT JOIN ec_fmedia.quotes_currency_dat_trans qdt ON qca.currency_ID = qdt.currency_ID
      WHERE qca.currency_ID IN (SELECT cur1
                                FROM ec_fmedia.quotes_pair_attr
                                WHERE pair_id IN
                                      (SELECT pair_id FROM ec_fmedia.quotes_pair_attr WHERE pair_type IN ('currency')))
      GROUP BY qdt.lang_id, qdt.currency_ID) q1
         JOIN (SELECT qdt.currency_ID,
                      qca.currency_short_name,
                      qdt.lang_id,
                      GROUP_CONCAT(qdt.fullname SEPARATOR ' ') AS fullname
               FROM ec_fmedia.quotes_currency_attr qca
                        LEFT JOIN ec_fmedia.quotes_currency_dat_trans qdt ON qca.currency_ID = qdt.currency_ID
               WHERE qca.currency_ID IN (SELECT cur2
                                         FROM ec_fmedia.quotes_pair_attr
                                         WHERE pair_id IN
                                               (SELECT pair_id
                                                FROM ec_fmedia.quotes_pair_attr
                                                WHERE pair_type IN ('currency')))
               GROUP BY qdt.lang_id, qdt.currency_ID) q2
              ON q1.lang_id = q2.lang_id
         JOIN ec_fmedia.quotes_pair_attr qpa
              ON q1.currency_ID = qpa.cur1 AND q2.currency_ID = qpa.cur2
WHERE qpa.pair_id IN (SELECT pair_id FROM ec_fmedia.quotes_pair_attr WHERE pair_type IN ('currency'))
ORDER BY pair_ID, lang_ID
ON DUPLICATE KEY UPDATE shortname              = VALUES(shortname),
                        pair_name_export_trans = VALUES(pair_name_export_trans),
                        name                   = VALUES(pair_name_export_trans);";
        let schema = FakeSchema::default();

        let column_list_offset = text
            .find("pair_ID, lang_id, shortname")
            .expect("marker present")
            + 2;
        let (column_list_target, _) = resolve_navigation_at(
            text,
            DatabaseDriver::MySQL,
            column_list_offset,
            None,
            &schema,
        )
        .expect("insert column list entry resolves");
        match column_list_target {
            NavigationTarget::Column { table, column, .. } => {
                assert_eq!(table, "quotes_pair_translate");
                assert_eq!(column, "pair_ID");
            }
            other => panic!("expected a column target, got {other:?}"),
        }

        let assignment_offset = text
            .rfind("shortname              =")
            .expect("marker present");
        let (assignment_target, _) = resolve_navigation_at(
            text,
            DatabaseDriver::MySQL,
            assignment_offset,
            None,
            &schema,
        )
        .expect("on duplicate key update target resolves");
        match assignment_target {
            NavigationTarget::Column { table, column, .. } => {
                assert_eq!(table, "quotes_pair_translate");
                assert_eq!(column, "shortname");
            }
            other => panic!("expected a column target, got {other:?}"),
        }

        let values_offset =
            text.rfind("VALUES(shortname)").expect("marker present") + "VALUES(".len();
        let (values_target, _) =
            resolve_navigation_at(text, DatabaseDriver::MySQL, values_offset, None, &schema)
                .expect("VALUES(col) argument resolves");
        match values_target {
            NavigationTarget::Column { table, column, .. } => {
                assert_eq!(table, "quotes_pair_translate");
                assert_eq!(column, "shortname");
            }
            other => panic!("expected a column target, got {other:?}"),
        }
    }

    fn scoped(text: &str, offset: usize) -> HashMap<String, ScopeBinding> {
        scope_bindings_at(text, DatabaseDriver::MySQL, offset)
            .expect("statement parses")
            .into_iter()
            .collect()
    }

    #[test]
    fn scope_bindings_returns_the_single_from_table() {
        let text = "SELECT 1 FROM db.table t WHERE 1 = 1";
        let offset = text.find("FROM").expect("marker present");
        let bindings = scoped(text, offset);
        assert_eq!(
            bindings.get("t"),
            Some(&ScopeBinding::Real {
                database: Some("db".to_string()),
                table: "table".to_string(),
            })
        );
    }

    #[test]
    fn scope_bindings_returns_both_sides_of_a_join() {
        let text = "SELECT 1 FROM db.orders o JOIN db.customers c ON o.customer_id = c.id";
        let offset = text.find("FROM").expect("marker present");
        let bindings = scoped(text, offset);
        assert_eq!(
            bindings.get("o"),
            Some(&ScopeBinding::Real {
                database: Some("db".to_string()),
                table: "orders".to_string(),
            })
        );
        assert_eq!(
            bindings.get("c"),
            Some(&ScopeBinding::Real {
                database: Some("db".to_string()),
                table: "customers".to_string(),
            })
        );
    }

    #[test]
    fn scope_bindings_expose_a_derived_tables_own_projected_names_not_the_real_columns() {
        // `q`'s own projection is `a` (a plain pass-through) and `c` (an
        // alias for `b`) -- a completion offered through `q.` must be exactly
        // these two names, never `real`'s raw column set.
        let text = "SELECT q.a FROM (SELECT a, b AS c FROM real_table) q";
        let offset = text.find("q.a").expect("marker present") + "q.".len();
        let bindings = scoped(text, offset);
        match bindings.get("q") {
            Some(ScopeBinding::Derived { columns }) => {
                assert_eq!(columns, &vec!["a".to_string(), "c".to_string()]);
            }
            other => panic!("expected a derived binding, got {other:?}"),
        }
    }

    #[test]
    fn scope_bindings_prefer_the_innermost_subquery_scope() {
        let text = "SELECT * FROM outer_tbl o WHERE o.id IN (SELECT 1 FROM inner_tbl x)";
        let offset = text.rfind("FROM").expect("inner marker present");
        let bindings = scoped(text, offset);
        assert_eq!(bindings.len(), 1);
        assert_eq!(
            bindings.get("x"),
            Some(&ScopeBinding::Real {
                database: None,
                table: "inner_tbl".to_string(),
            })
        );
    }

    #[test]
    fn scope_bindings_return_none_for_an_incomplete_statement() {
        // A dangling qualifier dot is not yet valid SQL -- exactly the moment
        // completion fires right after typing `alias.`. The caller is
        // expected to fall back to the heuristic scanner in this case.
        let text = "SELECT * FROM db.orders o WHERE o.";
        assert!(scope_bindings_at(text, DatabaseDriver::MySQL, text.len()).is_none());
    }

    #[test]
    fn scope_bindings_resolve_the_real_production_query_without_a_live_connection() {
        // `scope_bindings_at` never takes a schema/connection argument at
        // all -- there is no live-connection object to withhold, which is
        // itself the proof this path can never require one.
        let text = "INSERT INTO ec_fmedia.quotes_pair_translate (pair_ID) \
             SELECT qpa.pair_id \
             FROM (SELECT qca.currency_short_name FROM ec_fmedia.quotes_currency_attr qca) q1 \
             JOIN ec_fmedia.quotes_pair_attr qpa ON q1.currency_ID = qpa.cur1";
        let offset = text.find("qpa.pair_id").expect("marker present") + "qpa.".len();
        let bindings = scoped(text, offset);
        assert_eq!(
            bindings.get("qpa"),
            Some(&ScopeBinding::Real {
                database: Some("ec_fmedia".to_string()),
                table: "quotes_pair_attr".to_string(),
            })
        );
        assert!(matches!(
            bindings.get("q1"),
            Some(ScopeBinding::Derived { .. })
        ));
    }
}
