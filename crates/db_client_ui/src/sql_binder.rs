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
fn offset_for_location(text: &str, location: Location) -> Option<usize> {
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
    /// A CTE or derived table: its own projected columns, mapped (lowercased
    /// projected name) to the real source they resolve through to. A
    /// computed projection (e.g. an aggregate) maps to the first real column
    /// referenced inside it, mirroring the heuristic scanner's fallback.
    Derived {
        projections: HashMap<String, (Option<String>, String, String)>,
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
fn database_and_table(name: &ObjectName) -> (Option<String>, String) {
    let parts = object_name_parts(name);
    match parts.len() {
        0 => (None, String::new()),
        1 => (None, parts[0].clone()),
        len => (
            Some(parts[len - 2].clone()),
            parts[len - 1].clone(),
        ),
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
        let projections = select_projections(&cte.query, &ctes, ctx);
        ctes.insert(
            cte.alias.name.value.to_ascii_lowercase(),
            TableBinding::Derived { projections },
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
                if let Some(hit) = resolve_expr_at(constraint_expr, ctes, &local_tables, target, ctx)
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
    if let Some(hit) = bind_table_factor(&table_with_joins.relation, ctes, target, ctx, local_tables)
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
                ctes.get(&key_from_name).cloned().unwrap_or(TableBinding::Real {
                    database,
                    table,
                })
            } else {
                TableBinding::Real { database, table }
            };
            let alias_key = alias
                .as_ref()
                .map(|alias| alias.name.value.to_ascii_lowercase())
                .unwrap_or(key_from_name);
            Some((alias_key, binding))
        }
        TableFactor::Derived { subquery, alias, .. } => {
            let alias = alias.as_ref()?;
            let projections = select_projections(subquery, ctes, ctx);
            Some((
                alias.name.value.to_ascii_lowercase(),
                TableBinding::Derived { projections },
            ))
        }
        _ => None,
    }
}

/// Maps each of `query`'s own projected columns (lowercased) to the real
/// source it resolves through to, using only `query`'s own FROM/JOIN tables.
/// A projection with no identifiable source column (a literal, `*`, or a
/// pass-through of another derived table one level further down) is skipped
/// rather than guessed, matching the heuristic scanner's own convention.
fn select_projections(
    query: &Query,
    ctes: &HashMap<String, TableBinding>,
    ctx: &BindCtx,
) -> HashMap<String, (Option<String>, String, String)> {
    let mut projections = HashMap::default();
    let SetExpr::Select(select) = query.body.as_ref() else {
        return projections;
    };

    let mut tables: HashMap<String, TableBinding> = HashMap::default();
    for table_with_joins in &select.from {
        if let Some((key, binding)) = collect_table_binding(&table_with_joins.relation, ctes, ctx) {
            tables.insert(key, binding);
        }
        for join in &table_with_joins.joins {
            if let Some((key, binding)) = collect_table_binding(&join.relation, ctes, ctx) {
                tables.insert(key, binding);
            }
        }
    }

    for item in &select.projection {
        let (expr, explicit_name) = match item {
            SelectItem::UnnamedExpr(expr) => (Some(expr), None),
            SelectItem::ExprWithAlias { expr, alias } => (Some(expr), Some(alias.value.clone())),
            _ => (None, None),
        };
        let Some(expr) = expr else { continue };
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
    projections
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
                TableBinding::Derived { projections } => {
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
                            if ctx.schema.table_has_column(&resolved_database, table, column) {
                                owners.push(NavigationTarget::Column {
                                    database: database.clone(),
                                    table: table.clone(),
                                    column: column.to_string(),
                                    span,
                                });
                            }
                        }
                    }
                    TableBinding::Derived { projections } => {
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
            if owners.len() == 1 { owners.pop() } else { None }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeSchema(HashMap<(String, String), Vec<String>>);

    impl FakeSchema {
        fn with_table(database: &str, table: &str, columns: &[&str]) -> Self {
            let mut schema = FakeSchema::default();
            schema.0.insert(
                (database.to_string(), table.to_string()),
                columns.iter().map(|c| c.to_string()).collect(),
            );
            schema
        }
    }

    impl SchemaLookup for FakeSchema {
        fn table_has_column(&self, database: &str, table: &str, column: &str) -> bool {
            self.0
                .get(&(database.to_string(), table.to_string()))
                .is_some_and(|columns| columns.iter().any(|c| c.eq_ignore_ascii_case(column)))
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
        let offset =
            text.find("q1.currency_short_name").expect("marker present") + "q1.".len();
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

        let inner_offset =
            text.rfind("inner_s.operation").expect("inner marker present") + "inner_s.".len();
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
        let schema = {
            let mut schema = FakeSchema::with_table("db", "orders", &["flag"]);
            schema
                .0
                .insert(("db".to_string(), "shipments".to_string()), vec!["flag".to_string()]);
            schema
        };
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
            NavigationTarget::Table { database, table, .. } => {
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
            NavigationTarget::Table { database, table, .. } => {
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
            NavigationTarget::Table { database, table, .. } => {
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
        assert!(resolve_navigation_at(text, DatabaseDriver::MySQL, offset, None, &schema).is_some());
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

        let offset =
            text.find("q1.currency_short_name").expect("marker present") + "q1.".len();
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
        let text =
            "INSERT INTO shipments (status) SELECT s.status FROM staging s \
             ON DUPLICATE KEY UPDATE status = VALUES(status)";
        let schema = FakeSchema::with_table("db", "staging", &["status"]);
        let offset = text.rfind("VALUES(status)").expect("marker present") + "VALUES(".len();
        let (target, _) = resolve_navigation_at(text, DatabaseDriver::MySQL, offset, None, &schema)
            .expect("VALUES(col) argument resolves");
        match target {
            NavigationTarget::Column { database, table, column, .. } => {
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

        let column_list_offset =
            text.find("pair_ID, lang_id, shortname").expect("marker present") + 2;
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

        let assignment_offset =
            text.rfind("shortname              =").expect("marker present");
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

        let values_offset = text.rfind("VALUES(shortname)").expect("marker present")
            + "VALUES(".len();
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
}



