use crate::aerospike_view::AerospikeView;
use crate::compare_data::CompareDataView;
use crate::connection_view::ConnectionView;
use crate::data_import::ImportDataView;
use crate::db_migration::{
    BUNDLE_VERSION, ConsoleFile, EncryptedSecrets, ExportBundle, decrypt_secrets, encrypt_secrets,
};
use crate::ddl_source::DdlSourceView;
use crate::driver_icon::brand_icon;
use crate::erd_diagram::{ErdColumn, ErdRelationship, ErdTable, ErdView};
use crate::explain_plan::{
    ExplainPlanView, PlanNode, explain_sql_for_driver, parse_plan_tree, plan_text_from_result,
};
use crate::full_text_search::FullTextSearchView;
use crate::go_to_object::GoToObjectPalette;
use crate::modify_table::ModifyTableView;
use crate::native_dump::{
    DumpRequest, DumpRunCallback, DumpStatus, DumpTask, NativeDumpDialog, apply_substitutions,
    render_dump_status_row, spawn_dump,
};
use crate::result_view::{ResultView, format_query_error};
use crate::sql_completion_provider::install_on_editor;
use crate::store::{
    ActiveConnection, ConnectionStatus, DatabaseStore, DatabaseStoreEvent, RelativePosition,
    RunConfiguration, TreeItemRef,
};
use anyhow::Context as _;
use collections::{HashMap, HashSet};
use db::kvp::KeyValueStore;
use db_client::{
    ConnectionConfig, ConnectionId, DatabaseDriver, Folder, FolderId, ProcedureKind, QueryResult,
    schema::ColumnInfo,
};
use editor::{Editor, EditorEvent, GotoDefinitionKind, SemanticsProvider, ToOffset};
use futures::future::Shared;
use gpui::{
    AnyElement, App, AsyncApp, AsyncWindowContext, ClickEvent, Context, DismissEvent,
    DragMoveEvent, ElementId, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement,
    IntoElement, MouseButton, MouseDownEvent, ParentElement, Pixels, Point, PromptLevel, Render,
    ScrollHandle, SharedString, StatefulInteractiveElement, Styled, Subscription, Task, WeakEntity,
    Window, anchored, deferred, div, px,
};
use language::{Anchor, Buffer, BufferId, BufferRow};
use multi_buffer::MultiBuffer;
use project::{
    DocumentHighlight, InlayHint, InvalidationStrategy, Location, LocationLink, ProjectTransaction,
    lsp_store::{BufferSemanticTokens, CacheInlayHints, RefreshForServer},
};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use terminal_view::terminal_panel::TerminalPanel;
use time::OffsetDateTime;
use time::macros::format_description;
use ui::{
    CommonAnimationExt, ContextMenu, HighlightedLabel, Icon, IconButton, IconName, IconSize,
    Indicator, Label, LabelSize, ScrollAxes, Scrollbars, Tooltip, WithScrollbar, prelude::*,
    right_click_menu,
};
use util::ResultExt as _;
use util::TryFutureExt as _;
use workspace::{
    Event as WorkspaceEvent, ItemHandle, ModalView, OpenOptions, OpenVisible, Pane, Toast,
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    notifications::NotificationId,
};
use zed_actions::database_panel::{
    CollapseSelectedEntry, ExpandSelectedEntry, GoToDdl, GoToObject, MoveSelectedDown,
    MoveSelectedUp, QuickDocumentation, ShowDiagram, ToggleFocus,
};

const DATABASE_PANEL_KEY: &str = "DatabasePanel";
const ERD_TABLE_LIMIT: usize = 50;

/// Base left padding and per-level step for every row in the connection
/// tree (folders, connections, databases, tables, columns, and every
/// nested grouping under them). Every row's indentation must be derived
/// from its tree level via this single function so the guide rhythm stays
/// consistent end-to-end, instead of each nesting level hardcoding its own
/// pixel offset.
const TREE_ROW_BASE_INDENT: f32 = 8.;
const TREE_ROW_INDENT_STEP: f32 = 12.;

fn tree_indent(level: usize) -> Pixels {
    px(TREE_ROW_BASE_INDENT + level as f32 * TREE_ROW_INDENT_STEP)
}

fn parse_env_color(s: &str) -> Option<gpui::Rgba> {
    let hex = s.trim().trim_start_matches('#');
    if hex.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some(gpui::Rgba {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a: 1.0,
    })
}

pub(crate) fn init(cx: &mut App) {
    // Workspace action handlers (ToggleFocus, NewQuery, RunQuery) are registered
    // in zed::register_actions, which runs reliably for every workspace. An
    // observe_new here did not fire for the app's workspaces, so RunQuery had no
    // reachable handler and Ctrl+Enter fell through to the inline assistant.

    // A console restored from a saved session is reopened by the workspace, not
    // through `open_new_sql_query`, so it has no addon, semantics provider, or
    // completion provider until reopened. Re-install the console features for any
    // editor whose file is one of our console paths. The work is deferred because
    // `install_db_editor_features` updates the editor we are observing, which is
    // still leased inside this callback.
    cx.observe_new(|editor: &mut Editor, _window, cx: &mut Context<Editor>| {
        if editor.addon::<DbQueryEditorAddon>().is_some() {
            return;
        }
        let editor = cx.entity();
        cx.defer(move |cx| {
            let Some(store) = DatabaseStore::global(cx) else {
                return;
            };
            if editor.read(cx).addon::<DbQueryEditorAddon>().is_some() {
                return;
            }
            if console_connection_for_editor(&editor, &store, cx).is_none() {
                return;
            }
            let Some(workspace) = editor.read(cx).workspace() else {
                return;
            };
            install_db_editor_features(editor, store.downgrade(), workspace.downgrade(), cx);
        });
    })
    .detach();
}

// Carries the connection a SQL console editor is bound to, so Ctrl+Enter runs
// against that exact connection. The RunQuery handler reads this addon first;
// when it is absent (e.g. a console restored from a session) the handler falls
// back to the console file path, so the binding can never silently break.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QueryExecutionStatus {
    Running,
    Success,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct QueryExecutionMarker {
    row: u32,
    status: QueryExecutionStatus,
}

struct DbQueryEditorAddon {
    connection_id: ConnectionId,
    query_markers: Vec<QueryExecutionMarker>,
    inline_results: Option<Entity<crate::inline_results::InlineResultsController>>,
}

impl editor::Addon for DbQueryEditorAddon {
    fn render_gutter_indicator(
        &self,
        _: u32,
        buffer_row: Option<u32>,
        _: &mut Window,
        _: &mut Context<Editor>,
    ) -> Option<gpui::AnyElement> {
        let row = buffer_row?;
        let status = self
            .query_markers
            .iter()
            .find(|marker| marker.row == row)?
            .status;

        Some(render_query_status_indicator(row, status).into_any_element())
    }

    fn to_any(&self) -> &dyn std::any::Any {
        self
    }

    fn to_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

impl DbQueryEditorAddon {
    fn new(connection_id: ConnectionId) -> Self {
        Self {
            connection_id,
            query_markers: Vec::new(),
            inline_results: None,
        }
    }

    fn clear_query_markers(&mut self) {
        self.query_markers.clear();
    }

    fn mark_query(&mut self, row: u32, status: QueryExecutionStatus) {
        if let Some(marker) = self
            .query_markers
            .iter_mut()
            .find(|marker| marker.row == row)
        {
            marker.status = status;
        } else {
            self.query_markers
                .push(QueryExecutionMarker { row, status });
        }
    }

    #[cfg(test)]
    fn query_markers(&self) -> &[QueryExecutionMarker] {
        &self.query_markers
    }
}

fn render_query_status_indicator(row: u32, status: QueryExecutionStatus) -> impl IntoElement {
    let id = ElementId::from(SharedString::from(format!("sql-query-status-{row}")));
    div()
        .id(id.clone())
        .debug_selector(move || format!("SQL_QUERY_STATUS-{row}"))
        .size_5()
        .flex()
        .items_center()
        .justify_center()
        .child(match status {
            QueryExecutionStatus::Running => Icon::new(IconName::ArrowCircle)
                .size(IconSize::Small)
                .color(Color::Hint)
                .with_keyed_rotate_animation(id, 1)
                .into_any_element(),
            QueryExecutionStatus::Success => Icon::new(IconName::Check)
                .size(IconSize::Small)
                .color(Color::Success)
                .into_any_element(),
            QueryExecutionStatus::Error => Icon::new(IconName::XCircle)
                .size(IconSize::Small)
                .color(Color::Error)
                .into_any_element(),
        })
}

struct DbSemanticsProvider {
    connection_id: ConnectionId,
    store: WeakEntity<DatabaseStore>,
    workspace: WeakEntity<Workspace>,
}

impl crate::sql_binder::SchemaLookup for ActiveConnection {
    fn table_has_column(&self, database: &str, table: &str, column: &str) -> bool {
        self.expanded_tables
            .get(&(database.to_string(), table.to_string()))
            .is_some_and(|columns| columns.iter().any(|c| c.name.eq_ignore_ascii_case(column)))
    }

    fn has_schema_for_database(&self, database: &str) -> bool {
        self.expanded_databases.contains_key(database)
    }

    fn table_exists(&self, database: &str, table: &str) -> bool {
        self.expanded_databases
            .get(database)
            .is_some_and(|tables| tables.iter().any(|t| t.name.eq_ignore_ascii_case(table)))
    }

    fn has_columns_for_table(&self, database: &str, table: &str) -> bool {
        self.expanded_tables
            .contains_key(&(database.to_string(), table.to_string()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SqlTableReference {
    database: Option<String>,
    table: String,
    start: usize,
    end: usize,
}

fn is_sql_reference_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'.'
}

fn is_sql_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn keyword_at(bytes: &[u8], index: usize, keyword: &[u8]) -> bool {
    if index + keyword.len() > bytes.len() {
        return false;
    }
    if index > 0 && is_sql_word_byte(bytes[index - 1]) {
        return false;
    }
    if index + keyword.len() < bytes.len() && is_sql_word_byte(bytes[index + keyword.len()]) {
        return false;
    }
    bytes[index..index + keyword.len()]
        .iter()
        .zip(keyword)
        .all(|(left, right)| left.eq_ignore_ascii_case(right))
}

fn skip_sql_whitespace(bytes: &[u8], mut index: usize) -> usize {
    while index < bytes.len() && bytes[index].is_ascii_whitespace() {
        index += 1;
    }
    index
}

fn parse_table_reference(
    text: &str,
    reference_start: usize,
    reference_end: usize,
    required_offset: Option<usize>,
) -> Option<SqlTableReference> {
    if reference_start >= reference_end || reference_end > text.len() {
        return None;
    }

    let reference = &text[reference_start..reference_end];
    if reference.is_empty() || reference.starts_with('.') || reference.ends_with('.') {
        return None;
    }

    let (database, table, table_start) = if let Some(dot) = reference.rfind('.') {
        let database = &reference[..dot];
        let table = &reference[dot + 1..];
        if database.is_empty() || table.is_empty() {
            return None;
        }
        (
            Some(database.to_string()),
            table.to_string(),
            reference_start + dot + 1,
        )
    } else {
        (None, reference.to_string(), reference_start)
    };

    if table.is_empty() {
        return None;
    }

    // A bare (unqualified) reserved word like `SELECT` or `FROM` is never a
    // real table name; without this check the lexical scanner above would
    // happily treat any keyword under the cursor as a candidate table and
    // make it Ctrl+click-navigable.
    if database.is_none()
        && crate::sql_completion_provider::CORE_KEYWORDS
            .iter()
            .any(|keyword| keyword.eq_ignore_ascii_case(&table))
    {
        return None;
    }

    let table_end = table_start + table.len();
    if let Some(offset) = required_offset {
        if offset < table_start || offset > table_end {
            return None;
        }
    }

    Some(SqlTableReference {
        database,
        table,
        start: table_start,
        end: table_end,
    })
}

fn lexical_table_reference_at_offset(text: &str, offset: usize) -> Option<SqlTableReference> {
    if offset > text.len() {
        return None;
    }

    let bytes = text.as_bytes();
    let mut start = offset;
    while start > 0 && is_sql_reference_byte(bytes[start - 1]) {
        start -= 1;
    }

    let mut end = offset;
    while end < bytes.len() && is_sql_reference_byte(bytes[end]) {
        end += 1;
    }

    if start == end {
        return None;
    }

    parse_table_reference(text, start, end, Some(offset))
}

fn table_reference_at_offset(text: &str, offset: usize) -> Option<SqlTableReference> {
    lexical_table_reference_at_offset(text, offset)
}

fn table_reference_after(
    bytes: &[u8],
    sql: &str,
    mut reference_start: usize,
) -> Option<SqlTableReference> {
    reference_start = skip_sql_whitespace(bytes, reference_start);
    let mut reference_end = reference_start;
    while reference_end < bytes.len() && is_sql_reference_byte(bytes[reference_end]) {
        reference_end += 1;
    }
    parse_table_reference(sql, reference_start, reference_end, None)
}

fn select_table_reference(sql: &str) -> Option<SqlTableReference> {
    let bytes = sql.as_bytes();
    let select_start = skip_sql_whitespace(bytes, 0);
    if !keyword_at(bytes, select_start, b"select") {
        return None;
    }

    let mut index = select_start + b"select".len();
    while index < bytes.len() {
        if keyword_at(bytes, index, b"from") {
            return table_reference_after(bytes, sql, index + b"from".len());
        }
        index += 1;
    }
    None
}

fn show_create_table_reference(sql: &str) -> Option<SqlTableReference> {
    let bytes = sql.as_bytes();
    let mut index = skip_sql_whitespace(bytes, 0);
    if !keyword_at(bytes, index, b"show") {
        return None;
    }
    index = skip_sql_whitespace(bytes, index + b"show".len());
    if !keyword_at(bytes, index, b"create") {
        return None;
    }
    index = skip_sql_whitespace(bytes, index + b"create".len());
    if !keyword_at(bytes, index, b"table") {
        return None;
    }
    table_reference_after(bytes, sql, index + b"table".len())
}

fn statement_table_reference_at_offset(text: &str, offset: usize) -> Option<SqlTableReference> {
    let lexical_reference = lexical_table_reference_at_offset(text, offset)?;
    let cursor = offset.min(text.len());
    let Range {
        start: statement_start,
        end: statement_end,
    } = statement_bounds_at_offset(text, cursor);
    let statement = &text[statement_start..statement_end];
    let mut statement_reference =
        show_create_table_reference(statement).or_else(|| select_table_reference(statement))?;
    statement_reference.start += statement_start;
    statement_reference.end += statement_start;
    if lexical_reference.start == statement_reference.start
        && lexical_reference.end == statement_reference.end
    {
        Some(statement_reference)
    } else {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SqlDatabaseReference {
    database: String,
    start: usize,
    end: usize,
}

// `SHOW CREATE DATABASE <name>` / `SHOW CREATE SCHEMA <name>` — returns the span
// of the database identifier so Ctrl+click on it can open the database DDL.
fn show_create_database_reference(sql: &str) -> Option<SqlDatabaseReference> {
    let bytes = sql.as_bytes();
    let mut index = skip_sql_whitespace(bytes, 0);
    if !keyword_at(bytes, index, b"show") {
        return None;
    }
    index = skip_sql_whitespace(bytes, index + b"show".len());
    if !keyword_at(bytes, index, b"create") {
        return None;
    }
    index = skip_sql_whitespace(bytes, index + b"create".len());
    let keyword: &[u8] = if keyword_at(bytes, index, b"database") {
        b"database"
    } else if keyword_at(bytes, index, b"schema") {
        b"schema"
    } else {
        return None;
    };
    let start = skip_sql_whitespace(bytes, index + keyword.len());
    let mut end = start;
    while end < bytes.len() && is_sql_word_byte(bytes[end]) {
        end += 1;
    }
    if start == end {
        return None;
    }
    Some(SqlDatabaseReference {
        database: sql[start..end].to_string(),
        start,
        end,
    })
}

// Resolves a database reference at the cursor: either the `<name>` of a
// `SHOW CREATE DATABASE <name>` statement, or the database part of a qualified
// `database.table` reference (cursor before the dot). Returns None when the
// cursor is on the table part, so the table path keeps its existing behavior.
fn database_reference_at_offset(text: &str, offset: usize) -> Option<SqlDatabaseReference> {
    let cursor = offset.min(text.len());
    let Range {
        start: statement_start,
        end: statement_end,
    } = statement_bounds_at_offset(text, cursor);
    let statement = &text[statement_start..statement_end];

    if let Some(mut reference) = show_create_database_reference(statement) {
        reference.start += statement_start;
        reference.end += statement_start;
        if offset >= reference.start && offset <= reference.end {
            return Some(reference);
        }
    }

    let bytes = text.as_bytes();
    let mut word_start = offset.min(bytes.len());
    while word_start > 0 && is_sql_reference_byte(bytes[word_start - 1]) {
        word_start -= 1;
    }
    let mut word_end = offset.min(bytes.len());
    while word_end < bytes.len() && is_sql_reference_byte(bytes[word_end]) {
        word_end += 1;
    }
    if word_start == word_end {
        return None;
    }
    let word = &text[word_start..word_end];
    let dot = word.find('.')?;
    let dot_offset = word_start + dot;
    if offset > dot_offset {
        return None;
    }
    let database = &word[..dot];
    if database.is_empty() {
        return None;
    }
    let table_reference = statement_table_reference_at_offset(text, dot_offset + 1)?;
    if table_reference.database.as_deref() != Some(database) {
        return None;
    }
    Some(SqlDatabaseReference {
        database: database.to_string(),
        start: word_start,
        end: dot_offset,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SqlColumnReference {
    qualifier: Option<String>,
    column: String,
    start: usize,
    end: usize,
}

// Resolves a column token at the cursor: `alias.col`, `table.col`, or a bare
// `col`. Returns the column identifier plus its optional qualifier (the segment
// right before the last dot). Returns None when the cursor sits on the
// qualifier part, so `db.table` navigation keeps opening the table.
fn column_reference_at_offset(text: &str, offset: usize) -> Option<SqlColumnReference> {
    if offset > text.len() {
        return None;
    }
    let bytes = text.as_bytes();
    let mut start = offset;
    while start > 0 && is_sql_reference_byte(bytes[start - 1]) {
        start -= 1;
    }
    let mut end = offset;
    while end < bytes.len() && is_sql_reference_byte(bytes[end]) {
        end += 1;
    }
    if start == end {
        return None;
    }
    let word = &text[start..end];
    if word.starts_with('.') || word.ends_with('.') {
        return None;
    }

    let (qualifier, column, column_start) = match word.rfind('.') {
        Some(dot) => {
            let column = &word[dot + 1..];
            let left = &word[..dot];
            let qualifier = left.rsplit('.').next().unwrap_or(left);
            if column.is_empty() || qualifier.is_empty() {
                return None;
            }
            (
                Some(qualifier.to_string()),
                column.to_string(),
                start + dot + 1,
            )
        }
        None => (None, word.to_string(), start),
    };

    if offset < column_start {
        return None;
    }
    let column_end = column_start + column.len();
    Some(SqlColumnReference {
        qualifier,
        column,
        start: column_start,
        end: column_end,
    })
}

const DDL_STRUCTURAL_KEYWORDS: &[&str] = &[
    "primary",
    "unique",
    "key",
    "index",
    "constraint",
    "foreign",
    "fulltext",
    "spatial",
    "check",
    "create",
    "engine",
];

// True when a DDL body line declares `column`. Matches a quoted identifier
// (`\`col\``, `"col"`, `[col]`) that opens the line, or a bare leading
// identifier that is not a structural keyword (PRIMARY KEY, CONSTRAINT, ...).
fn line_defines_column(content: &str, column: &str) -> bool {
    let bytes = content.as_bytes();
    let (opening, closing): (Option<u8>, Option<u8>) = match bytes.first() {
        Some(b'`') => (Some(b'`'), Some(b'`')),
        Some(b'"') => (Some(b'"'), Some(b'"')),
        Some(b'[') => (Some(b'['), Some(b']')),
        _ => (None, None),
    };
    let identifier_start = if opening.is_some() { 1 } else { 0 };
    let mut identifier_end = identifier_start;
    while identifier_end < bytes.len() && is_sql_word_byte(bytes[identifier_end]) {
        identifier_end += 1;
    }
    if identifier_start == identifier_end {
        return false;
    }
    let identifier = &content[identifier_start..identifier_end];
    if !identifier.eq_ignore_ascii_case(column) {
        return false;
    }
    if let Some(closing) = closing {
        return bytes.get(identifier_end) == Some(&closing);
    }
    !DDL_STRUCTURAL_KEYWORDS
        .iter()
        .any(|keyword| identifier.eq_ignore_ascii_case(keyword))
}

// Byte offset of the line that declares `column` in a DDL string, so Ctrl+click
// on a column can center that definition. The first matching line wins, which
// picks the real column definition over any later `KEY`/index line naming it.
fn find_column_definition_offset(ddl: &str, column: &str) -> Option<usize> {
    let mut byte = 0usize;
    for line in ddl.split_inclusive('\n') {
        let leading = line.len() - line.trim_start().len();
        if line_defines_column(line.trim_start(), column) {
            return Some(byte + leading);
        }
        byte += line.len();
    }
    None
}

// Byte index of the `)` matching the innermost open scope that starts at
// `scope_start` (as returned by `innermost_scope_start`), or the end of `text`
// when `scope_start` is 0 (the cursor is not inside a subquery).
fn innermost_scope_end(text: &str, scope_start: usize) -> usize {
    if scope_start == 0 {
        return text.len();
    }
    let mut depth = 0i32;
    for (index, ch) in text[scope_start..].char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                if depth == 0 {
                    return scope_start + index;
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    text.len()
}

// Tables referenced by the `;`-delimited statement containing `offset`, used to
// resolve a column's `alias`/`table` qualifier and to scope a bare column. When
// `offset` sits inside a subquery, only that subquery's own FROM/JOIN tables are
// considered, so a reused alias in an outer query never shadows the inner one.
fn from_tables_at_offset(
    text: &str,
    offset: usize,
) -> Vec<crate::sql_completion_provider::TableRef> {
    let cursor = offset.min(text.len());
    let Range {
        start: statement_start,
        end: statement_end,
    } = statement_bounds_at_offset(text, cursor);
    let statement = &text[statement_start..statement_end];
    let relative_cursor = (cursor - statement_start).min(statement.len());
    let scope_start =
        crate::sql_completion_provider::innermost_scope_start(&statement[..relative_cursor]);
    let scope_end = innermost_scope_end(statement, scope_start);
    crate::sql_completion_provider::parse_table_refs(&statement[scope_start..scope_end])
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InsertColumnContext {
    database: Option<String>,
    table: String,
    // Absolute byte span of the parenthesized `(col, col, ...)` list right
    // after `INSERT INTO table`, when the statement has an explicit one.
    column_list: Option<(usize, usize)>,
    // Absolute byte offset where the body of a MySQL `ON DUPLICATE KEY
    // UPDATE` clause starts, when the statement has one.
    on_duplicate_key_update: Option<usize>,
}

// `INSERT [IGNORE] INTO <table>`, so column-list and `ON DUPLICATE KEY
// UPDATE` navigation share the same target-table resolution as a plain
// `INSERT INTO` table click.
fn insert_into_target(sql: &str) -> Option<SqlTableReference> {
    let bytes = sql.as_bytes();
    let mut index = skip_sql_whitespace(bytes, 0);
    if !keyword_at(bytes, index, b"insert") {
        return None;
    }
    index = skip_sql_whitespace(bytes, index + b"insert".len());
    if keyword_at(bytes, index, b"ignore") {
        index = skip_sql_whitespace(bytes, index + b"ignore".len());
    }
    if !keyword_at(bytes, index, b"into") {
        return None;
    }
    table_reference_after(bytes, sql, index + b"into".len())
}

// Byte offset right after `ON DUPLICATE KEY UPDATE`, so callers can treat
// everything from there to the end of the statement as column assignments
// against the INSERT target table (MySQL-specific upsert clause).
fn find_on_duplicate_key_update(statement: &str) -> Option<usize> {
    let bytes = statement.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if keyword_at(bytes, index, b"on") {
            let mut probe = skip_sql_whitespace(bytes, index + b"on".len());
            if keyword_at(bytes, probe, b"duplicate") {
                probe = skip_sql_whitespace(bytes, probe + b"duplicate".len());
                if keyword_at(bytes, probe, b"key") {
                    probe = skip_sql_whitespace(bytes, probe + b"key".len());
                    if keyword_at(bytes, probe, b"update") {
                        return Some(skip_sql_whitespace(bytes, probe + b"update".len()));
                    }
                }
            }
        }
        index += 1;
    }
    None
}

// INSERT-target context for the `;`-delimited statement containing `offset`,
// so a column in the `INSERT INTO table (col, col, ...)` list or in an `ON
// DUPLICATE KEY UPDATE col = VALUES(col)` clause resolves against the INSERT
// target table instead of the trailing SELECT's FROM tables.
fn insert_column_context_at_offset(text: &str, offset: usize) -> Option<InsertColumnContext> {
    let cursor = offset.min(text.len());
    let Range {
        start: statement_start,
        end: statement_end,
    } = statement_bounds_at_offset(text, cursor);
    let statement = &text[statement_start..statement_end];
    let target = insert_into_target(statement)?;
    let bytes = statement.as_bytes();
    let after_table = skip_sql_whitespace(bytes, target.end);

    let column_list = if bytes.get(after_table) == Some(&b'(') {
        let mut depth = 0i32;
        let mut span = None;
        for (index, ch) in statement[after_table..].char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        span = Some((
                            statement_start + after_table + 1,
                            statement_start + after_table + index,
                        ));
                        break;
                    }
                }
                _ => {}
            }
        }
        span
    } else {
        None
    };

    let on_duplicate_key_update =
        find_on_duplicate_key_update(statement).map(|local_offset| statement_start + local_offset);

    Some(InsertColumnContext {
        database: target.database,
        table: target.table,
        column_list,
        on_duplicate_key_update,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DerivedTableRef {
    alias: String,
    // Lowercased projected column name -> resolved real (schema, table,
    // column). A computed projection (e.g. an aggregate) maps to the first
    // real column referenced inside its expression, since that is the
    // closest useful navigation target when there is no single source column.
    projections: HashMap<String, (Option<String>, String, String)>,
}

// Derived-table aliases (`FROM (SELECT ...) alias`) visible from `offset`,
// scoped the same way as `from_tables_at_offset` so a click on `q1.col` in an
// outer query resolves through the derived table to its real source column.
fn derived_tables_at_offset(text: &str, offset: usize) -> Vec<DerivedTableRef> {
    let cursor = offset.min(text.len());
    let Range {
        start: statement_start,
        end: statement_end,
    } = statement_bounds_at_offset(text, cursor);
    let statement = &text[statement_start..statement_end];
    let relative_cursor = (cursor - statement_start).min(statement.len());
    let scope_start =
        crate::sql_completion_provider::innermost_scope_start(&statement[..relative_cursor]);
    let scope_end = innermost_scope_end(statement, scope_start);
    derived_tables_in_scope(&statement[scope_start..scope_end])
}

// Scans `scope` for `(SELECT ...) [AS] alias` derived tables directly in its
// own FROM/JOIN list (one nesting level: the subquery's own FROM tables must
// be real tables, not further derived tables), skipping every other
// parenthesized span (function calls, ON-clause groups, nested subqueries)
// entirely so it never misreads their contents as a derived table.
fn derived_tables_in_scope(scope: &str) -> Vec<DerivedTableRef> {
    let bytes = scope.as_bytes();
    let mut derived = Vec::new();
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] != b'(' {
            index += 1;
            continue;
        }
        let starts_derived_table = matches!(
            word_before(scope, bytes, index).as_deref(),
            Some("from") | Some("join")
        );
        if !starts_derived_table {
            index += 1;
            continue;
        }

        let inner_start = index + 1;
        let mut depth = 1i32;
        let mut inner_end = None;
        for (relative, ch) in scope[inner_start..].char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        inner_end = Some(inner_start + relative);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(inner_end) = inner_end else {
            break;
        };

        let subquery = &scope[inner_start..inner_end];
        let mut alias_start = skip_sql_whitespace(bytes, inner_end + 1);
        if keyword_at(bytes, alias_start, b"as") {
            alias_start = skip_sql_whitespace(bytes, alias_start + b"as".len());
        }
        let mut alias_end = alias_start;
        while alias_end < bytes.len() && is_sql_word_byte(bytes[alias_end]) {
            alias_end += 1;
        }
        if alias_end > alias_start
            && subquery
                .trim_start()
                .to_ascii_lowercase()
                .starts_with("select")
        {
            derived.push(DerivedTableRef {
                alias: scope[alias_start..alias_end].to_string(),
                projections: derived_table_projections(subquery),
            });
        }
        index = inner_end + 1;
    }
    derived
}

// Byte range of `subquery`'s own SELECT list (before its top-level FROM),
// respecting parenthesized function calls so a `FROM` inside one is ignored.
fn extract_select_list(subquery: &str) -> Option<&str> {
    let bytes = subquery.as_bytes();
    let mut index = skip_sql_whitespace(bytes, 0);
    if !keyword_at(bytes, index, b"select") {
        return None;
    }
    index = skip_sql_whitespace(bytes, index + b"select".len());
    if keyword_at(bytes, index, b"distinct") {
        index = skip_sql_whitespace(bytes, index + b"distinct".len());
    }
    let list_start = index;
    let mut depth = 0i32;
    while index < bytes.len() {
        match bytes[index] {
            b'(' => depth += 1,
            b')' => depth -= 1,
            _ if depth == 0 && keyword_at(bytes, index, b"from") => {
                return Some(&subquery[list_start..index]);
            }
            _ => {}
        }
        index += 1;
    }
    Some(&subquery[list_start..])
}

// Splits a comma-separated list at top level only, so commas inside a
// function call or nested subquery do not split its arguments apart.
fn split_top_level_commas(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut depth = 0i32;
    let mut start = 0usize;
    let mut parts = Vec::new();
    for (index, &byte) in bytes.iter().enumerate() {
        match byte {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b',' if depth == 0 => {
                parts.push(&text[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(&text[start..]);
    parts
}

// Splits a SELECT-list item into its expression and an explicit trailing `AS
// alias`, matched only at top level so an `AS` inside a function call/subquery
// is ignored.
fn split_projection_alias(item: &str) -> (&str, Option<String>) {
    let bytes = item.as_bytes();
    let mut depth = 0i32;
    let mut as_start = None;
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'(' => depth += 1,
            b')' => depth -= 1,
            _ if depth == 0 && keyword_at(bytes, index, b"as") => {
                as_start = Some(index);
            }
            _ => {}
        }
        index += 1;
    }
    if let Some(as_start) = as_start {
        let expr = item[..as_start].trim_end();
        let alias_start = skip_sql_whitespace(bytes, as_start + b"as".len());
        let mut alias_end = alias_start;
        while alias_end < bytes.len() && is_sql_word_byte(bytes[alias_end]) {
            alias_end += 1;
        }
        if alias_end > alias_start {
            return (expr, Some(item[alias_start..alias_end].to_string()));
        }
    }
    (item.trim(), None)
}

// First `qualifier.column` reference found in `expr`, scanning left to right.
// For a plain pass-through item (`qdt.col`) this is the item itself; for a
// computed expression (`GROUP_CONCAT(qdt.col SEPARATOR ' ')`) it is the
// closest known source column, used as the navigation fallback.
fn first_qualified_reference(expr: &str) -> Option<(String, String)> {
    let bytes = expr.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        if !is_sql_word_byte(bytes[index]) {
            index += 1;
            continue;
        }
        let word_start = index;
        while index < bytes.len() && is_sql_word_byte(bytes[index]) {
            index += 1;
        }
        if bytes.get(index) == Some(&b'.') {
            let qualifier = expr[word_start..index].to_string();
            let column_start = index + 1;
            let mut column_end = column_start;
            while column_end < bytes.len() && is_sql_word_byte(bytes[column_end]) {
                column_end += 1;
            }
            if column_end > column_start {
                return Some((qualifier, expr[column_start..column_end].to_string()));
            }
        }
    }
    None
}

// Maps each projected column of `subquery` (a derived table's own SELECT
// list) to its resolved real (schema, table, column) source, using the
// subquery's own FROM/JOIN tables. An item with no identifiable source column
// (a literal, `*`, or a pure output alias) is skipped rather than guessed.
fn derived_table_projections(subquery: &str) -> HashMap<String, (Option<String>, String, String)> {
    let mut projections = HashMap::default();
    let Some(select_list) = extract_select_list(subquery) else {
        return projections;
    };
    let tables = crate::sql_completion_provider::parse_table_refs(subquery);
    for item in split_top_level_commas(select_list) {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let (expr, explicit_alias) = split_projection_alias(item);
        let Some((qualifier, column)) = first_qualified_reference(expr) else {
            continue;
        };
        let Some(table_ref) =
            crate::sql_completion_provider::resolve_table_ref(&qualifier, &tables)
        else {
            continue;
        };
        let projected_name = explicit_alias.unwrap_or_else(|| column.clone());
        projections.insert(
            projected_name.to_ascii_lowercase(),
            (table_ref.schema.clone(), table_ref.name.clone(), column),
        );
    }
    projections
}

fn word_before(text: &str, bytes: &[u8], offset: usize) -> Option<String> {
    let mut index = offset;
    while index > 0 && bytes[index - 1].is_ascii_whitespace() {
        index -= 1;
    }
    let word_end = index;
    while index > 0 && is_sql_word_byte(bytes[index - 1]) {
        index -= 1;
    }
    if index == word_end {
        return None;
    }
    Some(text[index..word_end].to_ascii_lowercase())
}

impl SemanticsProvider for DbSemanticsProvider {
    fn hover(
        &self,
        _buffer: &Entity<Buffer>,
        _position: Anchor,
        _cx: &mut App,
    ) -> Option<Task<Option<Vec<project::Hover>>>> {
        None
    }

    fn inline_values(
        &self,
        _buffer: Entity<Buffer>,
        _range: std::ops::Range<Anchor>,
        _cx: &mut App,
    ) -> Option<Task<anyhow::Result<Vec<InlayHint>>>> {
        None
    }

    fn applicable_inlay_chunks(
        &self,
        _buffer: &Entity<Buffer>,
        _ranges: &[std::ops::Range<Anchor>],
        _cx: &mut App,
    ) -> Vec<std::ops::Range<BufferRow>> {
        vec![]
    }

    fn invalidate_inlay_hints(&self, _for_buffers: &HashSet<BufferId>, _cx: &mut App) {}

    fn inlay_hints(
        &self,
        _invalidate: InvalidationStrategy,
        _buffer: Entity<Buffer>,
        _ranges: Vec<std::ops::Range<Anchor>>,
        _known_chunks: Option<(clock::Global, HashSet<std::ops::Range<BufferRow>>)>,
        _cx: &mut App,
    ) -> Option<HashMap<std::ops::Range<BufferRow>, Task<anyhow::Result<CacheInlayHints>>>> {
        None
    }

    fn semantic_tokens(
        &self,
        _buffer: Entity<Buffer>,
        _refresh: Option<RefreshForServer>,
        _cx: &mut App,
    ) -> Option<Shared<Task<std::result::Result<BufferSemanticTokens, Arc<anyhow::Error>>>>> {
        None
    }

    fn supports_inlay_hints(&self, _buffer: &Entity<Buffer>, _cx: &mut App) -> bool {
        false
    }

    fn supports_semantic_tokens(&self, _buffer: &Entity<Buffer>, _cx: &mut App) -> bool {
        false
    }

    fn document_highlights(
        &self,
        _buffer: &Entity<Buffer>,
        _position: Anchor,
        _cx: &mut App,
    ) -> Option<Task<anyhow::Result<Vec<DocumentHighlight>>>> {
        None
    }

    fn definitions(
        &self,
        buffer: &Entity<Buffer>,
        position: Anchor,
        _kind: GotoDefinitionKind,
        cx: &mut App,
    ) -> Option<Task<anyhow::Result<Option<Vec<LocationLink>>>>> {
        let snapshot = buffer.read(cx).snapshot();
        let offset = snapshot.offset_for_anchor(&position);
        let text = snapshot.text();

        // Try a real AST-based resolution first: it parses the statement
        // under the cursor with `sqlparser` and walks a scope-stack binder,
        // handling constructs (CTEs, derived tables, nested subqueries) the
        // heuristic scanner below only partially covers. It returns `None`
        // both when the statement doesn't fully parse (most commonly because
        // it is still being typed) and when the token doesn't resolve to a
        // concrete table/column -- either way, the heuristic scanner below
        // remains the fallback and its behavior is unchanged.
        if let Some((target, range)) = self.ast_definition_target(&text, offset, cx) {
            let connection_id = self.connection_id;
            let word_start = snapshot.anchor_before(range.start);
            let word_end = snapshot.anchor_after(range.end);
            match target {
                AstDefinitionTarget::Table { database, table } => {
                    return Some(self.spawn_ddl_navigation(
                        buffer.clone(),
                        word_start..word_end,
                        None,
                        cx,
                        move |store, cx| store.get_table_ddl(connection_id, database, table, cx),
                    ));
                }
                AstDefinitionTarget::Database { database } => {
                    return Some(self.spawn_ddl_navigation(
                        buffer.clone(),
                        word_start..word_end,
                        None,
                        cx,
                        move |store, cx| store.get_database_ddl(connection_id, database, cx),
                    ));
                }
                AstDefinitionTarget::Column {
                    database,
                    table,
                    column,
                } => {
                    return Some(self.spawn_ddl_navigation(
                        buffer.clone(),
                        word_start..word_end,
                        Some(column),
                        cx,
                        move |store, cx| store.get_table_ddl(connection_id, database, table, cx),
                    ));
                }
            }
        }

        if let Some(table_reference) = statement_table_reference_at_offset(&text, offset) {
            let database_opt = table_reference.database;
            let table_name = table_reference.table;

            let (database, table) = self
                .store
                .read_with(cx, |store, _| {
                    let conn = store
                        .connections
                        .iter()
                        .find(|c| c.config.id == self.connection_id)?;
                    let db = database_opt.clone().or_else(|| {
                        conn.config
                            .database
                            .clone()
                            .or_else(|| conn.expanded_databases.keys().next().cloned())
                    })?;
                    Some((db, table_name.clone()))
                })
                .ok()
                .flatten()?;
            let word_start = snapshot.anchor_before(table_reference.start);
            let word_end = snapshot.anchor_after(table_reference.end);
            let connection_id = self.connection_id;
            return Some(self.spawn_ddl_navigation(
                buffer.clone(),
                word_start..word_end,
                None,
                cx,
                move |store, cx| store.get_table_ddl(connection_id, database, table, cx),
            ));
        }

        if let Some(database_reference) = database_reference_at_offset(&text, offset) {
            let database = database_reference.database;
            let word_start = snapshot.anchor_before(database_reference.start);
            let word_end = snapshot.anchor_after(database_reference.end);
            let connection_id = self.connection_id;
            return Some(self.spawn_ddl_navigation(
                buffer.clone(),
                word_start..word_end,
                None,
                cx,
                move |store, cx| store.get_database_ddl(connection_id, database, cx),
            ));
        }

        // An INSERT column list or `ON DUPLICATE KEY UPDATE`/`VALUES(...)`
        // column always belongs to the INSERT target table, never to the
        // trailing SELECT's FROM tables, so this runs ahead of the generic
        // column resolution below.
        if let Some(insert_context) = insert_column_context_at_offset(&text, offset) {
            let in_column_list = insert_context
                .column_list
                .is_some_and(|(start, end)| offset >= start && offset <= end);
            let in_on_duplicate_key_update = insert_context
                .on_duplicate_key_update
                .is_some_and(|start| offset >= start);
            if in_column_list || in_on_duplicate_key_update {
                if let Some(column_reference) = column_reference_at_offset(&text, offset) {
                    if column_reference.qualifier.is_none()
                        && !column_reference.column.eq_ignore_ascii_case("values")
                    {
                        let database = self
                            .store
                            .read_with(cx, |store, _| {
                                let conn = store
                                    .connections
                                    .iter()
                                    .find(|c| c.config.id == self.connection_id)?;
                                insert_context.database.clone().or_else(|| {
                                    conn.config
                                        .database
                                        .clone()
                                        .filter(|database| !database.is_empty())
                                        .or_else(|| conn.expanded_databases.keys().next().cloned())
                                })
                            })
                            .ok()
                            .flatten();
                        if let Some(database) = database {
                            let word_start = snapshot.anchor_before(column_reference.start);
                            let word_end = snapshot.anchor_after(column_reference.end);
                            let connection_id = self.connection_id;
                            let table = insert_context.table;
                            let focus_column = column_reference.column;
                            return Some(self.spawn_ddl_navigation(
                                buffer.clone(),
                                word_start..word_end,
                                Some(focus_column),
                                cx,
                                move |store, cx| {
                                    store.get_table_ddl(connection_id, database, table, cx)
                                },
                            ));
                        }
                    }
                }
            }
        }

        if let Some(column_reference) = column_reference_at_offset(&text, offset) {
            if let Some(qualifier) = &column_reference.qualifier {
                let derived_tables = derived_tables_at_offset(&text, offset);
                if let Some(derived) = derived_tables
                    .iter()
                    .find(|derived| derived.alias.eq_ignore_ascii_case(qualifier))
                {
                    if let Some((schema, table, real_column)) = derived
                        .projections
                        .get(&column_reference.column.to_ascii_lowercase())
                        .cloned()
                    {
                        let database = self
                            .store
                            .read_with(cx, |store, _| {
                                let conn = store
                                    .connections
                                    .iter()
                                    .find(|c| c.config.id == self.connection_id)?;
                                schema.clone().or_else(|| {
                                    conn.config
                                        .database
                                        .clone()
                                        .filter(|database| !database.is_empty())
                                        .or_else(|| conn.expanded_databases.keys().next().cloned())
                                })
                            })
                            .ok()
                            .flatten();
                        if let Some(database) = database {
                            let word_start = snapshot.anchor_before(column_reference.start);
                            let word_end = snapshot.anchor_after(column_reference.end);
                            let connection_id = self.connection_id;
                            return Some(self.spawn_ddl_navigation(
                                buffer.clone(),
                                word_start..word_end,
                                Some(real_column),
                                cx,
                                move |store, cx| {
                                    store.get_table_ddl(connection_id, database, table, cx)
                                },
                            ));
                        }
                    }
                }
            }

            let from_tables = from_tables_at_offset(&text, offset);
            let candidates: Vec<(Option<String>, String)> = match &column_reference.qualifier {
                Some(qualifier) => {
                    match crate::sql_completion_provider::resolve_table_ref(qualifier, &from_tables)
                    {
                        Some(table_ref) => {
                            vec![(table_ref.schema.clone(), table_ref.name.clone())]
                        }
                        None => vec![(None, qualifier.clone())],
                    }
                }
                None => from_tables
                    .iter()
                    .map(|table_ref| (table_ref.schema.clone(), table_ref.name.clone()))
                    .collect(),
            };

            let column = column_reference.column.clone();
            let resolved = self
                .store
                .read_with(cx, |store, _| {
                    let conn = store
                        .connections
                        .iter()
                        .find(|c| c.config.id == self.connection_id)?;
                    let default_database = conn
                        .config
                        .database
                        .clone()
                        .filter(|database| !database.is_empty())
                        .or_else(|| conn.expanded_databases.keys().next().cloned());
                    let resolve_database =
                        |schema: Option<String>| schema.or_else(|| default_database.clone());

                    let chosen = if candidates.len() <= 1 {
                        candidates.into_iter().next()
                    } else {
                        // A bare column with several FROM tables: keep only the
                        // table whose cached columns actually contain it, and
                        // navigate only when that leaves a single owner.
                        let mut owners = candidates
                            .into_iter()
                            .filter(|(schema, table)| {
                                let Some(database) = resolve_database(schema.clone()) else {
                                    return false;
                                };
                                conn.expanded_tables
                                    .get(&(database, table.clone()))
                                    .is_some_and(|columns| {
                                        columns.iter().any(|c| c.name.eq_ignore_ascii_case(&column))
                                    })
                            })
                            .collect::<Vec<_>>();
                        if owners.len() == 1 {
                            owners.pop()
                        } else {
                            None
                        }
                    }?;

                    let (schema, table) = chosen;
                    let database = resolve_database(schema)?;
                    Some((database, table))
                })
                .ok()
                .flatten();

            // Fall through to bare-table navigation when the token cannot be
            // tied to a known column, since any word token reaches this branch.
            if let Some((database, table)) = resolved {
                let word_start = snapshot.anchor_before(column_reference.start);
                let word_end = snapshot.anchor_after(column_reference.end);
                let connection_id = self.connection_id;
                let focus_column = column_reference.column;
                return Some(self.spawn_ddl_navigation(
                    buffer.clone(),
                    word_start..word_end,
                    Some(focus_column),
                    cx,
                    move |store, cx| store.get_table_ddl(connection_id, database, table, cx),
                ));
            }
        }

        if let Some(table_reference) = table_reference_at_offset(&text, offset) {
            let database_opt = table_reference.database;
            let table_name = table_reference.table;

            let resolved = self
                .store
                .read_with(cx, |store, _| {
                    let conn = store
                        .connections
                        .iter()
                        .find(|c| c.config.id == self.connection_id)?;
                    let db = database_opt.clone().or_else(|| {
                        conn.config
                            .database
                            .clone()
                            .or_else(|| conn.expanded_databases.keys().next().cloned())
                    })?;
                    Some((db, table_name.clone()))
                })
                .ok()
                .flatten();
            if let Some((database, table)) = resolved {
                let word_start = snapshot.anchor_before(table_reference.start);
                let word_end = snapshot.anchor_after(table_reference.end);
                let connection_id = self.connection_id;
                return Some(self.spawn_ddl_navigation(
                    buffer.clone(),
                    word_start..word_end,
                    None,
                    cx,
                    move |store, cx| store.get_table_ddl(connection_id, database, table, cx),
                ));
            }
        }

        None
    }

    fn range_for_rename(
        &self,
        _buffer: &Entity<Buffer>,
        _position: Anchor,
        _cx: &mut App,
    ) -> Task<anyhow::Result<Option<std::ops::Range<Anchor>>>> {
        Task::ready(Ok(None))
    }

    fn perform_rename(
        &self,
        _buffer: &Entity<Buffer>,
        _position: Anchor,
        _new_name: String,
        _cx: &mut App,
    ) -> Option<Task<anyhow::Result<ProjectTransaction>>> {
        None
    }
}

enum AstDefinitionTarget {
    Table {
        database: String,
        table: String,
    },
    Database {
        database: String,
    },
    Column {
        database: String,
        table: String,
        column: String,
    },
}

impl DbSemanticsProvider {
    /// Resolves the token at `offset` via the AST scope-stack binder
    /// (`sql_binder`), reading only the connection's cached schema -- never a
    /// live connection. Returns `None` when the statement doesn't fully parse,
    /// the token doesn't resolve to a concrete table/column, or no database
    /// can be determined (no explicit qualifier and no connection default);
    /// each case is the caller's signal to fall back to the heuristic scanner.
    fn ast_definition_target(
        &self,
        text: &str,
        offset: usize,
        cx: &App,
    ) -> Option<(AstDefinitionTarget, std::ops::Range<usize>)> {
        self.store
            .read_with(cx, |store, _| {
                let conn = store
                    .connections
                    .iter()
                    .find(|c| c.config.id == self.connection_id)?;
                let default_database = conn
                    .config
                    .database
                    .clone()
                    .filter(|database| !database.is_empty())
                    .or_else(|| conn.expanded_databases.keys().next().cloned());
                let (target, range) = crate::sql_binder::resolve_navigation_at(
                    text,
                    conn.config.driver,
                    offset,
                    default_database.as_deref(),
                    conn,
                )?;
                let target = match target {
                    crate::sql_binder::NavigationTarget::Table {
                        database, table, ..
                    } => AstDefinitionTarget::Table {
                        database: database.or(default_database)?,
                        table,
                    },
                    crate::sql_binder::NavigationTarget::Database { database, .. } => {
                        AstDefinitionTarget::Database { database }
                    }
                    crate::sql_binder::NavigationTarget::Column {
                        database,
                        table,
                        column,
                        ..
                    } => AstDefinitionTarget::Column {
                        database: database.or(default_database)?,
                        table,
                        column,
                    },
                };
                Some((target, range))
            })
            .ok()
            .flatten()
    }

    fn spawn_ddl_navigation(
        &self,
        source_buffer: Entity<Buffer>,
        origin_range: std::ops::Range<Anchor>,
        focus_column: Option<String>,
        cx: &mut App,
        make_ddl_task: impl FnOnce(
            &mut DatabaseStore,
            &mut Context<DatabaseStore>,
        ) -> Task<anyhow::Result<String>>
        + 'static,
    ) -> Task<anyhow::Result<Option<Vec<LocationLink>>>> {
        let store = self.store.clone();
        let workspace = self.workspace.clone();
        cx.spawn(async move |cx| {
            let ddl_task = store.update(cx, |store, cx| make_ddl_task(store, cx))?;
            let ddl = ddl_task.await?;

            let language_task = workspace.read_with(cx, |ws, _| {
                ws.app_state().languages.language_for_name("SQL")
            })?;
            let language = language_task.await.ok();

            let target_offset = focus_column
                .as_deref()
                .and_then(|column| find_column_definition_offset(&ddl, column))
                .unwrap_or(0);

            let ddl_buffer = workspace.update(cx, |ws, cx| {
                ws.project().update(cx, |project, cx| {
                    project.create_local_buffer(&ddl, language, false, cx)
                })
            })?;

            let target_anchor = ddl_buffer.read_with(cx, |buf, _| buf.anchor_before(target_offset));

            Ok(Some(vec![LocationLink {
                origin: Some(Location {
                    buffer: source_buffer,
                    range: origin_range,
                }),
                target: Location {
                    buffer: ddl_buffer,
                    range: target_anchor..target_anchor,
                },
            }]))
        })
    }
}

// Byte offsets of every top-level `;` in `text` -- i.e. semicolons that are
// NOT inside a `'...'` or `"..."` string literal. Both SQL's doubled-quote
// escape (`''`/`""`) and a backslash escape are honored, so a `;` embedded in
// a value (e.g. a PHP-serialized string like `a:9:{i:60;i:1;...}`) is never
// mistaken for a statement boundary. Comments are not tracked here (only
// `skip_leading_whitespace_and_comments` handles those, at the very start of
// a statement) -- a `;` inside a `-- ...`/`/* ... */` comment mid-statement
// is a rare enough case to leave as a known gap. Operating on bytes is safe:
// the characters checked (`'`, `"`, `;`, `\`) are all ASCII, so they can
// never appear as part of a multi-byte UTF-8 continuation sequence.
fn unquoted_semicolon_offsets(text: &str) -> Vec<usize> {
    let bytes = text.as_bytes();
    let mut offsets = Vec::new();
    let mut quote: Option<u8> = None;
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        match quote {
            Some(q) => {
                if byte == b'\\' && index + 1 < bytes.len() {
                    index += 2;
                    continue;
                }
                if byte == q {
                    if bytes.get(index + 1) == Some(&q) {
                        index += 2;
                        continue;
                    }
                    quote = None;
                }
                index += 1;
            }
            None => {
                if byte == b'\'' || byte == b'"' {
                    quote = Some(byte);
                } else if byte == b';' {
                    offsets.push(index);
                }
                index += 1;
            }
        }
    }
    offsets
}

// The `;`-delimited statement bounds containing byte offset `cursor`, aware
// of string-literal-quoted semicolons via `unquoted_semicolon_offsets`. This
// is the single shared implementation for every "which statement is the
// cursor in" resolution (run-at-cursor, FK/table/database reference
// look-up, completion scoping, INSERT column context) -- do not reintroduce
// a local `rfind(';')`/`find(';')` pair, which silently breaks on any value
// containing a semicolon.
fn statement_bounds_at_offset(text: &str, cursor: usize) -> Range<usize> {
    let cursor = cursor.min(text.len());
    let offsets = unquoted_semicolon_offsets(text);
    let start = offsets
        .iter()
        .rev()
        .find(|&&offset| offset < cursor)
        .map(|&offset| offset + 1)
        .unwrap_or(0);
    let end = offsets
        .iter()
        .find(|&&offset| offset >= cursor)
        .copied()
        .unwrap_or(text.len());
    start..end
}

// A cursor sitting immediately after a statement's own `;` (e.g. the caret
// left at end-of-line right after typing or clicking there) must still
// resolve to that statement, not the next one -- otherwise `rfind`/`find`
// below would put the boundary semicolon on the "before cursor" side and
// hand back the following statement instead. `;` is ASCII, so stepping back
// one byte can never land inside a multi-byte UTF-8 sequence.
fn rewind_past_own_semicolon(text: &str, cursor: usize) -> usize {
    let cursor = cursor.min(text.len());
    if cursor > 0 && text.as_bytes().get(cursor - 1) == Some(&b';') {
        cursor - 1
    } else {
        cursor
    }
}

// Returns the `;`-delimited SQL statement that contains the byte offset
// `cursor`, trimmed. `;` is ASCII so byte scanning stays on char boundaries.
#[cfg(test)]
fn statement_at_cursor(text: &str, cursor: usize) -> String {
    let cursor = rewind_past_own_semicolon(text, cursor);
    let bounds = statement_bounds_at_offset(text, cursor);
    text[bounds].trim().to_string()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SqlStatementRun {
    pub(crate) sql: String,
    start_row: u32,
    end_row: u32,
}

fn row_for_byte_offset(text: &str, offset: usize) -> u32 {
    text.as_bytes()
        .get(..offset.min(text.len()))
        .unwrap_or_default()
        .iter()
        .filter(|&&byte| byte == b'\n')
        .count() as u32
}

fn trim_sql_range(text: &str, range: Range<usize>) -> Option<Range<usize>> {
    let start = range.start.min(text.len());
    let end = range.end.min(text.len());
    if start >= end {
        return None;
    }
    let segment = text.get(start..end)?;
    let leading = segment.len() - segment.trim_start().len();
    let trailing = segment.len() - segment.trim_end().len();
    let trimmed_start = start + leading;
    let trimmed_end = end.saturating_sub(trailing);
    (trimmed_start < trimmed_end).then_some(trimmed_start..trimmed_end)
}

fn statement_range_at_cursor(text: &str, cursor: usize) -> Option<Range<usize>> {
    let cursor = rewind_past_own_semicolon(text, cursor);
    trim_sql_range(text, statement_bounds_at_offset(text, cursor))
}

// Finds the byte offset, relative to `s`, of the first byte that is not
// leading whitespace or a leading comment (`-- ...` to end of line, or
// `/* ... */`, including multi-line block comments). Returns `s.len()` if
// `s` is nothing but whitespace/comments.
//
// This only recognizes `--`/`/*` at a position where a new token is
// expected (right after whitespace or a prior comment), so it deliberately
// does not special-case `--`/`/*` appearing inside a string literal at the
// very start of a statement -- a statement essentially never begins with a
// string literal, so a full SQL tokenizer would be overkill here.
fn skip_leading_whitespace_and_comments(s: &str) -> usize {
    let bytes = s.as_bytes();
    let mut offset = 0;
    loop {
        let after_whitespace = s[offset..].len() - s[offset..].trim_start().len();
        offset += after_whitespace;
        if bytes.get(offset..offset + 2) == Some(b"--") {
            let line_end = s[offset..]
                .find('\n')
                .map(|i| offset + i + 1)
                .unwrap_or(s.len());
            offset = line_end;
            continue;
        }
        if bytes.get(offset..offset + 2) == Some(b"/*") {
            let comment_end = s[offset + 2..]
                .find("*/")
                .map(|i| offset + 2 + i + 2)
                .unwrap_or(s.len());
            offset = comment_end;
            continue;
        }
        break;
    }
    offset
}

pub(crate) fn statement_runs_in_range(text: &str, range: Range<usize>) -> Vec<SqlStatementRun> {
    let Some(range) = trim_sql_range(text, range) else {
        return Vec::new();
    };
    let mut statements = Vec::new();
    let mut start = range.start;
    let push_statement = |statements: &mut Vec<SqlStatementRun>, trimmed: Range<usize>| {
        let Some(segment) = text.get(trimmed.clone()) else {
            return;
        };
        let content_offset = trimmed.start + skip_leading_whitespace_and_comments(segment);
        if content_offset >= trimmed.end {
            // The whole interval is comments/whitespace -- nothing to run.
            return;
        }
        let Some(sql) = text.get(content_offset..trimmed.end) else {
            return;
        };
        statements.push(SqlStatementRun {
            sql: sql.to_string(),
            start_row: row_for_byte_offset(text, content_offset),
            end_row: row_for_byte_offset(text, trimmed.end),
        });
    };
    for semicolon in unquoted_semicolon_offsets(text) {
        if semicolon < range.start {
            continue;
        }
        if semicolon >= range.end {
            break;
        }
        if let Some(trimmed) = trim_sql_range(text, start..semicolon) {
            push_statement(&mut statements, trimmed);
        }
        start = semicolon + 1;
    }
    if let Some(trimmed) = trim_sql_range(text, start..range.end) {
        push_statement(&mut statements, trimmed);
    }
    statements
}

/// A node in the rendered connection tree. Folders nest other folders and
/// connections; a connection node carries the index into the `connections`
/// slice so the caller renders the original `ActiveConnection` without cloning.
enum TreeNode {
    Folder {
        folder: Folder,
        children: Vec<TreeNode>,
    },
    Connection {
        index: usize,
    },
}

// Builds the nested tree under `parent_id`: folders and connections
// interleaved by a single shared `order` space (tie-broken by name), then
// recursed into. This must match `DatabaseStore::combined_siblings`'s
// ordering exactly -- that is the space `reposition_item`'s precise
// before/after drag-and-drop writes into, so a folder and a connection can
// end up interleaved (e.g. connection, folder, connection); rendering
// folders as a separate group first would silently disagree with where a
// drag-and-drop reorder just placed things. Pure so the grouping/sorting is
// unit-tested without the GPUI render path. `depth` bounds recursion so
// cyclic stored data cannot loop forever.
fn build_folder_tree(
    folders: &[Folder],
    connections: &[ActiveConnection],
    parent_id: Option<FolderId>,
    depth: usize,
) -> Vec<TreeNode> {
    if depth > db_client::MAX_FOLDER_DEPTH {
        return Vec::new();
    }

    enum ChildItem<'a> {
        Folder(&'a Folder),
        Connection(usize),
    }

    let mut items: Vec<ChildItem> = folders
        .iter()
        .filter(|f| f.parent_id == parent_id)
        .map(ChildItem::Folder)
        .chain(
            connections
                .iter()
                .enumerate()
                .filter(|(_, c)| c.config.folder_id == parent_id)
                .map(|(index, _)| ChildItem::Connection(index)),
        )
        .collect();
    items.sort_by(|a, b| {
        let (order_a, name_a) = match a {
            ChildItem::Folder(folder) => (folder.order, folder.name.to_lowercase()),
            ChildItem::Connection(index) => (
                connections[*index].config.order,
                connections[*index].config.label.to_lowercase(),
            ),
        };
        let (order_b, name_b) = match b {
            ChildItem::Folder(folder) => (folder.order, folder.name.to_lowercase()),
            ChildItem::Connection(index) => (
                connections[*index].config.order,
                connections[*index].config.label.to_lowercase(),
            ),
        };
        order_a.cmp(&order_b).then_with(|| name_a.cmp(&name_b))
    });

    let mut nodes: Vec<TreeNode> = Vec::new();
    for item in items {
        match item {
            ChildItem::Folder(folder) => nodes.push(TreeNode::Folder {
                folder: folder.clone(),
                children: build_folder_tree(folders, connections, Some(folder.id), depth + 1),
            }),
            ChildItem::Connection(index) => nodes.push(TreeNode::Connection { index }),
        }
    }

    nodes
}

// Flattens the top-level folder/connection tree into the same order
// `render_tree_nodes` paints it in, skipping the children of collapsed
// folders. Keyboard SelectNext/Previous/First/Last walk this list. Pure so
// the ordering can be unit-tested without a GPUI render pass.
fn flatten_navigable_entities(
    nodes: &[TreeNode],
    connections: &[ActiveConnection],
    collapsed_folders: &HashSet<FolderId>,
) -> Vec<SelectedEntity> {
    let mut flat = Vec::new();
    for node in nodes {
        match node {
            TreeNode::Folder { folder, children } => {
                flat.push(SelectedEntity::Folder(folder.id));
                if !collapsed_folders.contains(&folder.id) {
                    flat.extend(flatten_navigable_entities(
                        children,
                        connections,
                        collapsed_folders,
                    ));
                }
            }
            TreeNode::Connection { index } => {
                // `render_tree_nodes` looks the index up in `connections` and
                // silently skips it if stale; do the same here so the flattened
                // list never gets out of sync with what is actually painted.
                if let Some(conn) = connections.get(*index) {
                    flat.push(SelectedEntity::Connection(conn.config.id));
                }
            }
        }
    }
    flat
}

// Appends a generated sample query (e.g. from a "New Query" context menu
// entry) to the end of the console's existing text, so a quick-preview
// action never silently discards whatever the user already had open. An
// empty (or whitespace-only) console is replaced outright rather than
// prefixed with a stray blank line.
fn append_sample_query(existing: &str, sample: &str) -> String {
    if existing.trim().is_empty() {
        sample.to_string()
    } else {
        format!("{}\n{}", existing.trim_end(), sample)
    }
}

// The console file's extension drives Zed's normal language-by-extension
// detection, so this is what actually gives a connection's console syntax
// highlighting. MongoDB's shell queries (`db.<collection>.<method>(...)`)
// are JS method-chaining, not SQL, so they get the JavaScript grammar;
// every other driver here speaks something SQL or SQL-like enough that the
// SQL grammar highlights it reasonably.
fn console_file_extension(driver: DatabaseDriver) -> &'static str {
    match driver {
        DatabaseDriver::MongoDB => "js",
        // Redis commands (`GET key`, `HGETALL key`, ...) are whitespace
        // tokens, not SQL syntax at all -- the SQL grammar has no keywords
        // that match, so it would highlight (or fail to highlight) them
        // incorrectly. Plain text is the honest choice: no grammar to
        // misapply.
        DatabaseDriver::Redis => "txt",
        _ => "sql",
    }
}

// A persistent scratch file per connection, kept in the config dir so it
// survives restarts and never needs an explicit save.
fn connection_query_path(
    connection_id: ConnectionId,
    label: &str,
    driver: DatabaseDriver,
) -> std::path::PathBuf {
    let sanitized: String = label
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let id = connection_id.simple().to_string();
    let short = &id[..id.len().min(8)];
    paths::config_dir()
        .join("db_client")
        .join("queries")
        .join(format!(
            "{sanitized}-{short}.{}",
            console_file_extension(driver)
        ))
}

/// Serializes an export bundle and writes it to `path` off the main thread.
fn write_export_bundle(
    path: std::path::PathBuf,
    folders: Vec<Folder>,
    connections: Vec<ConnectionConfig>,
    consoles: Vec<ConsoleFile>,
    secrets: Option<EncryptedSecrets>,
    cx: &mut App,
) {
    let bundle = ExportBundle {
        version: BUNDLE_VERSION,
        folders,
        connections,
        consoles,
        secrets,
    };
    cx.background_spawn(async move {
        if let Some(json) = serde_json::to_vec_pretty(&bundle).log_err() {
            std::fs::write(&path, json).log_err();
        }
    })
    .detach();
}

fn show_db_toast<C: gpui::AppContext>(
    workspace: &WeakEntity<Workspace>,
    id: &'static str,
    message: &str,
    cx: &mut C,
) {
    let toast = Toast::new(NotificationId::named(id.into()), message.to_string());
    workspace
        .update(cx, |workspace, cx| workspace.show_toast(toast, cx))
        .log_err();
}

/// Resolves which connection a focused editor's SQL console belongs to, without
/// depending on the editor addon. A file-backed console editor may not carry the
/// addon (the addon can be lost when the editor is reopened or restored from a
/// session), so the file path is the authoritative signal: console files live in
/// a known directory and embed the connection id prefix in their name. Falling
/// back to the path means Ctrl+Enter cannot silently degrade to the inline
/// assistant just because the addon is missing.
fn console_connection_for_editor(
    editor: &Entity<Editor>,
    store: &Entity<DatabaseStore>,
    cx: &App,
) -> Option<ConnectionId> {
    if let Some(addon) = editor.read(cx).addon::<DbQueryEditorAddon>() {
        return Some(addon.connection_id);
    }

    let buffer = editor.read(cx).buffer().read(cx).as_singleton()?;
    let abs_path = buffer.read(cx).file()?.as_local()?.abs_path(cx);
    let known_ids: Vec<ConnectionId> = store
        .read(cx)
        .connections()
        .iter()
        .map(|connection| connection.config.id)
        .collect();
    connection_id_from_console_path(&abs_path, &known_ids)
}

// Maps a console file path back to its connection id. Console files live in a
// fixed directory and embed the first 8 chars of the connection id at the end of
// the stem (see `connection_query_path`). Pure so the live resolution path is
// unit-tested without a real file-backed buffer.
fn connection_id_from_console_path(
    abs_path: &std::path::Path,
    known_ids: &[ConnectionId],
) -> Option<ConnectionId> {
    let queries_dir = paths::config_dir().join("db_client").join("queries");
    if abs_path.parent() != Some(queries_dir.as_path()) {
        return None;
    }
    let stem = abs_path.file_stem()?.to_str()?;
    let id_prefix = stem.get(stem.len().saturating_sub(8)..)?;
    known_ids.iter().copied().find(|id| {
        id.simple()
            .to_string()
            .get(..8)
            .is_some_and(|prefix| prefix == id_prefix)
    })
}

pub(crate) fn connection_env_color(
    store: &WeakEntity<DatabaseStore>,
    connection_id: ConnectionId,
    cx: &App,
) -> Option<gpui::Hsla> {
    store.upgrade().and_then(|store_entity| {
        store_entity
            .read(cx)
            .connections()
            .iter()
            .find(|c| c.config.id == connection_id)
            .and_then(|c| c.config.env_color.as_deref().and_then(parse_env_color))
            .map(gpui::Hsla::from)
    })
}

fn install_db_editor_features(
    editor: Entity<Editor>,
    store: WeakEntity<DatabaseStore>,
    workspace: WeakEntity<Workspace>,
    cx: &mut App,
) {
    let connection_id = store
        .upgrade()
        .and_then(|store_entity| console_connection_for_editor(&editor, &store_entity, cx));
    install_on_editor(editor.clone(), store.clone(), connection_id, cx);

    let Some(connection_id) = connection_id else {
        return;
    };

    let env_color = connection_env_color(&store, connection_id, cx);
    let validation_store = store.clone();
    editor.update(cx, |editor, cx| {
        if editor.addon::<DbQueryEditorAddon>().is_none() {
            editor.register_addon(DbQueryEditorAddon::new(connection_id));
        }
        editor.set_show_runnables(true, cx);
        editor.set_background_tint(env_color, cx);
        editor.set_semantics_provider(Some(Rc::new(DbSemanticsProvider {
            connection_id,
            store,
            workspace,
        })));
    });
    install_sql_validation(editor, validation_store, connection_id, cx);
}

/// A stable synthetic language-server id for diagnostics produced by the SQL
/// validator, which has no real language server behind it. Chosen far above
/// any id a real running language server would be assigned, to avoid ever
/// colliding with one.
const SQL_VALIDATOR_SERVER_ID: language::LanguageServerId =
    language::LanguageServerId(usize::MAX - 1000);

/// Idle time after the last edit before a validation pass runs, so typing
/// never triggers a parse+bind on every keystroke. `sql_validator::validate`
/// took ~4ms on a debug build for a deeply nested real production query
/// (three levels of subqueries, two derived tables, an `INSERT ... ON
/// DUPLICATE KEY UPDATE`) -- trivial next to this window, and only ever
/// matters once typing pauses.
const SQL_VALIDATION_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(500);

/// Revalidates `editor`'s buffer against the cached schema whenever it is
/// edited, debounced so a validation pass never runs mid-keystroke. Reads
/// only the in-memory schema cache (never a live connection), exactly like
/// completion and Ctrl+click navigation.
fn install_sql_validation(
    editor: Entity<Editor>,
    store: WeakEntity<DatabaseStore>,
    connection_id: ConnectionId,
    cx: &mut App,
) {
    let pending_validation: Rc<RefCell<Option<Task<()>>>> = Rc::new(RefCell::new(None));
    let trigger = {
        let editor = editor.clone();
        move |cx: &mut App| {
            let editor = editor.clone();
            let store = store.clone();
            let task = cx.spawn(async move |cx| {
                cx.background_executor()
                    .timer(SQL_VALIDATION_DEBOUNCE)
                    .await;
                run_sql_validation(&editor, &store, connection_id, cx).await;
            });
            *pending_validation.borrow_mut() = Some(task);
        }
    };
    trigger(cx);
    cx.subscribe(&editor, move |_editor, event, cx| {
        if matches!(event, EditorEvent::BufferEdited) {
            trigger(cx);
        }
    })
    .detach();
}

async fn run_sql_validation(
    editor: &Entity<Editor>,
    store: &WeakEntity<DatabaseStore>,
    connection_id: ConnectionId,
    cx: &mut AsyncApp,
) {
    let Some(buffer) = editor.read_with(cx, |editor, cx| editor.buffer().read(cx).as_singleton())
    else {
        return;
    };
    let text = buffer.read_with(cx, |buffer, _| buffer.text());
    // Reading the connection and validating happen inside one synchronous
    // closure so the `&ActiveConnection` borrow never needs to outlive it --
    // `validate` runs to completion and returns an owned `Vec` before the
    // closure (and the borrow) ends.
    let Ok(Some(diagnostics)) = store.read_with(cx, |store, _| {
        let conn = store
            .connections
            .iter()
            .find(|c| c.config.id == connection_id)?;
        let default_database = conn
            .config
            .database
            .clone()
            .filter(|database| !database.is_empty())
            .or_else(|| conn.expanded_databases.keys().next().cloned());
        Some(crate::sql_validator::validate(
            &text,
            conn.config.driver,
            default_database.as_deref(),
            conn,
        ))
    }) else {
        return;
    };
    apply_sql_diagnostics(&buffer, diagnostics, cx).await;
}

async fn apply_sql_diagnostics(
    buffer: &Entity<Buffer>,
    diagnostics: Vec<crate::sql_validator::SqlDiagnostic>,
    cx: &mut AsyncApp,
) {
    buffer.update(cx, |buffer, cx| {
        let snapshot = buffer.snapshot();
        let entries: Vec<_> = diagnostics
            .into_iter()
            .map(|diagnostic| {
                let start = snapshot.offset_to_point_utf16(diagnostic.range.start);
                let end = snapshot.offset_to_point_utf16(diagnostic.range.end);
                let severity = match diagnostic.level {
                    crate::sql_validator::DiagnosticLevel::Warning => {
                        language::DiagnosticSeverity::WARNING
                    }
                };
                language::DiagnosticEntry {
                    range: start..end,
                    diagnostic: language::Diagnostic {
                        source: Some("sql".to_string()),
                        severity,
                        message: diagnostic.message,
                        source_kind: language::DiagnosticSourceKind::Other,
                        ..Default::default()
                    },
                }
            })
            .collect();
        let diagnostic_set = language::DiagnosticSet::new(entries, &snapshot);
        buffer.update_diagnostics(SQL_VALIDATOR_SERVER_ID, diagnostic_set, cx);
    });
}

/// Opens the persistent SQL console for `connection_id`. The file lives on disk
/// (openable even when the database is not connected) and auto-saves whenever
/// the editor loses focus, so there is never a save prompt. Ctrl+Enter runs the
/// statement under the cursor against this connection.
pub fn open_new_sql_query(
    workspace: &mut Workspace,
    connection_id: ConnectionId,
    connection_label: String,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    open_sql_query_console(workspace, connection_id, connection_label, None, window, cx);
}

/// Like [`open_new_sql_query`], but appends `text` (a generated sample query,
/// e.g. from a "New Query" context menu entry) to the end of the connection's
/// existing persistent console instead of leaving it untouched. There is only
/// ever one query document per connection, so a generated sample joins
/// whatever is already there rather than opening a separate throwaway buffer.
pub fn open_sql_query_console_appending(
    workspace: &mut Workspace,
    connection_id: ConnectionId,
    connection_label: String,
    text: String,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    open_sql_query_console(
        workspace,
        connection_id,
        connection_label,
        Some(text),
        window,
        cx,
    );
}

fn open_sql_query_console(
    workspace: &mut Workspace,
    connection_id: ConnectionId,
    connection_label: String,
    append_text: Option<String>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let store_entity = DatabaseStore::global(cx);
    let driver = store_entity.as_ref().and_then(|store| {
        store
            .read(cx)
            .connections()
            .iter()
            .find(|connection| connection.config.id == connection_id)
            .map(|connection| connection.config.driver)
    });
    let store = store_entity.map(|store| store.downgrade());
    let workspace_handle = workspace.weak_handle();
    let path = connection_query_path(
        connection_id,
        &connection_label,
        driver.unwrap_or(DatabaseDriver::MySQL),
    );
    cx.spawn_in(window, async move |workspace, cx| {
        // Make sure the file exists before opening it (blocking fs off the main thread).
        let path_for_create = path.clone();
        cx.background_executor()
            .spawn(async move {
                if let Some(parent) = path_for_create.parent() {
                    std::fs::create_dir_all(parent).ok();
                }
                if !path_for_create.exists() {
                    std::fs::write(&path_for_create, b"").ok();
                }
            })
            .await;

        let open = workspace.update_in(cx, |workspace, window, cx| {
            workspace.open_abs_path(
                path.clone(),
                OpenOptions {
                    visible: Some(OpenVisible::None),
                    ..Default::default()
                },
                window,
                cx,
            )
        })?;
        let item = open.await?;

        workspace
            .update_in(cx, |_workspace, window, cx| {
                let Some(editor) = item.act_as::<Editor>(cx) else {
                    return;
                };
                editor.update(cx, |editor, cx| {
                    editor.register_addon(DbQueryEditorAddon::new(connection_id));
                    editor.set_show_runnables(true, cx);
                    if let Some(text) = append_text.as_deref().filter(|text| !text.is_empty()) {
                        let combined = append_sample_query(&editor.text(cx), text);
                        editor.set_text(combined, window, cx);
                        editor.move_to_end(&editor::actions::MoveToEnd, window, cx);
                    }
                });
                if let Some(store) = store.clone() {
                    install_db_editor_features(editor.clone(), store, workspace_handle.clone(), cx);
                }
                // Auto-save on focus loss: write the buffer back to its file so the
                // console never prompts to save.
                cx.subscribe(&editor, |workspace, editor, event, cx| {
                    if matches!(event, EditorEvent::Blurred) {
                        let project = workspace.project().clone();
                        if let Some(buffer) = editor.read(cx).buffer().read(cx).as_singleton() {
                            project
                                .update(cx, |project, cx| project.save_buffer(buffer, cx))
                                .detach_and_log_err(cx);
                        }
                    }
                })
                .detach();
            })
            .log_err();
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

/// Opens a SQL console for the active (or first connected) connection. Used by
/// the global NewQuery action; per-connection buttons call open_new_sql_query
/// directly with their own connection.
pub fn new_query_for_active_connection(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(panel) = workspace.panel::<DatabasePanel>(cx) else {
        return;
    };
    let store = panel.read(cx).store.clone();
    let connection = {
        let store_ref = store.read(cx);
        store_ref
            .active_connection()
            .or_else(|| {
                store_ref
                    .connections()
                    .iter()
                    .find(|c| matches!(c.status, ConnectionStatus::Connected))
            })
            .or_else(|| store_ref.connections().first())
            .cloned()
    };
    let Some(connection) = connection else {
        return;
    };
    let id = connection.config.id;
    let label = connection.config.label.clone();
    if connection.config.driver == DatabaseDriver::Aerospike {
        let default_namespace = connection.config.database.unwrap_or_default();
        let view = cx.new(|cx| {
            AerospikeView::new(
                store,
                workspace.weak_handle(),
                id,
                label.into(),
                default_namespace,
                window,
                cx,
            )
        });
        workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
        return;
    }
    open_new_sql_query(workspace, id, label, window, cx);
}

// Finds the result tab bound to `connection_id` in `pane`, or creates one, then
// activates it. One reused tab per connection so re-running a query updates its
// own tab instead of stacking new ones.
fn show_result_in_pane(
    pane: &Entity<Pane>,
    connection_id: ConnectionId,
    title: SharedString,
    env_color: Option<gpui::Hsla>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ResultView> {
    // Reuse this connection's tab only if it is not pinned. A pinned tab is left
    // untouched, so the query lands in a fresh, numbered tab instead.
    let existing = pane.read(cx).items_of_type::<ResultView>().find(|view| {
        let view = view.read(cx);
        view.connection_id() == Some(connection_id) && !view.is_pinned()
    });

    if let Some(view) = existing {
        let index = pane
            .read(cx)
            .items()
            .position(|item| item.item_id() == view.item_id());
        if let Some(index) = index {
            pane.update(cx, |pane, cx| {
                pane.activate_item(index, true, true, window, cx);
            });
        }
        return view;
    }

    // Number additional tabs for the same connection: "… — Results", then
    // "… — Results 2", "… — Results 3", and so on.
    let existing_count = pane
        .read(cx)
        .items_of_type::<ResultView>()
        .filter(|view| view.read(cx).connection_id() == Some(connection_id))
        .count();
    let title: SharedString = if existing_count == 0 {
        title
    } else {
        format!("{title} {}", existing_count + 1).into()
    };

    let view = cx.new(|cx| {
        ResultView::new(title, cx)
            .with_connection(connection_id)
            .with_env_color(env_color)
    });
    pane.update(cx, |pane, cx| {
        pane.add_item(Box::new(view.clone()), true, true, None, window, cx);
    });
    view
}

fn is_ident_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

/// Extracts named query parameters (`:name` or `?name`) in first-seen order,
/// ignoring occurrences inside string literals and PostgreSQL `::type` casts.
pub fn extract_query_parameters(sql: &str) -> Vec<String> {
    let chars: Vec<char> = sql.chars().collect();
    let mut parameters = Vec::new();
    let mut seen = HashSet::default();
    let mut index = 0;
    let mut quote: Option<char> = None;
    while index < chars.len() {
        let ch = chars[index];
        if let Some(active_quote) = quote {
            if ch == active_quote {
                quote = None;
            }
            index += 1;
            continue;
        }
        if ch == '\'' || ch == '"' {
            quote = Some(ch);
            index += 1;
            continue;
        }
        if ch == ':' && chars.get(index + 1) == Some(&':') {
            index += 2;
            continue;
        }
        if ch == ':' || ch == '?' {
            let mut end = index + 1;
            while end < chars.len() && is_ident_char(chars[end]) {
                end += 1;
            }
            if end > index + 1 {
                let name: String = chars[index + 1..end].iter().collect();
                if seen.insert(name.clone()) {
                    parameters.push(name);
                }
                index = end;
                continue;
            }
        }
        index += 1;
    }
    parameters
}

fn format_parameter_value(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.eq_ignore_ascii_case("null") {
        return "NULL".to_string();
    }
    if trimmed.parse::<i64>().is_ok() || trimmed.parse::<f64>().is_ok() {
        return trimmed.to_string();
    }
    format!("'{}'", value.replace('\'', "''"))
}

/// Substitutes named parameters with their (quoted) values. Numeric values and
/// `NULL` are inserted verbatim; everything else is single-quoted and escaped.
/// Tokens without a provided value are left untouched.
pub fn substitute_query_parameters(sql: &str, values: &HashMap<String, String>) -> String {
    let chars: Vec<char> = sql.chars().collect();
    let mut output = String::with_capacity(sql.len());
    let mut index = 0;
    let mut quote: Option<char> = None;
    while index < chars.len() {
        let ch = chars[index];
        if let Some(active_quote) = quote {
            output.push(ch);
            if ch == active_quote {
                quote = None;
            }
            index += 1;
            continue;
        }
        if ch == '\'' || ch == '"' {
            quote = Some(ch);
            output.push(ch);
            index += 1;
            continue;
        }
        if ch == ':' && chars.get(index + 1) == Some(&':') {
            output.push_str("::");
            index += 2;
            continue;
        }
        if ch == ':' || ch == '?' {
            let mut end = index + 1;
            while end < chars.len() && is_ident_char(chars[end]) {
                end += 1;
            }
            if end > index + 1 {
                let name: String = chars[index + 1..end].iter().collect();
                if let Some(value) = values.get(&name) {
                    output.push_str(&format_parameter_value(value));
                } else {
                    output.extend(&chars[index..end]);
                }
                index = end;
                continue;
            }
        }
        output.push(ch);
        index += 1;
    }
    output
}

pub fn run_current_sql_query(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    run_sql_from_editor(workspace, window, cx, |sql| sql);
}

pub fn format_current_sql_query(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(panel) = workspace.panel::<DatabasePanel>(cx) else {
        cx.propagate();
        return;
    };
    let store = panel.read(cx).store.clone();
    let active_item = workspace.active_item(cx);
    let editor = active_item.and_then(|item| item.act_as::<Editor>(cx));
    let Some(editor) = editor else {
        cx.propagate();
        return;
    };
    let Some(connection_id) = console_connection_for_editor(&editor, &store, cx) else {
        cx.propagate();
        return;
    };

    let driver = {
        let store_ref = store.read(cx);
        store_ref
            .connections()
            .iter()
            .find(|c| c.config.id == connection_id)
            .or_else(|| store_ref.active_connection())
            .map(|c| c.config.driver)
    };
    let Some(driver) = driver else {
        return;
    };

    let text = editor.read(cx).text(cx);
    let Some(formatted) = crate::sql_ast::format_sql(&text, driver) else {
        return;
    };
    if formatted == text {
        return;
    }
    editor.update(cx, |editor, cx| {
        editor.set_text(formatted, window, cx);
    });
}

/// Streams the current console statement's full result straight to a CSV
/// file, bypassing the results grid entirely -- unlike `run_current_sql_query`,
/// the row count here is not capped by `MAX_RESULT_ROWS`. Only the single
/// statement at the cursor (or the current selection) runs; named parameters
/// are not supported yet (the params-prompt flow `run_sql_from_editor` uses
/// is UI-heavy and out of scope for a first version -- a parameterized
/// statement is reported as an error instead of silently running unsubstituted).
pub fn execute_current_sql_query_to_file(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(panel) = workspace.panel::<DatabasePanel>(cx) else {
        cx.propagate();
        return;
    };
    let store = panel.read(cx).store.clone();
    let active_item = workspace.active_item(cx);
    let editor = active_item.and_then(|item| item.act_as::<Editor>(cx));
    let Some(editor) = editor else {
        cx.propagate();
        return;
    };
    let Some(bound_connection_id) = console_connection_for_editor(&editor, &store, cx) else {
        cx.propagate();
        return;
    };

    let sql = editor.update(cx, |editor, cx| {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let selection = editor.selections.newest_anchor();
        let start = selection.start.to_offset(&snapshot).0;
        let end = selection.end.to_offset(&snapshot).0;
        let full = editor.text(cx);
        if start != end {
            let (lo, hi) = (start.min(end), start.max(end));
            full.get(lo..hi).map(str::to_string)
        } else {
            let cursor = selection.head().to_offset(&snapshot).0;
            statement_range_at_cursor(&full, cursor)
                .map(|range| statement_runs_in_range(&full, range))
                .and_then(|runs| runs.into_iter().next())
                .map(|run| run.sql)
        }
    });
    let Some(sql) = sql else { return };
    let sql = sql.trim().trim_end_matches(';').trim().to_string();
    if sql.is_empty() {
        return;
    }
    if !extract_query_parameters(&sql).is_empty() {
        workspace.show_toast(
            Toast::new(
                NotificationId::named("db-execute-to-file-params".into()),
                "Execute to File does not support named parameters yet -- run the query \
                 normally first to substitute them, or remove the placeholders.",
            ),
            cx,
        );
        return;
    }

    let connection = {
        let store_ref = store.read(cx);
        store_ref
            .connections()
            .iter()
            .find(|c| c.config.id == bound_connection_id)
            .or_else(|| store_ref.active_connection())
            .and_then(|c| {
                c.provider.clone().map(|provider| {
                    (
                        c.config.database.clone().unwrap_or_default(),
                        c.config.label.clone(),
                        provider,
                    )
                })
            })
    };
    let Some((db_name, conn_label, provider)) = connection else {
        return;
    };

    let home = paths::home_dir().to_path_buf();
    let default_format = crate::execute_to_file::ExecuteToFileFormat::Csv;
    let path_rx = cx.prompt_for_new_path(&home, Some(default_format.default_file_name()));
    cx.spawn_in(window, async move |workspace, cx| {
        let Some(output_path) = path_rx.await.log_err().and_then(|r| r.log_err()).flatten() else {
            return;
        };
        let format = crate::execute_to_file::ExecuteToFileFormat::for_path(&output_path);
        workspace
            .update(cx, |workspace, cx| {
                if let Some(panel) = workspace.panel::<DatabasePanel>(cx) {
                    panel.update(cx, |panel, cx| {
                        panel.start_export_to_file(
                            format!("Execute to File: {conn_label}").into(),
                            provider,
                            db_name,
                            sql,
                            output_path,
                            format,
                            cx,
                        );
                    });
                }
            })
            .ok();
    })
    .detach();
}

/// Returns the active editor's on-disk absolute path, if it has one (a saved
/// `.sql` file, not a scratch/unsaved buffer).
fn active_editor_file_path(editor: &Entity<Editor>, cx: &App) -> Option<std::path::PathBuf> {
    let buffer = editor.read(cx).buffer().read(cx).as_singleton()?;
    let abs_path = buffer.read(cx).file()?.as_local()?.abs_path(cx);
    Some(abs_path)
}

/// Saves the active SQL file's currently active connection (and database) as
/// its run configuration, so `run_sql_file` can later target the same data
/// source with one action regardless of what connection happens to be active
/// at that later time.
pub fn save_run_configuration(
    workspace: &mut Workspace,
    _window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(panel) = workspace.panel::<DatabasePanel>(cx) else {
        cx.propagate();
        return;
    };
    let store = panel.read(cx).store.clone();
    let active_item = workspace.active_item(cx);
    let Some(editor) = active_item.and_then(|item| item.act_as::<Editor>(cx)) else {
        cx.propagate();
        return;
    };
    let Some(file_path) = active_editor_file_path(&editor, cx) else {
        workspace.show_toast(
            Toast::new(
                NotificationId::named("db-run-config-no-file".into()),
                "Save this file before setting a run configuration for it.".to_string(),
            ),
            cx,
        );
        return;
    };

    let resolved = {
        let store_ref = store.read(cx);
        let connection_id = console_connection_for_editor(&editor, &store, cx)
            .or_else(|| store_ref.active_connection().map(|c| c.config.id));
        connection_id.and_then(|id| {
            store_ref
                .connections()
                .iter()
                .find(|c| c.config.id == id)
                .map(|c| (id, c.config.label.clone(), c.config.database.clone()))
        })
    };
    let Some((connection_id, label, database)) = resolved else {
        workspace.show_toast(
            Toast::new(
                NotificationId::named("db-run-config-no-connection".into()),
                "Open a console for a connection (or make one active) before saving a run configuration."
                    .to_string(),
            ),
            cx,
        );
        return;
    };

    let name = file_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("SQL file")
        .to_string();
    store.update(cx, |store, cx| {
        store.set_run_configuration(
            RunConfiguration {
                id: uuid::Uuid::new_v4(),
                name,
                file_path,
                connection_id,
                database,
            },
            cx,
        );
    });
    workspace.show_toast(
        Toast::new(
            NotificationId::named("db-run-config-saved".into()),
            format!("Run configuration saved: always runs against \"{label}\"."),
        ),
        cx,
    );
}

/// Runs the active SQL file against its saved run configuration, ignoring
/// whichever connection is currently active in the panel. Fails gracefully
/// (a toast, not a panic) when the file has no saved configuration or when
/// the configuration's connection no longer exists.
pub fn run_sql_file(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(panel) = workspace.panel::<DatabasePanel>(cx) else {
        cx.propagate();
        return;
    };
    let store = panel.read(cx).store.clone();
    let active_item = workspace.active_item(cx);
    let Some(editor) = active_item.and_then(|item| item.act_as::<Editor>(cx)) else {
        cx.propagate();
        return;
    };
    let Some(file_path) = active_editor_file_path(&editor, cx) else {
        cx.propagate();
        return;
    };

    let config = store
        .read(cx)
        .run_configuration_for_path(&file_path)
        .cloned();
    let Some(config) = config else {
        workspace.show_toast(
            Toast::new(
                NotificationId::named("db-run-config-missing".into()),
                "No run configuration saved for this file yet — use Save Run Configuration first."
                    .to_string(),
            ),
            cx,
        );
        return;
    };

    let connection_exists = store
        .read(cx)
        .connections()
        .iter()
        .any(|c| c.config.id == config.connection_id);
    if !connection_exists {
        workspace.show_toast(
            Toast::new(
                NotificationId::named("db-run-config-stale".into()),
                "This run configuration's connection no longer exists. Save a new one for this file."
                    .to_string(),
            ),
            cx,
        );
        return;
    }

    store.update(cx, |store, cx| {
        if let Some(database) = config.database.clone() {
            store.set_connection_database(config.connection_id, database, cx);
        }
    });
    editor.update(cx, |editor, _cx| {
        editor.register_addon(DbQueryEditorAddon::new(config.connection_id));
    });
    run_current_sql_query(workspace, window, cx);
}

/// Toggles whether the active console shows each statement's result inline,
/// below the statement, in addition to the existing bottom-dock results tab
/// (which is never removed by this toggle -- turning it back off simply
/// clears the inline blocks and leaves the bottom-dock behavior exactly as
/// it always was). This is a per-console setting, not global: two open
/// consoles can independently be in inline or bottom-dock-only mode.
pub fn toggle_inline_results(
    workspace: &mut Workspace,
    _window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let active_item = workspace.active_item(cx);
    let Some(editor) = active_item.and_then(|item| item.act_as::<Editor>(cx)) else {
        cx.propagate();
        return;
    };
    let Some(bound_connection_id) = editor
        .read(cx)
        .addon::<DbQueryEditorAddon>()
        .map(|addon| addon.connection_id)
    else {
        cx.propagate();
        return;
    };

    let controller = editor.update(cx, |editor, cx| {
        let weak_editor = cx.weak_entity();
        if editor.addon::<DbQueryEditorAddon>().is_none() {
            editor.register_addon(DbQueryEditorAddon::new(bound_connection_id));
        }
        let controller = editor
            .addon::<DbQueryEditorAddon>()
            .and_then(|addon| addon.inline_results.clone())
            .unwrap_or_else(|| {
                cx.new(|_| crate::inline_results::InlineResultsController::new(weak_editor))
            });
        if let Some(addon) = editor.addon_mut::<DbQueryEditorAddon>() {
            addon.inline_results = Some(controller.clone());
        }
        controller
    });
    controller.update(cx, |controller, cx| controller.toggle(cx));
}

pub fn explain_current_sql_query(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(panel) = workspace.panel::<DatabasePanel>(cx) else {
        cx.propagate();
        return;
    };
    let store = panel.read(cx).store.clone();
    let active_item = workspace.active_item(cx);
    let editor = active_item.and_then(|item| item.act_as::<Editor>(cx));
    let Some(editor) = editor else {
        cx.propagate();
        return;
    };
    let Some(connection_id) = console_connection_for_editor(&editor, &store, cx) else {
        cx.propagate();
        return;
    };

    let statement = editor.update(cx, |editor, cx| {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let selection = editor.selections.newest_anchor();
        let start = selection.start.to_offset(&snapshot).0;
        let end = selection.end.to_offset(&snapshot).0;
        let full = editor.text(cx);
        let range = if start != end {
            (start.min(end), start.max(end))
        } else {
            let cursor = selection.head().to_offset(&snapshot).0;
            match statement_range_at_cursor(&full, cursor) {
                Some(range) => (range.start, range.end),
                None => return String::new(),
            }
        };
        statement_runs_in_range(&full, range.0..range.1)
            .into_iter()
            .next()
            .map(|run| run.sql)
            .unwrap_or_default()
    });
    let sql = statement.trim().trim_end_matches(';').trim().to_string();
    if sql.is_empty() {
        return;
    }

    let resolved = {
        let store_ref = store.read(cx);
        store_ref
            .connections()
            .iter()
            .find(|c| c.config.id == connection_id)
            .or_else(|| store_ref.active_connection())
            .map(|c| {
                (
                    c.config.id,
                    c.config.database.clone().unwrap_or_default(),
                    c.config.driver,
                )
            })
    };
    let Some((id, database, driver)) = resolved else {
        return;
    };
    panel.update(cx, |panel, cx| {
        panel.open_explain_plan(id, database, driver, sql, window, cx);
    });
}

fn run_sql_from_editor(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    transform: impl Fn(String) -> String,
) {
    let panel = workspace.panel::<DatabasePanel>(cx);
    let panel = match panel {
        Some(p) => p,
        None => {
            cx.propagate();
            return;
        }
    };
    let store = panel.read(cx).store.clone();

    let active_item = workspace.active_item(cx);
    let editor = active_item.and_then(|item| item.act_as::<Editor>(cx));
    let editor = match editor {
        Some(e) => e,
        None => {
            // Not an editor — let the keystroke fall through.
            cx.propagate();
            return;
        }
    };

    // This binding fires for every full editor, so we must decide whether the
    // focused editor is one of our SQL consoles. Resolve by addon first, then by
    // the console file path, so a console whose addon was lost still runs the
    // query instead of falling through to the inline assistant. If it is not a
    // console, propagate so normal editors keep their default ctrl-enter.
    let bound_connection_id = match console_connection_for_editor(&editor, &store, cx) {
        Some(id) => id,
        None => {
            cx.propagate();
            return;
        }
    };

    let mut statements = editor.update(cx, |editor, cx| {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let selection = editor.selections.newest_anchor();
        let start = selection.start.to_offset(&snapshot).0;
        let end = selection.end.to_offset(&snapshot).0;
        let full = editor.text(cx);
        if start != end {
            let (lo, hi) = (start.min(end), start.max(end));
            statement_runs_in_range(&full, lo..hi)
        } else {
            let cursor = selection.head().to_offset(&snapshot).0;
            statement_range_at_cursor(&full, cursor)
                .map(|range| statement_runs_in_range(&full, range))
                .unwrap_or_default()
        }
    });
    if statements.is_empty() {
        return;
    }
    statements = statements
        .into_iter()
        .filter_map(|statement| {
            let sql = transform(statement.sql);
            let sql = sql.trim().trim_end_matches(';').trim().to_string();
            (!sql.is_empty()).then_some(SqlStatementRun {
                sql,
                start_row: statement.start_row,
                end_row: statement.end_row,
            })
        })
        .collect();
    if statements.is_empty() {
        return;
    }

    // A statement with named parameters can't run as-is; collect values from
    // the user first, then re-run the substituted SQL.
    let combined_sql = statements
        .iter()
        .map(|statement| statement.sql.clone())
        .collect::<Vec<_>>()
        .join(";\n");
    if !extract_query_parameters(&combined_sql).is_empty() {
        panel.update(cx, |panel, cx| {
            panel.open_query_params_prompt(bound_connection_id, combined_sql.clone(), window, cx);
        });
        return;
    }

    editor.update(cx, |editor, cx| {
        if let Some(addon) = editor.addon_mut::<DbQueryEditorAddon>() {
            addon.clear_query_markers();
            cx.notify();
        }
    });

    let connection = {
        let store_ref = store.read(cx);
        let resolved = store_ref
            .connections()
            .iter()
            .find(|c| c.config.id == bound_connection_id)
            .or_else(|| store_ref.active_connection())
            .or_else(|| {
                store_ref
                    .connections()
                    .iter()
                    .find(|c| matches!(c.status, ConnectionStatus::Connected))
            });
        resolved.map(|c| {
            (
                c.config.id,
                c.config.database.clone().unwrap_or_default(),
                c.config.label.clone(),
                matches!(c.status, ConnectionStatus::Connected),
                c.config
                    .env_color
                    .as_deref()
                    .and_then(parse_env_color)
                    .map(gpui::Hsla::from),
            )
        })
    };
    let (conn_id, db_name, conn_label, connected, env_color) = match connection {
        Some(connection) => connection,
        None => return,
    };

    // Results open as tabs in the terminal panel's pane — the same bottom-dock
    // area where terminals open — with one reused tab per connection. Reveal the
    // panel so the first query shows up.
    let Some(terminal_panel) = workspace.panel::<TerminalPanel>(cx) else {
        return;
    };
    let Some(pane) = terminal_panel.read(cx).pane() else {
        return;
    };
    let result_view = show_result_in_pane(
        &pane,
        conn_id,
        format!("{conn_label} — Results").into(),
        env_color,
        window,
        cx,
    );
    result_view.update(cx, |view, cx| {
        view.clear_table_context();
        view.set_loading(cx);
    });
    workspace.open_panel::<TerminalPanel>(window, cx);

    cx.spawn_in(window, async move |_workspace, cx| {
        let mut connected = connected;
        for statement in statements {
            let inline_view = editor.update(cx, |editor, cx| {
                let controller = editor
                    .addon::<DbQueryEditorAddon>()
                    .and_then(|addon| addon.inline_results.clone());
                if let Some(addon) = editor.addon_mut::<DbQueryEditorAddon>() {
                    addon.mark_query(statement.start_row, QueryExecutionStatus::Running);
                    cx.notify();
                }
                controller.and_then(|controller| {
                    controller.update(cx, |controller, cx| {
                        controller.begin_statement(statement.start_row, statement.end_row, cx)
                    })
                })
            });
            result_view.update(cx, |view, cx| {
                view.clear_table_context();
                view.set_loading(cx);
            });

            // Auto-connect if the database is not connected (covers both a fresh
            // session and a dropped connection).
            if !connected {
                let connect = store.update(cx, |store, cx| store.connect(conn_id, cx));
                if let Err(err) = connect.await {
                    editor.update(cx, |editor, cx| {
                        if let Some(addon) = editor.addon_mut::<DbQueryEditorAddon>() {
                            addon.mark_query(statement.start_row, QueryExecutionStatus::Error);
                            cx.notify();
                        }
                    });
                    let message = format!(
                        "Could not connect to '{conn_label}': {}",
                        format_query_error(&err)
                    );
                    if let Some(inline_view) = &inline_view {
                        inline_view.update(cx, |view, cx| view.set_error(message.clone(), cx));
                    }
                    result_view.update(cx, |view, cx| view.set_error(message, cx));
                    return anyhow::Ok(());
                }
                connected = true;
            }

            let sql = statement.sql.clone();
            let task = store.update(cx, |store, cx| {
                store.execute_query(conn_id, db_name.clone(), sql.clone(), cx)
            });
            let result = task.await;
            match result {
                Ok(result) => {
                    editor.update(cx, |editor, cx| {
                        if let Some(addon) = editor.addon_mut::<DbQueryEditorAddon>() {
                            addon.mark_query(statement.start_row, QueryExecutionStatus::Success);
                            cx.notify();
                        }
                    });
                    if let Some(inline_view) = &inline_view {
                        inline_view.update(cx, |view, cx| view.set_result(&result, cx));
                    }
                    let table_context = select_table_reference(&sql).map(|reference| {
                        let database = reference.database.unwrap_or_else(|| db_name.clone());
                        (database, reference.table)
                    });
                    let store = store.downgrade();
                    result_view.update_in(cx, |view, window, cx| {
                        view.set_query_result(
                            store.clone(),
                            conn_id,
                            db_name.clone(),
                            sql.clone(),
                            result,
                            cx,
                        );
                        if let Some((database, table)) = table_context {
                            view.set_table_context(store, conn_id, database, table, window, cx);
                        }
                    })?;
                }
                Err(err) => {
                    editor.update(cx, |editor, cx| {
                        if let Some(addon) = editor.addon_mut::<DbQueryEditorAddon>() {
                            addon.mark_query(statement.start_row, QueryExecutionStatus::Error);
                            cx.notify();
                        }
                    });
                    if let Some(inline_view) = &inline_view {
                        inline_view
                            .update(cx, |view, cx| view.set_error(format_query_error(&err), cx));
                    }
                    result_view.update(cx, |view, cx| view.set_error(format_query_error(&err), cx));
                    return anyhow::Ok(());
                }
            }
        }
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

pub struct DatabasePanel {
    focus_handle: FocusHandle,
    store: Entity<DatabaseStore>,
    workspace: WeakEntity<Workspace>,
    history_expanded: bool,
    table_filter_editor: Entity<Editor>,
    collapsed_folders: HashSet<FolderId>,
    collapsed_connections: HashSet<ConnectionId>,
    editing_folder: Option<EditingFolder>,
    drag_target: Option<DropTarget>,
    views_expanded: HashSet<(ConnectionId, String)>,
    procedures_expanded: HashSet<(ConnectionId, String)>,
    sequences_expanded: HashSet<(ConnectionId, String)>,
    events_expanded: HashSet<(ConnectionId, String)>,
    table_indexes_expanded: HashSet<(ConnectionId, String, String)>,
    table_fks_expanded: HashSet<(ConnectionId, String, String)>,
    table_triggers_expanded: HashSet<(ConnectionId, String, String)>,
    server_objects_expanded: HashSet<ConnectionId>,
    server_users: HashMap<ConnectionId, Vec<(String, String)>>,
    table_filter_is_regex: bool,
    selected_tree_node: Option<SelectedTreeNode>,
    selected_entity: Option<SelectedEntity>,
    dump: DumpUiState,
    export: ExportUiState,
    context_menu: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
    tree_scroll_handle: ScrollHandle,
    // True until the first `DatabaseStoreEvent` after load carries the
    // disk-loaded folders/connections. There's nothing to restore no
    // persisted collapse state was found, so as soon as real ids exist,
    // everything is collapsed by default instead of the empty-set default of
    // "nothing collapsed" (fully expanded).
    initial_collapse_pending: bool,
    pending_tree_state_serialization: Task<Option<()>>,
    _subscriptions: Vec<Subscription>,
}

/// A tree row that can be the target of a click-to-select highlight,
/// independent of `active_connection_id` (which connection new queries
/// target) and `selected_tree_node` (which database/table an action like
/// Quick Documentation targets).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectedEntity {
    Folder(FolderId),
    Connection(ConnectionId),
}

#[derive(Default, Serialize, Deserialize)]
struct SerializedDatabasePanel {
    #[serde(default)]
    collapsed_folders: HashSet<FolderId>,
    #[serde(default)]
    collapsed_connections: HashSet<ConnectionId>,
    #[serde(default)]
    views_expanded: HashSet<(ConnectionId, String)>,
    #[serde(default)]
    procedures_expanded: HashSet<(ConnectionId, String)>,
    #[serde(default)]
    sequences_expanded: HashSet<(ConnectionId, String)>,
    #[serde(default)]
    events_expanded: HashSet<(ConnectionId, String)>,
    #[serde(default)]
    table_indexes_expanded: HashSet<(ConnectionId, String, String)>,
    #[serde(default)]
    table_fks_expanded: HashSet<(ConnectionId, String, String)>,
    #[serde(default)]
    table_triggers_expanded: HashSet<(ConnectionId, String, String)>,
    #[serde(default)]
    server_objects_expanded: HashSet<ConnectionId>,
}

/// Background native-dump state owned by the panel: the visible task list for the
/// status strip and the spawned task handles (kept alive so a dump keeps running,
/// and droppable to cancel one). `next_id` labels each task. The settings dialog
/// itself is a workspace modal, not held here.
#[derive(Default)]
struct DumpUiState {
    tasks: Vec<DumpTask>,
    runners: Vec<(usize, Task<()>)>,
    next_id: usize,
}

/// Background execute-to-file state, mirroring `DumpUiState` (same status
/// strip, same `DumpTask`/`DumpStatus` types -- a streaming export and a
/// native dump are both "a long background job that produces a file"). Each
/// runner also carries the `cancelled` flag its `FileRowSink` checks, so
/// dismissing a still-running export can ask it to stop and clean up its
/// partial file instead of only dropping the task handle.
#[derive(Default)]
struct ExportUiState {
    tasks: Vec<DumpTask>,
    runners: Vec<(usize, Task<()>, Arc<std::sync::atomic::AtomicBool>)>,
    next_id: usize,
}

/// A compact local timestamp for dump output filenames. Falls back to UTC when
/// the local offset can't be resolved (e.g. a sandboxed environment).
fn dump_timestamp() -> String {
    let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
    let format = format_description!("[year][month][day]-[hour][minute][second]");
    now.format(&format).unwrap_or_default()
}

/// The native dump tool is driver specific; only MySQL and PostgreSQL have one.
fn dump_menu_label(driver: DatabaseDriver) -> Option<&'static str> {
    match driver {
        DatabaseDriver::MySQL => Some("Export with mysqldump…"),
        DatabaseDriver::PostgreSQL => Some("Export with pg_dump…"),
        _ => None,
    }
}

/// Whether `driver` speaks a schema shape (fixed table/column layout,
/// SQL-style `WHERE`-filterable queries, `ALTER TABLE`-able DDL) that
/// full-text search and schema diff can work against. MongoDB's collections
/// have neither a fixed schema nor a SQL query surface, so both features are
/// hidden for it rather than generating a query that can only fail.
fn supports_relational_query_features(driver: DatabaseDriver) -> bool {
    !matches!(driver, DatabaseDriver::MongoDB)
}

/// Label for the button/tooltip that opens a new query console, tailored to
/// the driver's actual query language — MongoDB's console takes mongo shell
/// commands (`db.<collection>.find({...})`), not SQL.
fn new_query_button_label(driver: DatabaseDriver) -> &'static str {
    match driver {
        DatabaseDriver::MongoDB => "Queries",
        DatabaseDriver::Aerospike => "Get / Put / Scan",
        DatabaseDriver::Redis => "Commands",
        _ => "SQL Queries",
    }
}

/// The tree node the keyboard acts on. A node is a database (table = None) or a
/// table within it, so Go to DDL / Quick Documentation / Show Diagram know their
/// target without a pointer event.
#[derive(Clone)]
struct SelectedTreeNode {
    connection_id: ConnectionId,
    database: String,
    table: Option<String>,
}

/// What is being dragged in the connection tree. Folders and connections are the
/// only draggable items; both can be dropped onto a folder or the top level.
#[derive(Clone, Copy)]
enum DraggedDbItem {
    Connection(ConnectionId),
    Folder(FolderId),
}

/// The folder currently highlighted as a drop target, or the top level when the
/// pointer is over empty panel space. `Folder` reparents-into (append at the
/// end); the `Before*`/`After*` variants insert at the hovered row's exact
/// sibling position instead, set when the pointer is over that row's top or
/// bottom edge rather than its body.
#[derive(Clone, Copy, PartialEq)]
enum DropTarget {
    Folder(FolderId),
    TopLevel,
    BeforeFolder(FolderId),
    AfterFolder(FolderId),
    BeforeConnection(ConnectionId),
    AfterConnection(ConnectionId),
}

impl DraggedDbItem {
    fn as_tree_item_ref(self) -> TreeItemRef {
        match self {
            DraggedDbItem::Connection(id) => TreeItemRef::Connection(id),
            DraggedDbItem::Folder(id) => TreeItemRef::Folder(id),
        }
    }
}

/// Inline folder rename/create: an editor overlaid on the folder row. The
/// subscription commits the name when the editor loses focus.
struct EditingFolder {
    id: FolderId,
    editor: Entity<Editor>,
    _subscription: Subscription,
}

/// Drag image shown under the cursor while dragging a tree item.
struct DraggedDbItemPreview {
    label: SharedString,
    icon: IconName,
}

impl Render for DraggedDbItemPreview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .gap_1()
            .items_center()
            .px_2()
            .py_0p5()
            .rounded_md()
            .bg(cx.theme().colors().elevated_surface_background)
            .border_1()
            .border_color(cx.theme().colors().border)
            .child(
                Icon::new(self.icon)
                    .size(IconSize::Small)
                    .color(Color::Muted),
            )
            .child(Label::new(self.label.clone()).size(LabelSize::Small))
    }
}

/// Workspace modal that collects values for a parameterized query, then hands the
/// final SQL to `on_run` (the panel opens it in a console tab).
type MasterPasswordCallback = Arc<dyn Fn(Option<String>, &mut Window, &mut App)>;

/// A workspace modal that collects a master password used to encrypt (on export)
/// or decrypt (on import) the connection passwords carried in a portable bundle.
/// The callback gets `Some(password)` on confirm, or `None` when the user skips.
struct MasterPasswordView {
    focus_handle: FocusHandle,
    title: SharedString,
    subtitle: SharedString,
    confirm_label: SharedString,
    allow_skip: bool,
    editor: Entity<Editor>,
    on_result: MasterPasswordCallback,
}

impl MasterPasswordView {
    fn new(
        title: impl Into<SharedString>,
        subtitle: impl Into<SharedString>,
        confirm_label: impl Into<SharedString>,
        allow_skip: bool,
        on_result: MasterPasswordCallback,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Master password", window, cx);
            editor
        });
        Self {
            focus_handle: cx.focus_handle(),
            title: title.into(),
            subtitle: subtitle.into(),
            confirm_label: confirm_label.into(),
            allow_skip,
            editor,
            on_result,
        }
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let password = self.editor.read(cx).text(cx);
        if password.is_empty() {
            return;
        }
        (self.on_result.clone())(Some(password), window, cx);
        cx.emit(DismissEvent);
    }

    fn skip(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        (self.on_result.clone())(None, window, cx);
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for MasterPasswordView {}

impl ModalView for MasterPasswordView {}

impl Focusable for MasterPasswordView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for MasterPasswordView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("MasterPassword")
            .track_focus(&self.focus_handle)
            .elevation_3(cx)
            .w(px(440.))
            .p_3()
            .gap_2()
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| this.confirm(window, cx)))
            .child(crate::widgets::dialog_header(
                self.title.clone(),
                "master-password-close",
                cx.listener(|_, _, _, cx| cx.emit(DismissEvent)),
            ))
            .child(
                Label::new(self.subtitle.clone())
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(div().child(self.editor.clone()))
            .child(
                h_flex()
                    .w_full()
                    .justify_end()
                    .gap_2()
                    .when(self.allow_skip, |row| {
                        row.child(
                            Button::new("master-password-skip", "Skip passwords")
                                .on_click(cx.listener(|this, _, window, cx| this.skip(window, cx))),
                        )
                    })
                    .child(
                        Button::new("master-password-cancel", "Cancel")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        Button::new("master-password-confirm", self.confirm_label.clone())
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, window, cx| this.confirm(window, cx))),
                    ),
            )
    }
}

type QueryRunCallback = Arc<dyn Fn(String, &mut Window, &mut App)>;

struct QueryParamsView {
    focus_handle: FocusHandle,
    sql: String,
    inputs: Vec<(String, Entity<Editor>)>,
    on_run: QueryRunCallback,
}

impl QueryParamsView {
    fn new(
        sql: String,
        on_run: QueryRunCallback,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let inputs = extract_query_parameters(&sql)
            .into_iter()
            .map(|name| {
                let editor = cx.new(|cx| {
                    let mut editor = Editor::single_line(window, cx);
                    editor.set_placeholder_text("value", window, cx);
                    editor
                });
                (name, editor)
            })
            .collect();
        Self {
            focus_handle: cx.focus_handle(),
            sql,
            inputs,
            on_run,
        }
    }

    fn run(&mut self, strip: bool, window: &mut Window, cx: &mut Context<Self>) {
        let final_sql = if strip {
            self.sql.clone()
        } else {
            let values: HashMap<String, String> = self
                .inputs
                .iter()
                .map(|(name, editor)| (name.clone(), editor.read(cx).text(cx)))
                .collect();
            substitute_query_parameters(&self.sql, &values)
        };
        (self.on_run.clone())(final_sql, window, cx);
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for QueryParamsView {}

impl ModalView for QueryParamsView {}

impl Focusable for QueryParamsView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for QueryParamsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows: Vec<_> = self
            .inputs
            .iter()
            .map(|(name, editor)| {
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .w(px(120.))
                            .child(Label::new(name.clone()).size(LabelSize::Small)),
                    )
                    .child(div().flex_1().child(editor.clone()))
            })
            .collect();
        v_flex()
            .key_context("QueryParams")
            .track_focus(&self.focus_handle)
            .elevation_3(cx)
            .w(px(420.))
            .p_3()
            .gap_2()
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .child(crate::widgets::dialog_header(
                "Query Parameters",
                "query-params-close",
                cx.listener(|_, _, _, cx| cx.emit(DismissEvent)),
            ))
            .child(v_flex().gap_1().children(rows))
            .child(
                h_flex()
                    .w_full()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("params-cancel", "Cancel")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        Button::new("params-strip", "Run as-is").on_click(
                            cx.listener(|this, _, window, cx| this.run(true, window, cx)),
                        ),
                    )
                    .child(
                        Button::new("params-run", "Run")
                            .style(ButtonStyle::Filled)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.run(false, window, cx)),
                            ),
                    ),
            )
    }
}

/// One place `RenameTableView` found the table's current name referenced --
/// either an open console buffer (excerpt is the matching line's text) or a
/// cached database-side routine/trigger/event source (excerpt is a short
/// snippet), so the user can see what needs attention before confirming.
struct RenameTableUsage {
    label: SharedString,
    excerpt: SharedString,
}

type RenameConfirmCallback = Arc<dyn Fn(String, &mut Window, &mut App)>;

/// Renames a table with a find-usages preview: open console buffers that
/// reference the table are listed and rewritten in place on confirm (a real
/// text substitution of the matched identifier, not just a warning), while
/// database-side routine/trigger/event source can only be flagged as needing
/// a manual update -- changing that requires its own `ALTER PROCEDURE`-style
/// statement per object, which is out of scope for a table rename.
struct RenameTableView {
    focus_handle: FocusHandle,
    old_name: SharedString,
    new_name_editor: Entity<Editor>,
    usages: Vec<RenameTableUsage>,
    has_db_side_usages: bool,
    on_confirm: RenameConfirmCallback,
}

impl RenameTableView {
    fn new(
        old_name: String,
        usages: Vec<RenameTableUsage>,
        has_db_side_usages: bool,
        on_confirm: RenameConfirmCallback,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let new_name_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(old_name.clone(), window, cx);
            editor.select_all(&Default::default(), window, cx);
            editor
        });
        Self {
            focus_handle: cx.focus_handle(),
            old_name: old_name.into(),
            new_name_editor,
            usages,
            has_db_side_usages,
            on_confirm,
        }
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let new_name = self.new_name_editor.read(cx).text(cx).trim().to_string();
        if new_name.is_empty() || new_name == self.old_name.as_ref() {
            cx.emit(DismissEvent);
            return;
        }
        (self.on_confirm.clone())(new_name, window, cx);
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for RenameTableView {}

impl ModalView for RenameTableView {}

impl Focusable for RenameTableView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for RenameTableView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let usage_rows: Vec<_> = self
            .usages
            .iter()
            .map(|usage| {
                v_flex()
                    .gap_0p5()
                    .child(Label::new(usage.label.clone()).size(LabelSize::Small))
                    .child(
                        Label::new(usage.excerpt.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
            })
            .collect();
        v_flex()
            .key_context("RenameTable")
            .track_focus(&self.focus_handle)
            .elevation_3(cx)
            .w(px(460.))
            .p_3()
            .gap_2()
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .child(crate::widgets::dialog_header(
                "Rename Table",
                "rename-table-close",
                cx.listener(|_, _, _, cx| cx.emit(DismissEvent)),
            ))
            .child(
                v_flex()
                    .gap_1()
                    .child(Label::new("New name").size(LabelSize::Small))
                    .child(
                        div()
                            .w_full()
                            .rounded_md()
                            .border_1()
                            .border_color(cx.theme().colors().border)
                            .px_2()
                            .py_1()
                            .child(self.new_name_editor.clone()),
                    ),
            )
            .when(!self.usages.is_empty(), |el| {
                el.child(
                    v_flex()
                        .gap_1()
                        .child(
                            Label::new(format!(
                                "{} usage{} found",
                                self.usages.len(),
                                if self.usages.len() == 1 { "" } else { "s" }
                            ))
                            .size(LabelSize::Small),
                        )
                        .child(
                            v_flex()
                                .id("rename-table-usages")
                                .gap_1()
                                .max_h(px(180.))
                                .overflow_y_scroll()
                                .children(usage_rows),
                        ),
                )
            })
            .when(self.has_db_side_usages, |el| {
                el.child(
                    Label::new(
                        "Some routines, triggers, or events also reference this table -- \
                         their source is not rewritten automatically and needs a manual update.",
                    )
                    .size(LabelSize::XSmall)
                    .color(Color::Warning),
                )
            })
            .child(
                h_flex()
                    .w_full()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("rename-table-cancel", "Cancel")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        div()
                            .debug_selector(|| "rename-table-confirm".into())
                            .child(
                                Button::new("rename-table-confirm", "Rename")
                                    .style(ButtonStyle::Filled)
                                    .on_click(
                                        cx.listener(|this, _, window, cx| this.confirm(window, cx)),
                                    ),
                            ),
                    ),
            )
    }
}

/// Workspace modal that picks the second table for the Compare Data flow, then
/// hands the chosen table to `on_pick` (the panel runs the comparison).
type ComparePickCallback = Arc<dyn Fn(String, &mut Window, &mut App)>;

struct ComparePickerView {
    focus_handle: FocusHandle,
    left_table: String,
    candidates: Vec<String>,
    on_pick: ComparePickCallback,
}

impl ComparePickerView {
    fn new(
        left_table: String,
        candidates: Vec<String>,
        on_pick: ComparePickCallback,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            left_table,
            candidates,
            on_pick,
        }
    }
}

impl EventEmitter<DismissEvent> for ComparePickerView {}

impl ModalView for ComparePickerView {}

impl Focusable for ComparePickerView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ComparePickerView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows: Vec<_> = self
            .candidates
            .iter()
            .cloned()
            .map(|candidate| {
                Button::new(
                    SharedString::from(format!("cmp-{candidate}")),
                    candidate.clone(),
                )
                .style(ButtonStyle::Subtle)
                .full_width()
                .on_click(cx.listener(move |this, _, window, cx| {
                    (this.on_pick.clone())(candidate.clone(), window, cx);
                    cx.emit(DismissEvent);
                }))
            })
            .collect();
        v_flex()
            .key_context("ComparePicker")
            .track_focus(&self.focus_handle)
            .elevation_3(cx)
            .w(px(420.))
            .max_h(px(480.))
            .p_3()
            .gap_2()
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .child(crate::widgets::dialog_header(
                SharedString::from(format!("Compare {} with", self.left_table)),
                "compare-pick-close",
                cx.listener(|_, _, _, cx| cx.emit(DismissEvent)),
            ))
            .when(rows.is_empty(), |column| {
                column.child(
                    Label::new("No other tables to compare")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .child(
                v_flex()
                    .id("compare-candidates")
                    .gap_0p5()
                    .max_h(px(360.))
                    .overflow_y_scroll()
                    .children(rows),
            )
    }
}

struct QuickDocView {
    focus_handle: FocusHandle,
    title: SharedString,
    columns: Vec<ColumnInfo>,
}

impl QuickDocView {
    fn new(
        title: SharedString,
        columns: Vec<ColumnInfo>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            title,
            columns,
        }
    }
}

impl EventEmitter<DismissEvent> for QuickDocView {}

impl ModalView for QuickDocView {}

impl Focusable for QuickDocView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for QuickDocView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows: Vec<_> =
            self.columns
                .iter()
                .map(|col| {
                    let overlays = DatabasePanel::column_overlay_icons(col, false);
                    h_flex()
                        .items_center()
                        .gap_1()
                        .py_0p5()
                        .child(Label::new(col.name.clone()).size(LabelSize::XSmall))
                        .child(
                            Label::new(col.data_type.clone())
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                        .children(overlays.into_iter().map(|(icon, color)| {
                            Icon::new(icon).size(IconSize::XSmall).color(color)
                        }))
                })
                .collect();
        v_flex()
            .key_context("QuickDoc")
            .track_focus(&self.focus_handle)
            .elevation_3(cx)
            .w(px(360.))
            .max_h(px(480.))
            .overflow_hidden()
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .child(crate::widgets::dialog_header(
                self.title.clone(),
                "quick-doc-close",
                cx.listener(|_, _, _, cx| cx.emit(DismissEvent)),
            ))
            .child(
                div()
                    .id("quick-doc-body")
                    .flex_1()
                    .overflow_y_scroll()
                    .px_2()
                    .py_1()
                    .child(v_flex().children(rows)),
            )
    }
}

impl DatabasePanel {
    fn serialization_key(workspace: &Workspace) -> Option<String> {
        workspace
            .database_id()
            .map(|id| i64::from(id).to_string())
            .or(workspace.session_id())
            .map(|id| format!("DatabasePanel-{id:?}"))
    }

    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        let serialized_panel = match workspace
            .read_with(&cx, |workspace, _| Self::serialization_key(workspace))
            .ok()
            .flatten()
        {
            Some(serialization_key) => {
                let kvp = cx.update(|_, cx| KeyValueStore::global(cx))?;
                cx.background_spawn(async move { kvp.read_kvp(&serialization_key) })
                    .await
                    .context("loading database panel")
                    .log_err()
                    .flatten()
                    .map(|panel| serde_json::from_str::<SerializedDatabasePanel>(&panel))
                    .transpose()
                    .log_err()
                    .flatten()
            }
            None => None,
        };

        let result = workspace.update_in(&mut cx, |workspace, window, cx| {
            let store = cx.new(|cx| DatabaseStore::new(cx));
            cx.set_global(crate::store::GlobalDatabaseStore(store.clone()));
            let focus_handle = cx.focus_handle();
            let workspace_entity = cx.entity();
            let workspace_handle = workspace.weak_handle();
            let table_filter_editor = cx.new(|cx| {
                let mut ed = Editor::single_line(window, cx);
                ed.set_placeholder_text("Filter tables...", window, cx);
                ed
            });
            let initial_collapse_pending = serialized_panel.is_none();
            cx.new(|cx| {
                let store_subscription = cx.subscribe(
                    &store,
                    |this: &mut DatabasePanel,
                     _store: Entity<DatabaseStore>,
                     _event: &DatabaseStoreEvent,
                     cx: &mut Context<DatabasePanel>| {
                        this.apply_initial_collapse_if_needed(cx);
                        cx.notify();
                    },
                );
                let store_weak = store.downgrade();
                let workspace_weak = workspace_entity.downgrade();
                let workspace_subscription = cx.subscribe(
                    &workspace_entity,
                    move |_this: &mut DatabasePanel,
                          _workspace: Entity<Workspace>,
                          event: &WorkspaceEvent,
                          cx: &mut Context<DatabasePanel>| {
                        if let WorkspaceEvent::ItemAdded { item } = event {
                            if let Some(editor) = item.act_as::<Editor>(cx) {
                                install_db_editor_features(
                                    editor,
                                    store_weak.clone(),
                                    workspace_weak.clone(),
                                    cx,
                                );
                            }
                        }
                    },
                );
                let filter_subscription = cx.subscribe(
                    &table_filter_editor,
                    |_this: &mut DatabasePanel,
                     _editor: Entity<Editor>,
                     _event: &editor::EditorEvent,
                     cx: &mut Context<DatabasePanel>| {
                        cx.notify();
                    },
                );
                let restored = serialized_panel.unwrap_or_default();
                DatabasePanel {
                    focus_handle,
                    store,
                    workspace: workspace_handle,
                    history_expanded: false,
                    table_filter_editor,
                    collapsed_folders: restored.collapsed_folders,
                    collapsed_connections: restored.collapsed_connections,
                    editing_folder: None,
                    drag_target: None,
                    views_expanded: restored.views_expanded,
                    procedures_expanded: restored.procedures_expanded,
                    sequences_expanded: restored.sequences_expanded,
                    events_expanded: restored.events_expanded,
                    table_indexes_expanded: restored.table_indexes_expanded,
                    table_fks_expanded: restored.table_fks_expanded,
                    table_triggers_expanded: restored.table_triggers_expanded,
                    server_objects_expanded: restored.server_objects_expanded,
                    server_users: HashMap::default(),
                    table_filter_is_regex: false,
                    selected_tree_node: None,
                    selected_entity: None,
                    dump: DumpUiState::default(),
                    export: ExportUiState::default(),
                    context_menu: None,
                    tree_scroll_handle: ScrollHandle::new(),
                    initial_collapse_pending,
                    pending_tree_state_serialization: Task::ready(None),
                    _subscriptions: vec![
                        store_subscription,
                        workspace_subscription,
                        filter_subscription,
                    ],
                }
            })
        });
        result
    }

    /// Populates `collapsed_folders`/`collapsed_connections` with every id the
    /// store currently knows about, the first time real (disk-loaded) ids show
    /// up, when there was nothing persisted to restore. Without this, the
    /// empty default sets mean "nothing collapsed" i.e. the whole tree renders
    /// expanded on first launch (or after any persistence miss).
    fn apply_initial_collapse_if_needed(&mut self, cx: &mut Context<Self>) {
        if !self.initial_collapse_pending {
            return;
        }
        let store = self.store.read(cx);
        if store.folders().is_empty() && store.connections().is_empty() {
            return;
        }
        self.collapsed_folders = store.folders().iter().map(|f| f.id).collect();
        self.collapsed_connections = store.connections().iter().map(|c| c.config.id).collect();
        self.initial_collapse_pending = false;
    }

    fn serialize_tree_state(&mut self, cx: &mut Context<Self>) {
        let Some(serialization_key) = self
            .workspace
            .read_with(cx, |workspace, _| Self::serialization_key(workspace))
            .ok()
            .flatten()
        else {
            return;
        };
        let serialized = SerializedDatabasePanel {
            collapsed_folders: self.collapsed_folders.clone(),
            collapsed_connections: self.collapsed_connections.clone(),
            views_expanded: self.views_expanded.clone(),
            procedures_expanded: self.procedures_expanded.clone(),
            sequences_expanded: self.sequences_expanded.clone(),
            events_expanded: self.events_expanded.clone(),
            table_indexes_expanded: self.table_indexes_expanded.clone(),
            table_fks_expanded: self.table_fks_expanded.clone(),
            table_triggers_expanded: self.table_triggers_expanded.clone(),
            server_objects_expanded: self.server_objects_expanded.clone(),
        };
        let kvp = KeyValueStore::global(cx);
        self.pending_tree_state_serialization = cx.background_spawn(
            async move {
                kvp.write_kvp(serialization_key, serde_json::to_string(&serialized)?)
                    .await?;
                anyhow::Ok(())
            }
            .log_err(),
        );
    }

    fn open_add_connection_modal(&self, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        self.workspace
            .update(cx, |workspace, cx| {
                let view = cx.new(|cx| {
                    ConnectionView::new(window, cx).with_on_confirm(move |config, cx| {
                        store.update(cx, |store, cx| {
                            store.add_connection(config, cx);
                        });
                    })
                });
                workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
            })
            .log_err();
    }

    fn open_edit_connection_modal(
        &self,
        existing: ConnectionConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = self.store.clone();
        let original_id = existing.id;
        self.workspace
            .update(cx, |workspace, cx| {
                let view = cx.new(|cx| {
                    ConnectionView::new_with_config(&existing, window, cx).with_on_confirm(
                        move |mut config, cx| {
                            config.id = original_id;
                            store.update(cx, |store, cx| {
                                store.update_connection(config, cx);
                            });
                        },
                    )
                });
                workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
            })
            .log_err();
    }

    fn toggle_folder_collapsed(&mut self, folder: FolderId, cx: &mut Context<Self>) {
        if !self.collapsed_folders.remove(&folder) {
            self.collapsed_folders.insert(folder);
        }
        self.serialize_tree_state(cx);
        cx.notify();
    }

    // Keyboard tree navigation over the top-level folder/connection list (the
    // `SelectedEntity` variants that already exist). Deeper row kinds
    // (databases, tables, columns, indexes, ...) have no `SelectedEntity`
    // variant of their own yet and are click-only; giving them keyboard
    // navigation too would mean adding a variant per kind and threading
    // selection through every one of those render functions, which is a much
    // larger change than this pass — left as a follow-up.
    fn navigable_entities(&self, cx: &App) -> Vec<SelectedEntity> {
        let connections: Vec<ActiveConnection> =
            self.store.read(cx).connections().iter().cloned().collect();
        let folders: Vec<Folder> = self.store.read(cx).folders().to_vec();
        let nodes = build_folder_tree(&folders, &connections, None, 1);
        flatten_navigable_entities(&nodes, &connections, &self.collapsed_folders)
    }

    fn select_next(&mut self, _: &menu::SelectNext, _window: &mut Window, cx: &mut Context<Self>) {
        let entities = self.navigable_entities(cx);
        if entities.is_empty() {
            return;
        }
        let next = match self
            .selected_entity
            .and_then(|current| entities.iter().position(|e| *e == current))
        {
            Some(index) => entities.get(index + 1).copied().unwrap_or(entities[index]),
            None => entities[0],
        };
        self.selected_entity = Some(next);
        cx.notify();
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let entities = self.navigable_entities(cx);
        if entities.is_empty() {
            return;
        }
        let previous = match self
            .selected_entity
            .and_then(|current| entities.iter().position(|e| *e == current))
        {
            Some(0) => entities[0],
            Some(index) => entities[index - 1],
            None => *entities.last().expect("checked non-empty above"),
        };
        self.selected_entity = Some(previous);
        cx.notify();
    }

    fn select_first(
        &mut self,
        _: &menu::SelectFirst,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let entities = self.navigable_entities(cx);
        self.selected_entity = entities.into_iter().next();
        cx.notify();
    }

    fn select_last(&mut self, _: &menu::SelectLast, _window: &mut Window, cx: &mut Context<Self>) {
        let entities = self.navigable_entities(cx);
        self.selected_entity = entities.into_iter().next_back();
        cx.notify();
    }

    // Enter/Space activates the selected row the same way a click does:
    // opening a connection or toggling a folder's collapsed state.
    fn confirm_selected(
        &mut self,
        _: &menu::Confirm,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.selected_entity {
            Some(SelectedEntity::Folder(id)) => self.toggle_folder_collapsed(id, cx),
            Some(SelectedEntity::Connection(id)) => {
                self.store.update(cx, |store, cx| {
                    store.set_active_connection(id, cx);
                });
            }
            None => {}
        }
    }

    // Left arrow collapses the selected folder if expanded, matching
    // project_panel's CollapseSelectedEntry convention.
    fn collapse_selected(&mut self, cx: &mut Context<Self>) {
        if let Some(SelectedEntity::Folder(id)) = self.selected_entity
            && !self.collapsed_folders.contains(&id)
        {
            self.toggle_folder_collapsed(id, cx);
        }
    }

    // Right arrow expands the selected folder if collapsed, matching
    // project_panel's ExpandSelectedEntry convention.
    fn expand_selected(&mut self, cx: &mut Context<Self>) {
        if let Some(SelectedEntity::Folder(id)) = self.selected_entity
            && self.collapsed_folders.contains(&id)
        {
            self.toggle_folder_collapsed(id, cx);
        }
    }

    // Shift+Up/Down reorders the selected folder or connection among its
    // siblings, driving the same store methods as the "Move Up"/"Move Down"
    // context-menu entries and the connection selection-action-bar buttons.
    fn move_selected(&mut self, direction: i64, cx: &mut Context<Self>) {
        match self.selected_entity {
            Some(SelectedEntity::Folder(id)) => {
                self.store.update(cx, |store, cx| {
                    store.reorder_folder(id, direction, cx);
                });
            }
            Some(SelectedEntity::Connection(id)) => {
                self.store.update(cx, |store, cx| {
                    store.reorder_connection(id, direction, cx);
                });
            }
            None => {}
        }
    }

    fn toggle_connection_collapsed(&mut self, id: ConnectionId, cx: &mut Context<Self>) {
        let now_expanding = self.collapsed_connections.remove(&id);
        if !now_expanding {
            self.collapsed_connections.insert(id);
        } else {
            let needs_databases = self
                .store
                .read(cx)
                .connections()
                .iter()
                .find(|c| c.config.id == id)
                .is_some_and(|c| c.databases.is_none());
            if needs_databases {
                self.store
                    .update(cx, |store, cx| store.refresh_databases(id, cx))
                    .detach_and_log_err(cx);
            }
        }
        self.serialize_tree_state(cx);
        cx.notify();
    }

    /// Collapses the whole tree: every folder and connection is folded and all
    /// cached schema expansion is cleared. Cheap — touches only local state.
    fn collapse_all(&mut self, cx: &mut Context<Self>) {
        let folder_ids: HashSet<FolderId> =
            self.store.read(cx).folders().iter().map(|f| f.id).collect();
        let connection_ids: HashSet<ConnectionId> = self
            .store
            .read(cx)
            .connections()
            .iter()
            .map(|c| c.config.id)
            .collect();
        self.collapsed_folders = folder_ids;
        self.collapsed_connections = connection_ids;
        self.views_expanded.clear();
        self.table_indexes_expanded.clear();
        self.table_fks_expanded.clear();
        self.table_triggers_expanded.clear();
        self.server_objects_expanded.clear();
        self.store
            .update(cx, |store, cx| store.collapse_all_schema(cx));
        self.serialize_tree_state(cx);
        cx.notify();
    }

    /// Expands the structural tree: folders, connections, and the databases of
    /// connected connections (their tables load lazily). Tables and columns are
    /// not force-expanded to avoid a burst of metadata queries.
    fn expand_all(&mut self, cx: &mut Context<Self>) {
        self.collapsed_folders.clear();
        self.collapsed_connections.clear();
        self.store
            .update(cx, |store, cx| store.expand_all_databases(cx))
            .detach_and_log_err(cx);
        self.serialize_tree_state(cx);
        cx.notify();
    }

    /// Creates a folder under `parent` and immediately opens its inline editor so
    /// the name is typed in place (like creating a file in the project panel).
    /// Silently rejected when nesting would exceed the depth limit.
    fn start_new_folder(
        &mut self,
        parent: Option<FolderId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(parent) = parent {
            self.collapsed_folders.remove(&parent);
            self.serialize_tree_state(cx);
        }
        let new_id = self.store.update(cx, |store, cx| {
            store.add_folder("New Folder".into(), parent, cx)
        });
        if let Some(id) = new_id {
            self.begin_folder_rename(id, "New Folder", window, cx);
        }
    }

    fn begin_folder_rename(
        &mut self,
        id: FolderId,
        current_name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editor = cx.new(|cx| {
            let mut ed = Editor::single_line(window, cx);
            ed.set_text(current_name, window, cx);
            ed
        });
        let subscription = cx.subscribe(&editor, |this, _editor, event: &EditorEvent, cx| {
            if matches!(event, EditorEvent::Blurred) {
                this.commit_folder_rename(cx);
            }
        });
        let handle = editor.focus_handle(cx);
        window.focus(&handle, cx);
        self.editing_folder = Some(EditingFolder {
            id,
            editor,
            _subscription: subscription,
        });
        cx.notify();
    }

    fn commit_folder_rename(&mut self, cx: &mut Context<Self>) {
        let Some(editing) = self.editing_folder.take() else {
            return;
        };
        let name = editing.editor.read(cx).text(cx).trim().to_string();
        // An empty name keeps the existing folder name rather than deleting it,
        // so a stray Enter never destroys a folder.
        if !name.is_empty() {
            self.store
                .update(cx, |store, cx| store.rename_folder(editing.id, name, cx));
        }
        cx.notify();
    }

    fn cancel_folder_rename(&mut self, cx: &mut Context<Self>) {
        self.editing_folder = None;
        cx.notify();
    }

    fn delete_folder(&mut self, folder_id: FolderId, cx: &mut Context<Self>) {
        let removed = self
            .store
            .update(cx, |store, cx| store.remove_folder(folder_id, cx));
        if removed {
            return;
        }
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.show_toast(
                    Toast::new(
                        NotificationId::named("db-folder-not-empty".into()),
                        "Can't delete a folder that isn't empty. Move or delete its contents first.",
                    ),
                    cx,
                );
            })
            .ok();
    }

    fn new_connection_in_folder(
        &self,
        folder: Option<FolderId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = self.store.clone();
        self.workspace
            .update(cx, |workspace, cx| {
                let view = cx.new(|cx| {
                    ConnectionView::new(window, cx).with_on_confirm(move |mut config, cx| {
                        config.folder_id = folder;
                        store.update(cx, |store, cx| {
                            store.add_connection(config, cx);
                        });
                    })
                });
                workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
            })
            .log_err();
    }

    fn drag_preview(
        label: SharedString,
        icon: IconName,
        cx: &mut App,
    ) -> Entity<DraggedDbItemPreview> {
        cx.new(|_| DraggedDbItemPreview { label, icon })
    }

    /// Classifies a pointer's relative vertical position within a folder row
    /// (0.0 top .. 1.0 bottom) into a drop zone: the top and bottom quarters
    /// insert before/after the row, the middle half reparents into it (folders
    /// are the only rows that can contain children).
    fn folder_drop_zone(relative_y: f32, folder_id: FolderId) -> DropTarget {
        if relative_y < 0.25 {
            DropTarget::BeforeFolder(folder_id)
        } else if relative_y > 0.75 {
            DropTarget::AfterFolder(folder_id)
        } else {
            DropTarget::Folder(folder_id)
        }
    }

    /// Classifies a pointer's relative vertical position within a connection
    /// row (0.0 top .. 1.0 bottom) into a drop zone. Connections can't contain
    /// children, so the row splits evenly into before/after halves with no
    /// reparent-into zone.
    fn connection_drop_zone(relative_y: f32, connection_id: ConnectionId) -> DropTarget {
        if relative_y < 0.5 {
            DropTarget::BeforeConnection(connection_id)
        } else {
            DropTarget::AfterConnection(connection_id)
        }
    }

    /// Applies a drop of `item` onto `target`. `Folder`/`TopLevel` reparent
    /// (cycle and depth are guarded by the store, appending at the end);
    /// `Before*`/`After*` insert `item` at that exact sibling position,
    /// reparenting it too when the anchor lives under a different parent.
    fn handle_drop(&mut self, item: DraggedDbItem, target: DropTarget, cx: &mut Context<Self>) {
        self.drag_target = None;
        match target {
            DropTarget::Folder(id) => {
                self.store.update(cx, |store, cx| match item {
                    DraggedDbItem::Connection(cid) => {
                        store.move_connection_to_folder(cid, Some(id), cx)
                    }
                    DraggedDbItem::Folder(fid) => {
                        store.move_folder(fid, Some(id), cx);
                    }
                });
            }
            DropTarget::TopLevel => {
                self.store.update(cx, |store, cx| match item {
                    DraggedDbItem::Connection(cid) => {
                        store.move_connection_to_folder(cid, None, cx)
                    }
                    DraggedDbItem::Folder(fid) => {
                        store.move_folder(fid, None, cx);
                    }
                });
            }
            DropTarget::BeforeFolder(anchor) => {
                self.store.update(cx, |store, cx| {
                    store.reposition_item(
                        item.as_tree_item_ref(),
                        TreeItemRef::Folder(anchor),
                        RelativePosition::Before,
                        cx,
                    );
                });
            }
            DropTarget::AfterFolder(anchor) => {
                self.store.update(cx, |store, cx| {
                    store.reposition_item(
                        item.as_tree_item_ref(),
                        TreeItemRef::Folder(anchor),
                        RelativePosition::After,
                        cx,
                    );
                });
            }
            DropTarget::BeforeConnection(anchor) => {
                self.store.update(cx, |store, cx| {
                    store.reposition_item(
                        item.as_tree_item_ref(),
                        TreeItemRef::Connection(anchor),
                        RelativePosition::Before,
                        cx,
                    );
                });
            }
            DropTarget::AfterConnection(anchor) => {
                self.store.update(cx, |store, cx| {
                    store.reposition_item(
                        item.as_tree_item_ref(),
                        TreeItemRef::Connection(anchor),
                        RelativePosition::After,
                        cx,
                    );
                });
            }
        }
        cx.notify();
    }

    /// Whether `folder_id` can move up/down among its sibling folders (same
    /// `parent_id`, ordered by `order`) — mirrors the sibling lookup
    /// `DatabaseStore::reorder_folder` does internally, so a menu entry's
    /// visibility always matches whether the move would actually happen.
    fn folder_move_bounds(&self, folder_id: FolderId, cx: &Context<Self>) -> (bool, bool) {
        let store = self.store.read(cx);
        let Some(parent_id) = store
            .folders()
            .iter()
            .find(|folder| folder.id == folder_id)
            .map(|folder| folder.parent_id)
        else {
            return (false, false);
        };
        let mut siblings: Vec<(FolderId, i64)> = store
            .folders()
            .iter()
            .filter(|folder| folder.parent_id == parent_id)
            .map(|folder| (folder.id, folder.order))
            .collect();
        siblings.sort_by_key(|(_, order)| *order);
        let Some(position) = siblings
            .iter()
            .position(|(sibling_id, _)| *sibling_id == folder_id)
        else {
            return (false, false);
        };
        (position > 0, position + 1 < siblings.len())
    }

    fn render_folder_row(
        &self,
        folder: &Folder,
        depth: usize,
        is_collapsed: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let folder_id = folder.id;
        let name = folder.name.clone();
        let entity = cx.entity();
        let is_reparent_target = self.drag_target == Some(DropTarget::Folder(folder_id));
        let is_before_target = self.drag_target == Some(DropTarget::BeforeFolder(folder_id));
        let is_after_target = self.drag_target == Some(DropTarget::AfterFolder(folder_id));
        let is_selected = self.selected_entity == Some(SelectedEntity::Folder(folder_id));
        let (can_move_up, can_move_down) = self.folder_move_bounds(folder_id, cx);
        let editing = self
            .editing_folder
            .as_ref()
            .filter(|editing| editing.id == folder_id)
            .map(|editing| editing.editor.clone());

        let mut row = h_flex()
            .id(ElementId::from(SharedString::from(format!(
                "folder-row-{folder_id}"
            ))))
            .debug_selector(|| format!("folder-row-{folder_id}"))
            .min_w_full()
            .items_center()
            .gap_1()
            .py_1()
            .pr_2()
            .pl(tree_indent(depth))
            .rounded_sm()
            .relative()
            .when(is_selected, |el| {
                el.bg(cx.theme().colors().element_selected)
            })
            .hover(|style| style.bg(cx.theme().colors().element_hover))
            .when(is_reparent_target, |el| {
                el.bg(cx.theme().colors().drop_target_background)
            })
            .when(is_before_target, |el| {
                el.border_t_2()
                    .border_color(cx.theme().colors().text_accent)
            })
            .when(is_after_target, |el| {
                el.border_b_2()
                    .border_color(cx.theme().colors().text_accent)
            })
            .when(is_selected, |el| {
                // Absolutely positioned so the accent never perturbs row layout
                // (a real border would shift every child right by its width,
                // which previously broke a fixed-offset click test).
                el.child(
                    div()
                        .absolute()
                        .left_0()
                        .top_0()
                        .bottom_0()
                        .w(px(2.))
                        .bg(cx.theme().colors().text_accent),
                )
            })
            .child(
                h_flex()
                    .items_center()
                    .gap_0p5()
                    .child(
                        Icon::new(if is_collapsed {
                            IconName::ChevronRight
                        } else {
                            IconName::ChevronDown
                        })
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                    )
                    .child(
                        Icon::new(if is_collapsed {
                            IconName::Folder
                        } else {
                            IconName::FolderOpen
                        })
                        .size(IconSize::Small)
                        .color(Color::Default),
                    ),
            );

        if let Some(editor) = editing {
            row = row.child(
                div()
                    .flex_1()
                    .key_context("DbFolderRename")
                    .on_action(cx.listener(|this, _: &menu::Confirm, _, cx| {
                        this.commit_folder_rename(cx);
                    }))
                    .on_action(cx.listener(|this, _: &menu::Cancel, _, cx| {
                        this.cancel_folder_rename(cx);
                    }))
                    .child(editor),
            );
            return row.into_any_element();
        }

        let row = row
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                this.selected_entity = Some(SelectedEntity::Folder(folder_id));
                this.toggle_folder_collapsed(folder_id, cx);
            }))
            .on_drag(DraggedDbItem::Folder(folder_id), {
                let name = name.clone();
                move |_, _, _, cx| Self::drag_preview(name.clone().into(), IconName::Folder, cx)
            })
            .on_drag_move(
                cx.listener(move |this, event: &DragMoveEvent<DraggedDbItem>, _, cx| {
                    if !event.bounds.contains(&event.event.position) {
                        return;
                    }
                    let relative_y =
                        (event.event.position.y - event.bounds.origin.y) / event.bounds.size.height;
                    let new_target = Self::folder_drop_zone(relative_y, folder_id);
                    if this.drag_target != Some(new_target) {
                        this.drag_target = Some(new_target);
                        cx.notify();
                    }
                }),
            )
            .on_drop(cx.listener(move |this, item: &DraggedDbItem, _, cx| {
                let target = this.drag_target.unwrap_or(DropTarget::Folder(folder_id));
                this.handle_drop(*item, target, cx);
            }))
            .child(
                Label::new(name.clone())
                    .size(LabelSize::Small)
                    .single_line(),
            );

        right_click_menu(ElementId::from(SharedString::from(format!(
            "folder-menu-{folder_id}"
        ))))
        .trigger(move |_, _, _| row)
        .menu(move |window, cx| {
            let entity = entity.clone();
            let name = name.clone();
            ContextMenu::build(window, cx, move |menu, _, _| {
                menu.entry("New Subfolder", None, {
                    let entity = entity.clone();
                    move |window, cx| {
                        entity.update(cx, |panel, cx| {
                            panel.start_new_folder(Some(folder_id), window, cx);
                        });
                    }
                })
                .entry("New Connection", None, {
                    let entity = entity.clone();
                    move |window, cx| {
                        entity.update(cx, |panel, cx| {
                            panel.new_connection_in_folder(Some(folder_id), window, cx);
                        });
                    }
                })
                .separator()
                .when(can_move_up, |menu| {
                    let entity = entity.clone();
                    menu.entry("Move Up", None, move |_, cx| {
                        entity.update(cx, |panel, cx| {
                            panel.store.update(cx, |store, cx| {
                                store.reorder_folder(folder_id, -1, cx);
                            });
                        });
                    })
                })
                .when(can_move_down, |menu| {
                    let entity = entity.clone();
                    menu.entry("Move Down", None, move |_, cx| {
                        entity.update(cx, |panel, cx| {
                            panel.store.update(cx, |store, cx| {
                                store.reorder_folder(folder_id, 1, cx);
                            });
                        });
                    })
                })
                .separator()
                .entry("Rename", None, {
                    let entity = entity.clone();
                    move |window, cx| {
                        entity.update(cx, |panel, cx| {
                            panel.begin_folder_rename(folder_id, &name, window, cx);
                        });
                    }
                })
                .entry("Delete Folder", None, move |_, cx| {
                    entity.update(cx, |panel, cx| {
                        panel.delete_folder(folder_id, cx);
                    });
                })
            })
        })
        .into_any_element()
    }

    fn open_sql_query_with_text(
        workspace: WeakEntity<Workspace>,
        store: WeakEntity<DatabaseStore>,
        connection_id: ConnectionId,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let languages = workspace
            .update(cx, |ws, _cx| ws.app_state().languages.clone())
            .log_err();
        let Some(languages) = languages else { return };
        let language_task = languages.language_for_name("SQL");
        cx.spawn_in(window, async move |_, cx| {
            let language = language_task.await.log_err();
            workspace
                .update_in(cx, |workspace, window, cx| {
                    let project = workspace.project().clone();
                    let buffer_task = project.update(cx, move |project, cx| {
                        project.create_buffer(language, false, cx)
                    });
                    let workspace_weak = workspace.weak_handle();
                    cx.spawn_in(window, async move |workspace, cx| {
                        let buffer = buffer_task.await?;
                        let multi = cx.new(|cx| {
                            MultiBuffer::singleton(buffer, cx).with_title("query.sql".into())
                        });
                        workspace.update_in(cx, |workspace, window, cx| {
                            let editor = cx.new(|cx| {
                                let mut ed = Editor::for_multibuffer(multi, None, window, cx);
                                ed.set_text(text.clone(), window, cx);
                                ed.register_addon(DbQueryEditorAddon::new(connection_id));
                                ed.set_show_runnables(true, cx);
                                ed.set_semantics_provider(Some(Rc::new(DbSemanticsProvider {
                                    connection_id,
                                    store: store.clone(),
                                    workspace: workspace_weak.clone(),
                                })));
                                ed
                            });
                            workspace.add_item_to_active_pane(
                                Box::new(editor),
                                None,
                                true,
                                window,
                                cx,
                            );
                        })?;
                        anyhow::Ok(())
                    })
                    .detach_and_log_err(cx);
                })
                .log_err();
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    /// Stacked tree overlays for a column: gold star = primary key,
    /// blue link = foreign key, hash = unique/secondary index,
    /// dot = NOT NULL. Order is stable so icons line up across rows.
    fn column_overlay_icons(col: &ColumnInfo, is_fk: bool) -> Vec<(IconName, Color)> {
        let mut icons = Vec::new();
        if col.column_key.as_deref() == Some("PRI") {
            icons.push((IconName::StarFilled, Color::Warning));
        }
        if is_fk {
            icons.push((IconName::Link, Color::Info));
        }
        match col.column_key.as_deref() {
            Some("UNI") => icons.push((IconName::Hash, Color::Accent)),
            Some("MUL") if !is_fk => icons.push((IconName::Hash, Color::Muted)),
            _ => {}
        }
        if !col.is_nullable {
            icons.push((IconName::SquareDot, Color::Muted));
        }
        icons
    }

    fn go_to_ddl_for_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(node) = self.selected_tree_node.clone() {
            match node.table {
                Some(table) => {
                    self.open_table_ddl(node.connection_id, node.database, table, window, cx)
                }
                None => self.open_database_ddl(node.connection_id, node.database, window, cx),
            }
        }
    }

    fn quick_doc_for_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(node) = self.selected_tree_node.clone()
            && let Some(table) = node.table
        {
            self.open_quick_doc(node.connection_id, node.database, table, window, cx);
        }
    }

    fn show_diagram_for_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(node) = self.selected_tree_node.clone() {
            self.open_erd_diagram(node.connection_id, node.database, window, cx);
        }
    }

    fn open_table_ddl(
        &mut self,
        id: ConnectionId,
        database: String,
        table: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ddl_task = self
            .store
            .update(cx, |store, cx| store.get_table_ddl(id, database, table, cx));
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |this, cx| {
            let ddl = ddl_task.await?;
            this.update_in(cx, |panel, window, cx| {
                Self::open_sql_query_with_text(
                    workspace.clone(),
                    panel.store.downgrade(),
                    id,
                    ddl,
                    window,
                    cx,
                );
            })
            .log_err();
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn open_database_ddl(
        &mut self,
        id: ConnectionId,
        database: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ddl_task = self
            .store
            .update(cx, |store, cx| store.get_database_ddl(id, database, cx));
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |this, cx| {
            let ddl = ddl_task.await?;
            this.update_in(cx, |panel, window, cx| {
                Self::open_sql_query_with_text(
                    workspace.clone(),
                    panel.store.downgrade(),
                    id,
                    ddl,
                    window,
                    cx,
                );
            })
            .log_err();
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn open_quick_doc(
        &mut self,
        id: ConnectionId,
        database: String,
        table: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title = SharedString::from(format!("{database}.{table}"));
        let task = self.store.update(cx, |store, cx| {
            store.describe_table(id, database, table, cx)
        });
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |_this, cx| {
            let columns = task.await.unwrap_or_default();
            workspace
                .update_in(cx, |workspace, window, cx| {
                    workspace.toggle_modal(window, cx, |window, cx| {
                        QuickDocView::new(title.clone(), columns.clone(), window, cx)
                    });
                })
                .log_err();
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn open_modify_table(
        &mut self,
        id: ConnectionId,
        database: String,
        table: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let driver = self
            .store
            .read(cx)
            .connections()
            .iter()
            .find(|connection| connection.config.id == id)
            .map(|connection| connection.config.driver);
        let Some(driver) = driver else { return };
        let describe = self.store.update(cx, |store, cx| {
            store.describe_table(id, database.clone(), table.clone(), cx)
        });
        let indexes = self.store.update(cx, |store, cx| {
            store.list_indexes(id, database.clone(), table.clone(), cx)
        });
        let foreign_keys = self.store.update(cx, |store, cx| {
            store.list_foreign_keys(id, database.clone(), table.clone(), cx)
        });
        let checks = self.store.update(cx, |store, cx| {
            store.list_check_constraints(id, database.clone(), table.clone(), cx)
        });
        let store = self.store.clone();
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |_this, cx| {
            let columns = describe.await.unwrap_or_default();
            let indexes = indexes.await.unwrap_or_default();
            let foreign_keys = foreign_keys.await.unwrap_or_default();
            let checks = checks.await.unwrap_or_default();
            workspace
                .update_in(cx, |workspace, window, cx| {
                    workspace.toggle_modal(window, cx, |window, cx| {
                        ModifyTableView::new(
                            store.clone(),
                            id,
                            driver,
                            database.clone(),
                            table.clone(),
                            &columns,
                            &indexes,
                            &foreign_keys,
                            &checks,
                            window,
                            cx,
                        )
                    });
                })
                .log_err();
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn open_dump_dialog(
        &mut self,
        id: ConnectionId,
        preset_databases: Vec<String>,
        preset_tables: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let config = self
            .store
            .read(cx)
            .connections()
            .iter()
            .find(|connection| connection.config.id == id)
            .map(|connection| connection.config.clone());
        let Some(config) = config else { return };
        let driver = config.driver;
        let panel = cx.entity().downgrade();
        let on_run: DumpRunCallback = Arc::new(move |request, _window, cx: &mut App| {
            panel
                .update(cx, |panel, cx| panel.start_dump(id, request, cx))
                .ok();
        });
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.toggle_modal(window, cx, |window, cx| {
                    NativeDumpDialog::new(
                        driver,
                        config,
                        preset_databases,
                        preset_tables,
                        window,
                        cx,
                    )
                    .on_run(on_run)
                });
            })
            .log_err();
    }

    fn start_dump(&mut self, id: ConnectionId, request: DumpRequest, cx: &mut Context<Self>) {
        let password = self
            .store
            .read(cx)
            .connections()
            .iter()
            .find(|connection| connection.config.id == id)
            .map(|connection| connection.config.password.clone())
            .filter(|password| !password.is_empty());
        let timestamp = dump_timestamp();
        let database = request
            .databases
            .first()
            .or(request.database.as_ref())
            .cloned()
            .unwrap_or_default();
        let resolved_output = apply_substitutions(
            &request.output_path,
            &request.data_source,
            &database,
            &timestamp,
        );
        let task_id = self.dump.next_id;
        self.dump.next_id += 1;
        let label: SharedString = request.data_source.clone().into();
        self.dump.tasks.push(DumpTask {
            id: task_id,
            label,
            status: DumpStatus::Running,
        });
        let dump = spawn_dump(request, password, resolved_output, cx);
        let handle = cx.spawn(async move |panel, cx| {
            let result = dump.await;
            panel
                .update(cx, |panel, cx| {
                    if let Some(task) = panel.dump.tasks.iter_mut().find(|task| task.id == task_id)
                    {
                        task.status = match result {
                            Ok(output_path) => DumpStatus::Done { output_path },
                            Err(message) => DumpStatus::Failed { message },
                        };
                    }
                    panel
                        .dump
                        .runners
                        .retain(|(running_id, _)| *running_id != task_id);
                    cx.notify();
                })
                .ok();
        });
        self.dump.runners.push((task_id, handle));
        cx.notify();
    }

    fn open_exec_dialog(
        &mut self,
        id: ConnectionId,
        database: String,
        connection_label: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = self.store.clone();
        let label_for_run = connection_label.clone();
        let on_run: crate::sql_exec::ExecRunCallback =
            Arc::new(move |sql_text, _window, cx: &mut App| {
                store.update(cx, |store, cx| {
                    store.start_exec_job(id, database.clone(), label_for_run.clone(), sql_text, cx);
                });
            });
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.toggle_modal(window, cx, |window, cx| {
                    crate::sql_exec::ExecDialog::new(connection_label, window, cx).on_run(on_run)
                });
            })
            .log_err();
    }

    fn dismiss_dump_task(&mut self, task_id: usize, cx: &mut Context<Self>) {
        // Dropping the runner handle cancels a dump that is still running.
        self.dump
            .runners
            .retain(|(running_id, _)| *running_id != task_id);
        self.dump.tasks.retain(|task| task.id != task_id);
        cx.notify();
    }

    fn render_dump_status(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self
            .dump
            .tasks
            .iter()
            .map(|task| {
                let task_id = task.id;
                let is_running = matches!(task.status, DumpStatus::Running);
                h_flex()
                    .w_full()
                    .items_center()
                    .gap_1()
                    .child(div().flex_1().child(render_dump_status_row(task, cx)))
                    .child(
                        IconButton::new(
                            SharedString::from(format!("dump-task-dismiss-{task_id}")),
                            if is_running {
                                IconName::XCircle
                            } else {
                                IconName::Close
                            },
                        )
                        .tooltip(Tooltip::text(if is_running { "Cancel" } else { "Dismiss" }))
                        .on_click(cx.listener(move |panel, _, _, cx| {
                            panel.dismiss_dump_task(task_id, cx);
                        })),
                    )
            })
            .collect::<Vec<_>>();
        v_flex()
            .flex_none()
            .w_full()
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .children(rows)
    }

    fn start_export_to_file(
        &mut self,
        label: SharedString,
        provider: std::sync::Arc<dyn db_client::provider::DbProvider>,
        database: String,
        sql: String,
        output_path: std::path::PathBuf,
        format: crate::execute_to_file::ExecuteToFileFormat,
        cx: &mut Context<Self>,
    ) {
        let task_id = self.export.next_id;
        self.export.next_id += 1;
        self.export.tasks.push(DumpTask {
            id: task_id,
            label,
            status: DumpStatus::Running,
        });
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let export = crate::execute_to_file::spawn_execute_to_file(
            provider,
            database,
            sql,
            output_path.clone(),
            format,
            cancelled.clone(),
            cx,
        );
        let handle = cx.spawn(async move |panel, cx| {
            let result = export.await;
            panel
                .update(cx, |panel, cx| {
                    if let Some(task) = panel
                        .export
                        .tasks
                        .iter_mut()
                        .find(|task| task.id == task_id)
                    {
                        task.status = match result {
                            Ok(rows) => DumpStatus::Done {
                                output_path: format!("{} ({rows} rows)", output_path.display()),
                            },
                            Err(message) => DumpStatus::Failed { message },
                        };
                    }
                    panel
                        .export
                        .runners
                        .retain(|(running_id, ..)| *running_id != task_id);
                    cx.notify();
                })
                .ok();
        });
        self.export.runners.push((task_id, handle, cancelled));
        cx.notify();
    }

    fn dismiss_export_task(&mut self, task_id: usize, cx: &mut Context<Self>) {
        // Ask a still-running export to stop at its next row/columns write
        // instead of dropping the task handle outright, so `FileRowSink`
        // reaches its own cleanup path (deleting the partial file) rather
        // than being cut off mid-write.
        if let Some((.., cancelled)) = self
            .export
            .runners
            .iter()
            .find(|(running_id, ..)| *running_id == task_id)
        {
            cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.export.tasks.retain(|task| task.id != task_id);
        cx.notify();
    }

    /// Opens a picker over every OTHER connection as the copy target. Reuses
    /// `ComparePickerView`'s generic label-list-plus-callback picker (built
    /// for choosing a second table to diff) since choosing a second
    /// connection to copy into is the same shape of UI.
    fn open_copy_table_picker(
        &mut self,
        id: ConnectionId,
        database: String,
        table: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let candidates: Vec<String> = self
            .store
            .read(cx)
            .connections()
            .iter()
            .filter(|connection| connection.config.id != id)
            .map(|connection| connection.config.label.clone())
            .collect();
        let weak = cx.entity().downgrade();
        let title = table.clone();
        let on_pick: ComparePickCallback = Arc::new(move |target_label, window, cx| {
            weak.update(cx, |panel, cx| {
                panel.start_table_copy(
                    id,
                    database.clone(),
                    table.clone(),
                    target_label,
                    window,
                    cx,
                );
            })
            .ok();
        });
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.toggle_modal(window, cx, |_window, cx| {
                    ComparePickerView::new(title, candidates, on_pick, cx)
                });
            })
            .log_err();
    }

    /// Copies every row of `source_table` into a same-named table on the
    /// connection labeled `target_label`, creating it first (type-mapped from
    /// the source driver) unless a compatible table already exists there.
    fn start_table_copy(
        &mut self,
        source_id: ConnectionId,
        source_database: String,
        source_table: String,
        target_label: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store_ref = self.store.read(cx);
        let Some(source) = store_ref
            .connections()
            .iter()
            .find(|connection| connection.config.id == source_id)
        else {
            return;
        };
        let Some(source_provider) = source.provider.clone() else {
            return;
        };
        let source_driver = source.config.driver;
        let Some(target) = store_ref
            .connections()
            .iter()
            .find(|connection| connection.config.label == target_label)
        else {
            return;
        };
        let Some(target_provider) = target.provider.clone() else {
            return;
        };
        let target_driver = target.config.driver;
        let target_database = target.config.database.clone().unwrap_or_default();

        let task_id = self.export.next_id;
        self.export.next_id += 1;
        self.export.tasks.push(DumpTask {
            id: task_id,
            label: format!("Copy Table: {source_table} → {target_label}").into(),
            status: DumpStatus::Running,
        });
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancelled_for_task = cancelled.clone();
        let source_table_for_task = source_table;
        let handle = cx.spawn_in(window, async move |panel, cx| {
            let cancelled = cancelled_for_task;
            let source_columns = source_provider
                .describe_table(&source_database, &source_table_for_task)
                .await
                .unwrap_or_default();
            let existing_target_columns = target_provider
                .describe_table(&target_database, &source_table_for_task)
                .await
                .ok()
                .filter(|columns| !columns.is_empty());
            let copy = cx.update(|_window, cx| {
                crate::table_copy::spawn_table_copy(
                    source_provider,
                    source_database,
                    source_table_for_task.clone(),
                    source_driver,
                    source_columns,
                    target_provider,
                    target_database,
                    source_table_for_task,
                    target_driver,
                    existing_target_columns,
                    cancelled.clone(),
                    cx,
                )
            });
            let Ok(copy) = copy else { return };
            let result = copy.await;
            panel
                .update(cx, |panel, cx| {
                    if let Some(task) = panel
                        .export
                        .tasks
                        .iter_mut()
                        .find(|task| task.id == task_id)
                    {
                        task.status = match result {
                            Ok(rows) => DumpStatus::Done {
                                output_path: format!("{rows} row(s) copied"),
                            },
                            Err(message) => DumpStatus::Failed { message },
                        };
                    }
                    panel
                        .export
                        .runners
                        .retain(|(running_id, ..)| *running_id != task_id);
                    cx.notify();
                })
                .ok();
        });
        self.export.runners.push((task_id, handle, cancelled));
        cx.notify();
    }

    fn render_export_status(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self
            .export
            .tasks
            .iter()
            .map(|task| {
                let task_id = task.id;
                let is_running = matches!(task.status, DumpStatus::Running);
                h_flex()
                    .w_full()
                    .items_center()
                    .gap_1()
                    .child(div().flex_1().child(render_dump_status_row(task, cx)))
                    .child(
                        IconButton::new(
                            SharedString::from(format!("export-task-dismiss-{task_id}")),
                            if is_running {
                                IconName::XCircle
                            } else {
                                IconName::Close
                            },
                        )
                        .tooltip(Tooltip::text(if is_running { "Cancel" } else { "Dismiss" }))
                        .on_click(cx.listener(move |panel, _, _, cx| {
                            panel.dismiss_export_task(task_id, cx);
                        })),
                    )
            })
            .collect::<Vec<_>>();
        v_flex()
            .flex_none()
            .w_full()
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .children(rows)
    }

    fn open_ddl_source(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace
            .update(cx, |workspace, cx| {
                let view = cx.new(|cx| DdlSourceView::new(window, cx));
                workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
            })
            .log_err();
    }

    /// Opens the fuzzy go-to-object palette (Ctrl+N) for the connection the
    /// tree selection currently targets, falling back to the active
    /// connection when nothing is selected in the tree.
    fn open_go_to_object_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let connection_id = self
            .selected_tree_node
            .as_ref()
            .map(|node| node.connection_id)
            .or_else(|| self.store.read(cx).active_connection_id());
        let Some(connection_id) = connection_id else {
            return;
        };
        let Some(connection) = self
            .store
            .read(cx)
            .connections()
            .iter()
            .find(|c| c.config.id == connection_id)
        else {
            return;
        };
        let label = SharedString::from(connection.config.label.clone());
        let driver = connection.config.driver;
        let store = self.store.clone();
        self.workspace
            .update(cx, |workspace, cx| {
                let workspace_weak = cx.entity().downgrade();
                workspace.toggle_modal(window, cx, |window, cx| {
                    GoToObjectPalette::new(
                        store,
                        workspace_weak,
                        connection_id,
                        label,
                        driver,
                        window,
                        cx,
                    )
                });
            })
            .log_err();
    }

    /// Exports the whole Database Explorer (folder tree, all connection
    /// settings, the SQL console files, and optionally master-password-encrypted
    /// passwords) to one portable file the user picks.
    fn export_database_explorer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        let folders = store.read(cx).folders().to_vec();
        let connections: Vec<ConnectionConfig> = store
            .read(cx)
            .connections()
            .iter()
            .map(|connection| {
                let mut config = connection.config.clone();
                config.password = String::new();
                config
            })
            .collect();
        let console_specs: Vec<(ConnectionId, std::path::PathBuf)> = connections
            .iter()
            .map(|config| {
                (
                    config.id,
                    connection_query_path(config.id, &config.label, config.driver),
                )
            })
            .collect();
        let secrets_task = store.update(cx, |store, cx| store.read_all_secrets(cx));
        let workspace = self.workspace.clone();
        let path_rx =
            cx.prompt_for_new_path(paths::home_dir(), Some("database-explorer.zdbexport.json"));

        cx.spawn_in(window, async move |_this, cx| {
            let Some(path) = path_rx
                .await
                .log_err()
                .and_then(|result| result.log_err())
                .flatten()
            else {
                return;
            };
            let consoles = cx
                .background_executor()
                .spawn(async move {
                    console_specs
                        .into_iter()
                        .filter_map(|(connection_id, file_path)| {
                            let content = std::fs::read_to_string(&file_path).ok()?;
                            let filename = file_path.file_name()?.to_string_lossy().into_owned();
                            Some(ConsoleFile {
                                connection_id,
                                filename,
                                content,
                            })
                        })
                        .collect::<Vec<_>>()
                })
                .await;
            let secrets_map = secrets_task.await.unwrap_or_default();

            if secrets_map.is_empty() {
                workspace
                    .update_in(cx, |_workspace, _window, cx| {
                        write_export_bundle(path, folders, connections, consoles, None, cx);
                    })
                    .log_err();
                return;
            }

            workspace
                .update_in(cx, |workspace, window, cx| {
                    let on_result: MasterPasswordCallback = Arc::new(move |password, _window, cx| {
                        let secrets = password
                            .and_then(|master| encrypt_secrets(&secrets_map, &master).log_err());
                        write_export_bundle(
                            path.clone(),
                            folders.clone(),
                            connections.clone(),
                            consoles.clone(),
                            secrets,
                            cx,
                        );
                    });
                    workspace.toggle_modal(window, cx, |window, cx| {
                        MasterPasswordView::new(
                            "Export with a master password",
                            "Set a master password to encrypt the saved connection passwords; \
                             you will enter it when importing. Or skip to export without passwords.",
                            "Encrypt & export",
                            true,
                            on_result,
                            window,
                            cx,
                        )
                    });
                })
                .log_err();
        })
        .detach();
    }

    /// Imports a bundle written by `export_database_explorer`. Restores the tree,
    /// connections and console files immediately; if the bundle carries encrypted
    /// passwords, asks once for the master password to decrypt them into the
    /// keychain. A wrong/skipped master password still leaves everything else
    /// restored — connecting later prompts for the password.
    fn import_database_explorer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        let workspace = self.workspace.clone();
        let path_rx = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: None,
        });

        cx.spawn_in(window, async move |_this, cx| {
            let Some(path) = path_rx
                .await
                .log_err()
                .and_then(|result| result.log_err())
                .flatten()
                .and_then(|paths| paths.into_iter().next())
            else {
                return;
            };
            let read_path = path.clone();
            let bytes = cx
                .background_executor()
                .spawn(async move { std::fs::read(&read_path) })
                .await;
            let Some(bytes) = bytes.log_err() else {
                show_db_toast(
                    &workspace,
                    "db-import-read",
                    "Could not read the export file.",
                    cx,
                );
                return;
            };
            let Some(bundle) = serde_json::from_slice::<ExportBundle>(&bytes).log_err() else {
                show_db_toast(
                    &workspace,
                    "db-import-parse",
                    "That file is not a valid Database Explorer export.",
                    cx,
                );
                return;
            };

            let folders = bundle.folders.clone();
            let connections = bundle.connections.clone();
            store.update(cx, |store, cx| store.restore_tree(folders, connections, cx));

            let labels: HashMap<ConnectionId, String> = bundle
                .connections
                .iter()
                .map(|config| (config.id, config.label.clone()))
                .collect();
            let drivers: HashMap<ConnectionId, DatabaseDriver> = bundle
                .connections
                .iter()
                .map(|config| (config.id, config.driver))
                .collect();
            let console_writes: Vec<(std::path::PathBuf, String)> = bundle
                .consoles
                .iter()
                .filter_map(|console| {
                    let label = labels.get(&console.connection_id)?;
                    let driver = drivers
                        .get(&console.connection_id)
                        .copied()
                        .unwrap_or(DatabaseDriver::MySQL);
                    Some((
                        connection_query_path(console.connection_id, label, driver),
                        console.content.clone(),
                    ))
                })
                .collect();
            cx.background_executor()
                .spawn(async move {
                    for (file_path, content) in console_writes {
                        if let Some(parent) = file_path.parent() {
                            std::fs::create_dir_all(parent).ok();
                        }
                        std::fs::write(&file_path, content).log_err();
                    }
                })
                .await;

            let Some(secrets) = bundle.secrets.clone() else {
                show_db_toast(
                    &workspace,
                    "db-import-done",
                    "Database Explorer restored.",
                    cx,
                );
                return;
            };

            let store_for_modal = store.clone();
            let workspace_for_modal = workspace.clone();
            workspace
                .update_in(cx, |workspace, window, cx| {
                    let on_result: MasterPasswordCallback =
                        Arc::new(move |password, _window, cx| {
                            let Some(master) = password else {
                                return;
                            };
                            match decrypt_secrets(&secrets, &master) {
                                Ok(secrets) => {
                                    store_for_modal
                                        .update(cx, |store, cx| store.restore_secrets(secrets, cx))
                                        .detach();
                                }
                                Err(_) => show_db_toast(
                                    &workspace_for_modal,
                                    "db-import-decrypt",
                                    "Could not decrypt passwords; everything else was restored. \
                                 Connecting will ask for the password.",
                                    cx,
                                ),
                            }
                        });
                    workspace.toggle_modal(window, cx, |window, cx| {
                        MasterPasswordView::new(
                            "Restore saved passwords",
                            "Enter the master password used when exporting to decrypt the saved \
                             connection passwords. Cancel to restore without them.",
                            "Decrypt",
                            false,
                            on_result,
                            window,
                            cx,
                        )
                    });
                })
                .log_err();
        })
        .detach();
    }

    fn open_data_import(
        &mut self,
        id: ConnectionId,
        database: String,
        table: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let driver = self
            .store
            .read(cx)
            .connections()
            .iter()
            .find(|connection| connection.config.id == id)
            .map(|connection| connection.config.driver);
        let Some(driver) = driver else { return };
        let describe = self.store.update(cx, |store, cx| {
            store.describe_table(id, database.clone(), table.clone(), cx)
        });
        let store = self.store.clone();
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |_this, cx| {
            let columns = describe.await.unwrap_or_default();
            let target_columns: Vec<String> =
                columns.into_iter().map(|column| column.name).collect();
            workspace
                .update_in(cx, |workspace, window, cx| {
                    workspace.toggle_modal(window, cx, |window, cx| {
                        ImportDataView::new(
                            store.clone(),
                            id,
                            database.clone(),
                            table.clone(),
                            driver,
                            target_columns.clone(),
                            window,
                            cx,
                        )
                    });
                })
                .log_err();
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn open_explain_plan(
        &mut self,
        id: ConnectionId,
        database: String,
        driver: DatabaseDriver,
        sql: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let explain_sql = explain_sql_for_driver(driver, &sql);
        let store = self.store.clone();
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |_this, cx| {
            let connect = store.update(cx, |store, cx| store.connect(id, cx));
            connect.await.log_err();
            let task = store.update(cx, |store, cx| {
                store.execute_query(id, database.clone(), explain_sql, cx)
            });
            let result = task.await;
            let roots = match result {
                Ok(result) => parse_plan_tree(&plan_text_from_result(&result)),
                Err(error) => vec![PlanNode {
                    text: format!("EXPLAIN failed: {error}"),
                    children: Vec::new(),
                }],
            };
            workspace
                .update_in(cx, |workspace, window, cx| {
                    let context = crate::explain_plan::ExplainQueryContext {
                        store,
                        connection_id: id,
                        database,
                        driver,
                        sql,
                    };
                    let view = cx.new(|cx| ExplainPlanView::new(roots, Some(context), window, cx));
                    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
                })
                .log_err();
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn open_erd_diagram(
        &mut self,
        id: ConnectionId,
        database: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let provider = self
            .store
            .read(cx)
            .connections()
            .iter()
            .find(|connection| connection.config.id == id)
            .and_then(|connection| connection.provider.clone());
        let Some(provider) = provider else { return };
        let workspace = self.workspace.clone();
        let title: SharedString = format!("Diagram: {database}").into();
        cx.spawn_in(window, async move |_this, cx| {
            let tables = provider
                .list_tables(&database)
                .await
                .log_err()
                .unwrap_or_default();
            // Describing every table is one round-trip each; cap the diagram so a
            // large schema cannot freeze the introspection or the layout.
            let mut erd_tables = Vec::new();
            let mut relationships = Vec::new();
            for table in tables.into_iter().take(ERD_TABLE_LIMIT) {
                let columns = provider
                    .describe_table(&database, &table.name)
                    .await
                    .log_err()
                    .unwrap_or_default();
                let foreign_keys = provider
                    .list_foreign_keys(&database, &table.name)
                    .await
                    .log_err()
                    .unwrap_or_default();
                let fk_columns: HashSet<String> = foreign_keys
                    .iter()
                    .map(|fk| fk.from_column.clone())
                    .collect();
                for fk in &foreign_keys {
                    relationships.push(ErdRelationship {
                        from_table: table.name.clone(),
                        from_column: fk.from_column.clone(),
                        to_table: fk.to_table.clone(),
                        to_column: fk.to_column.clone(),
                    });
                }
                erd_tables.push(ErdTable {
                    name: table.name.clone(),
                    columns: columns
                        .into_iter()
                        .map(|column| ErdColumn {
                            is_primary_key: column.column_key.as_deref() == Some("PRI"),
                            is_foreign_key: fk_columns.contains(&column.name),
                            name: column.name,
                            data_type: column.data_type,
                        })
                        .collect(),
                });
            }
            workspace
                .update_in(cx, |workspace, window, cx| {
                    let view =
                        cx.new(|cx| ErdView::new(erd_tables, relationships, title, window, cx));
                    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
                })
                .log_err();
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    /// Opens the Get/Put/Scan form for an Aerospike connection, in place of
    /// the SQL console other drivers get — Aerospike has no query language.
    fn open_new_aerospike_view(
        &mut self,
        id: ConnectionId,
        label: String,
        default_namespace: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = self.store.clone();
        let workspace = self.workspace.clone();
        let Some(workspace) = workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            let view = cx.new(|cx| {
                AerospikeView::new(
                    store,
                    workspace.weak_handle(),
                    id,
                    label.into(),
                    default_namespace,
                    window,
                    cx,
                )
            });
            workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
        });
    }

    fn open_full_text_search(
        &mut self,
        id: ConnectionId,
        database: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (provider, driver, label) = {
            let store = self.store.read(cx);
            let Some(connection) = store.connections().iter().find(|c| c.config.id == id) else {
                return;
            };
            let Some(provider) = connection.provider.clone() else {
                return;
            };
            (
                provider,
                connection.config.driver,
                SharedString::from(connection.config.label.clone()),
            )
        };
        let store = self.store.clone();
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |_this, cx| {
            let tables = provider
                .list_tables(&database)
                .await
                .log_err()
                .unwrap_or_default()
                .into_iter()
                .map(|table| table.name)
                .collect::<Vec<_>>();
            workspace
                .update_in(cx, |workspace, window, cx| {
                    let view = cx.new(|cx| {
                        FullTextSearchView::new(
                            store,
                            workspace.weak_handle(),
                            id,
                            label,
                            database,
                            driver,
                            tables,
                            window,
                            cx,
                        )
                    });
                    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
                })
                .log_err();
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn open_compare_picker(
        &mut self,
        id: ConnectionId,
        database: String,
        left_table: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let provider = self
            .store
            .read(cx)
            .connections()
            .iter()
            .find(|connection| connection.config.id == id)
            .and_then(|connection| connection.provider.clone());
        let Some(provider) = provider else { return };
        let left_for_filter = left_table.clone();
        cx.spawn_in(window, async move |this, cx| {
            let candidates: Vec<String> = provider
                .list_tables(&database)
                .await
                .log_err()
                .unwrap_or_default()
                .into_iter()
                .map(|table| table.name)
                .filter(|name| name != &left_for_filter)
                .collect();
            this.update_in(cx, |panel, window, cx| {
                let weak = cx.entity().downgrade();
                let title = left_table.clone();
                let database = database.clone();
                let on_pick: ComparePickCallback = Arc::new(move |right_table, window, cx| {
                    weak.update(cx, |panel, cx| {
                        panel.start_compare(
                            id,
                            database.clone(),
                            left_table.clone(),
                            right_table,
                            window,
                            cx,
                        );
                    })
                    .ok();
                });
                panel
                    .workspace
                    .update(cx, |workspace, cx| {
                        workspace.toggle_modal(window, cx, |_window, cx| {
                            ComparePickerView::new(title.clone(), candidates.clone(), on_pick, cx)
                        });
                    })
                    .log_err();
            })
            .log_err();
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn start_compare(
        &mut self,
        id: ConnectionId,
        database: String,
        left_table: String,
        right_table: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let provider = self
            .store
            .read(cx)
            .connections()
            .iter()
            .find(|connection| connection.config.id == id)
            .and_then(|connection| connection.provider.clone());
        let Some(provider) = provider else { return };
        let workspace = self.workspace.clone();
        let title: SharedString = format!("Compare: {left_table} vs {right_table}").into();
        cx.spawn_in(window, async move |_this, cx| {
            let left = provider
                .execute_query(&database, &format!("SELECT * FROM {left_table}"))
                .await;
            let right = provider
                .execute_query(&database, &format!("SELECT * FROM {right_table}"))
                .await;
            // Match rows by the left table's primary key when it has one, so a
            // value change reads as Changed rather than Added+Removed.
            let key_columns = provider
                .describe_table(&database, &left_table)
                .await
                .log_err()
                .map(|columns| {
                    columns
                        .iter()
                        .enumerate()
                        .filter(|(_, column)| column.column_key.as_deref() == Some("PRI"))
                        .map(|(index, _)| index)
                        .collect::<Vec<_>>()
                })
                .filter(|keys| !keys.is_empty());
            let (Ok(left), Ok(right)) = (left, right) else {
                return anyhow::Ok(());
            };
            workspace
                .update_in(cx, |workspace, window, cx| {
                    let view = cx.new(|cx| {
                        CompareDataView::new(left, right, key_columns, title, window, cx)
                    });
                    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
                })
                .log_err();
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    /// Same picker flow as `open_compare_picker`, but for structural
    /// comparison: the chosen second table's schema is diffed against
    /// `left_table`'s instead of their row data.
    fn open_schema_compare_picker(
        &mut self,
        id: ConnectionId,
        database: String,
        left_table: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let provider = self
            .store
            .read(cx)
            .connections()
            .iter()
            .find(|connection| connection.config.id == id)
            .and_then(|connection| connection.provider.clone());
        let Some(provider) = provider else { return };
        let left_for_filter = left_table.clone();
        cx.spawn_in(window, async move |this, cx| {
            let candidates: Vec<String> = provider
                .list_tables(&database)
                .await
                .log_err()
                .unwrap_or_default()
                .into_iter()
                .map(|table| table.name)
                .filter(|name| name != &left_for_filter)
                .collect();
            this.update_in(cx, |panel, window, cx| {
                let weak = cx.entity().downgrade();
                let title = left_table.clone();
                let database = database.clone();
                let on_pick: ComparePickCallback = Arc::new(move |right_table, window, cx| {
                    weak.update(cx, |panel, cx| {
                        panel.start_schema_compare(
                            id,
                            database.clone(),
                            left_table.clone(),
                            right_table,
                            window,
                            cx,
                        );
                    })
                    .ok();
                });
                panel
                    .workspace
                    .update(cx, |workspace, cx| {
                        workspace.toggle_modal(window, cx, |_window, cx| {
                            ComparePickerView::new(title.clone(), candidates.clone(), on_pick, cx)
                        });
                    })
                    .log_err();
            })
            .log_err();
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    /// Fetches both tables' full structural introspection (columns, indexes,
    /// foreign keys, checks), diffs them, and opens the result as a
    /// `SchemaDiffView` -- the "from" side is `left_table` (what gets
    /// altered), the "to" side is `right_table` (the desired shape).
    fn start_schema_compare(
        &mut self,
        id: ConnectionId,
        database: String,
        left_table: String,
        right_table: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (provider, driver) = {
            let store_ref = self.store.read(cx);
            let Some(connection) = store_ref.connections().iter().find(|c| c.config.id == id)
            else {
                return;
            };
            let Some(provider) = connection.provider.clone() else {
                return;
            };
            (provider, connection.config.driver)
        };
        let workspace = self.workspace.clone();
        let store = self.store.downgrade();
        let panel_weak = cx.entity().downgrade();
        let title: SharedString = format!("Schema: {left_table} vs {right_table}").into();
        let from_table = left_table.clone();
        cx.spawn_in(window, async move |_this, cx| {
            let (
                left_columns,
                left_indexes,
                left_fks,
                left_checks,
                right_columns,
                right_indexes,
                right_fks,
                right_checks,
            ) = futures::join!(
                provider.describe_table(&database, &left_table),
                provider.list_indexes(&database, &left_table),
                provider.list_foreign_keys(&database, &left_table),
                provider.list_check_constraints(&database, &left_table),
                provider.describe_table(&database, &right_table),
                provider.list_indexes(&database, &right_table),
                provider.list_foreign_keys(&database, &right_table),
                provider.list_check_constraints(&database, &right_table),
            );
            let from = crate::schema_diff::TableSchema {
                columns: left_columns.unwrap_or_default(),
                indexes: left_indexes.unwrap_or_default(),
                foreign_keys: left_fks.unwrap_or_default(),
                checks: left_checks.unwrap_or_default(),
            };
            let to = crate::schema_diff::TableSchema {
                columns: right_columns.unwrap_or_default(),
                indexes: right_indexes.unwrap_or_default(),
                foreign_keys: right_fks.unwrap_or_default(),
                checks: right_checks.unwrap_or_default(),
            };
            let diff = crate::schema_diff::SchemaDiff::compute(&from, &to);
            workspace
                .update_in(cx, |workspace, window, cx| {
                    let workspace_weak = workspace.weak_handle();
                    let on_run: Arc<dyn Fn(String, &mut Window, &mut App)> =
                        Arc::new(move |script, _window, cx| {
                            let workspace_weak = workspace_weak.clone();
                            let store = store.clone();
                            panel_weak
                                .update_in(cx, |_panel, window, cx| {
                                    DatabasePanel::open_sql_query_with_text(
                                        workspace_weak,
                                        store,
                                        id,
                                        script,
                                        window,
                                        cx,
                                    );
                                })
                                .ok();
                        });
                    let view = cx.new(|cx| {
                        crate::schema_diff::SchemaDiffView::new(
                            &diff,
                            &from_table,
                            driver,
                            title,
                            on_run,
                            window,
                            cx,
                        )
                    });
                    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
                })
                .log_err();
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn open_query_params_prompt(
        &mut self,
        connection_id: ConnectionId,
        sql: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let panel = cx.entity().downgrade();
        let on_run: QueryRunCallback = Arc::new(move |final_sql, window, cx| {
            panel
                .update(cx, |panel, cx| {
                    Self::open_sql_query_with_text(
                        panel.workspace.clone(),
                        panel.store.downgrade(),
                        connection_id,
                        final_sql,
                        window,
                        cx,
                    );
                })
                .ok();
        });
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.toggle_modal(window, cx, |window, cx| {
                    QueryParamsView::new(sql, on_run, window, cx)
                });
            })
            .log_err();
    }

    /// Opens the rename dialog for a table: scans every open console buffer
    /// and this connection's already-cached routine/trigger/event source for
    /// whole-word references to `table` before the user even picks a new
    /// name, so the usage preview is available immediately.
    fn open_rename_table_dialog(
        &mut self,
        id: ConnectionId,
        database: String,
        table: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self
            .store
            .read(cx)
            .connections()
            .iter()
            .any(|c| c.config.id == id)
        {
            return;
        }

        let mut usages = Vec::new();
        let mut console_matches: Vec<(Entity<Editor>, String)> = Vec::new();
        if let Some(workspace) = self.workspace.upgrade() {
            let store = self.store.clone();
            for pane in workspace.read(cx).panes() {
                for editor in pane.read(cx).items_of_type::<Editor>() {
                    if console_connection_for_editor(&editor, &store, cx).is_none() {
                        continue;
                    }
                    let text = editor.read(cx).text(cx);
                    let matches = crate::rename_refactor::find_name_usages(&text, &table);
                    if matches.is_empty() {
                        continue;
                    }
                    let file_label = active_editor_file_path(&editor, cx)
                        .and_then(|path| {
                            path.file_name()
                                .map(|name| name.to_string_lossy().into_owned())
                        })
                        .unwrap_or_else(|| "console".to_string());
                    for usage in &matches {
                        usages.push(RenameTableUsage {
                            label: format!("{file_label}:{}", usage.line).into(),
                            excerpt: usage.excerpt.clone().into(),
                        });
                    }
                    console_matches.push((editor.clone(), text));
                }
            }
        }

        let mut has_db_side_usages = false;
        if let Some(conn) = self
            .store
            .read(cx)
            .connections()
            .iter()
            .find(|c| c.config.id == id)
        {
            let mut db_side_sources: Vec<(String, Option<String>)> = Vec::new();
            if let Some(procedures) = conn.db_procedures.get(&database) {
                db_side_sources.extend(
                    procedures
                        .iter()
                        .map(|p| (format!("procedure {}", p.name), p.definition.clone())),
                );
            }
            if let Some(events) = conn.db_events.get(&database) {
                db_side_sources.extend(
                    events
                        .iter()
                        .map(|e| (format!("event {}", e.name), e.definition.clone())),
                );
            }
            if let Some(triggers) = conn.table_triggers.get(&(database.clone(), table.clone())) {
                db_side_sources.extend(
                    triggers
                        .iter()
                        .map(|t| (format!("trigger {}", t.name), t.definition.clone())),
                );
            }
            for (label, definition) in db_side_sources {
                let Some(definition) = definition else {
                    continue;
                };
                let matches = crate::rename_refactor::find_name_usages(&definition, &table);
                if !matches.is_empty() {
                    has_db_side_usages = true;
                    usages.push(RenameTableUsage {
                        label: label.into(),
                        excerpt: matches[0].excerpt.clone().into(),
                    });
                }
            }
        }

        let store = self.store.clone();
        let workspace = self.workspace.clone();
        let old_table = table.clone();
        let on_confirm: RenameConfirmCallback = Arc::new(move |new_name, window, cx| {
            let database = database.clone();
            let old_table = old_table.clone();
            let console_matches = console_matches.clone();
            let new_name_for_task = new_name.clone();
            let rename_task = store.update(cx, |store, cx| {
                store.rename_table(id, database, old_table.clone(), new_name_for_task, cx)
            });
            let workspace = workspace.clone();
            window
                .spawn(cx, async move |cx| {
                    let result = rename_task.await;
                    if result.is_ok() {
                        for (editor, text) in console_matches {
                            let rewritten = crate::rename_refactor::replace_name_usages(
                                &text, &old_table, &new_name,
                            );
                            editor
                                .update_in(cx, |editor, window, cx| {
                                    editor.set_text(rewritten, window, cx);
                                })
                                .ok();
                        }
                        workspace
                            .update(cx, |workspace, cx| {
                                workspace.show_toast(
                                    Toast::new(
                                        NotificationId::named("db-rename-table-done".into()),
                                        format!("Renamed to \"{new_name}\"."),
                                    ),
                                    cx,
                                );
                            })
                            .ok();
                    } else if let Err(error) = result {
                        workspace
                            .update(cx, |workspace, cx| {
                                workspace.show_toast(
                                    Toast::new(
                                        NotificationId::named("db-rename-table-failed".into()),
                                        format!("Rename failed: {error}"),
                                    ),
                                    cx,
                                );
                            })
                            .ok();
                    }
                    anyhow::Ok(())
                })
                .detach();
        });

        self.workspace
            .update(cx, |workspace, cx| {
                workspace.toggle_modal(window, cx, |window, cx| {
                    RenameTableView::new(table, usages, has_db_side_usages, on_confirm, window, cx)
                });
            })
            .log_err();
    }

    /// Decides whether a table name passes the explorer filter and, if so,
    /// which byte range to highlight. `None` means the table is filtered out.
    /// In regex mode an invalid pattern (passed as `filter_regex == None` while
    /// `is_regex == true`) shows everything without highlight rather than panicking.
    fn table_filter_match(
        name: &str,
        filter_raw: &str,
        filter_regex: Option<&regex::Regex>,
        is_regex: bool,
    ) -> Option<Vec<usize>> {
        if filter_raw.is_empty() {
            return Some(Vec::new());
        }
        if let Some(regex) = filter_regex {
            return regex.find(name).map(|matched| matched.range().collect());
        }
        if is_regex {
            return Some(Vec::new());
        }
        let lower = name.to_lowercase();
        let needle = filter_raw.to_lowercase();
        lower
            .find(&needle)
            .map(|start| (start..start + needle.len()).collect())
    }

    fn toggle_server_objects(&mut self, id: ConnectionId, cx: &mut Context<Self>) {
        if self.server_objects_expanded.contains(&id) {
            self.server_objects_expanded.remove(&id);
            self.serialize_tree_state(cx);
            cx.notify();
            return;
        }
        self.server_objects_expanded.insert(id);
        self.serialize_tree_state(cx);
        if !self.server_users.contains_key(&id) {
            let task = self.store.update(cx, |store, cx| store.list_users(id, cx));
            cx.spawn(async move |this, cx| {
                let users = task.await;
                this.update(cx, |this, cx| {
                    if let Ok(users) = users {
                        this.server_users
                            .insert(id, users.into_iter().map(|u| (u.name, u.host)).collect());
                        cx.notify();
                    }
                })
                .log_err();
                anyhow::Ok(())
            })
            .detach_and_log_err(cx);
        }
        cx.notify();
    }

    fn generate_insert_template(
        table: &str,
        driver: DatabaseDriver,
        columns: &[ColumnInfo],
    ) -> String {
        let qt = driver.quote_identifier(table);
        if columns.is_empty() {
            return format!("INSERT INTO {} () VALUES ();", qt);
        }
        let cols: Vec<String> = columns
            .iter()
            .map(|c| driver.quote_identifier(&c.name))
            .collect();
        let placeholders: Vec<String> = columns.iter().map(|c| format!("'{}'", c.name)).collect();
        format!(
            "INSERT INTO {} ({})\nVALUES ({});",
            qt,
            cols.join(", "),
            placeholders.join(", "),
        )
    }

    fn generate_update_template(
        table: &str,
        driver: DatabaseDriver,
        columns: &[ColumnInfo],
    ) -> String {
        let qt = driver.quote_identifier(table);
        if columns.is_empty() {
            return format!("UPDATE {} SET  WHERE ;", qt);
        }
        let pk = columns
            .iter()
            .find(|c| c.column_key.as_deref() == Some("PRI"));
        let non_pk_cols: Vec<&ColumnInfo> = columns
            .iter()
            .filter(|c| c.column_key.as_deref() != Some("PRI"))
            .collect();
        let set_clause: Vec<String> = non_pk_cols
            .iter()
            .map(|c| format!("{} = '{}'", driver.quote_identifier(&c.name), c.name))
            .collect();
        let where_clause = if let Some(pk_col) = pk {
            format!(
                "{} = '{}'",
                driver.quote_identifier(&pk_col.name),
                pk_col.name
            )
        } else {
            "1 = 1".to_string()
        };
        format!(
            "UPDATE {} SET {}\nWHERE {};",
            qt,
            set_clause.join(",\n       "),
            where_clause
        )
    }

    fn mock_value(data_type: &str, row_num: usize) -> String {
        let upper = data_type.to_uppercase();
        if upper.starts_with("INT")
            || upper.starts_with("BIGINT")
            || upper.starts_with("SMALLINT")
            || upper.starts_with("TINYINT")
            || upper.starts_with("MEDIUMINT")
            || upper.starts_with("SERIAL")
            || upper.starts_with("INTEGER")
        {
            return row_num.to_string();
        }
        if upper.starts_with("NUMERIC") || upper.starts_with("DECIMAL") {
            return format!("{}.{}", row_num, row_num % 100);
        }
        if upper.starts_with("FLOAT") || upper.starts_with("DOUBLE") || upper.starts_with("REAL") {
            return format!("{}.{}", row_num, row_num % 10);
        }
        if upper.starts_with("BOOL") {
            return if row_num.is_multiple_of(2) {
                "true".to_string()
            } else {
                "false".to_string()
            };
        }
        if upper.starts_with("DATE") && !upper.contains("TIME") {
            return format!(
                "'{}-{:02}-{:02}'",
                2024,
                (row_num % 12) + 1,
                (row_num % 28) + 1
            );
        }
        if upper.starts_with("TIMESTAMP") || upper.starts_with("DATETIME") {
            return format!(
                "'{}-{:02}-{:02} {:02}:00:00'",
                2024,
                (row_num % 12) + 1,
                (row_num % 28) + 1,
                row_num % 24
            );
        }
        if upper.starts_with("TIME") {
            return format!("'{:02}:{:02}:00'", row_num % 24, row_num % 60);
        }
        if upper.starts_with("UUID") {
            return format!("'00000000-0000-0000-0000-{:012}'", row_num);
        }
        if upper.starts_with("JSON") {
            return format!("'{{\"id\":{row_num}}}'");
        }
        format!("'value_{row_num}'")
    }

    fn generate_mock_data(
        table: &str,
        driver: DatabaseDriver,
        columns: &[ColumnInfo],
        count: usize,
    ) -> String {
        let qt = driver.quote_identifier(table);
        let insertable_cols: Vec<&ColumnInfo> = columns
            .iter()
            .filter(|c| c.extra != "auto_increment" && c.extra != "GENERATED ALWAYS")
            .collect();

        if insertable_cols.is_empty() {
            return format!("-- No insertable columns found for table {table}");
        }

        let col_list: Vec<String> = insertable_cols
            .iter()
            .map(|c| driver.quote_identifier(&c.name))
            .collect();

        (1..=count)
            .map(|i| {
                let values: Vec<String> = insertable_cols
                    .iter()
                    .map(|c| Self::mock_value(&c.data_type, i))
                    .collect();
                format!(
                    "INSERT INTO {} ({}) VALUES ({});",
                    qt,
                    col_list.join(", "),
                    values.join(", ")
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn generate_delete_template(
        table: &str,
        driver: DatabaseDriver,
        columns: &[ColumnInfo],
    ) -> String {
        let qt = driver.quote_identifier(table);
        let pk = columns
            .iter()
            .find(|c| c.column_key.as_deref() == Some("PRI"));
        let where_clause = if let Some(pk_col) = pk {
            format!(
                "{} = '{}'",
                driver.quote_identifier(&pk_col.name),
                pk_col.name
            )
        } else {
            "1 = 1".to_string()
        };
        format!("DELETE FROM {}\nWHERE {};", qt, where_clause)
    }

    fn selected_connection(&self, cx: &Context<Self>) -> Option<ActiveConnection> {
        match self.selected_entity {
            Some(SelectedEntity::Connection(id)) => self
                .store
                .read(cx)
                .connections()
                .iter()
                .find(|conn| conn.config.id == id)
                .cloned(),
            _ => None,
        }
    }

    /// Whether the selected connection can move up/down among its siblings
    /// (same folder, ordered by `config.order`) — mirrors the sibling lookup
    /// `DatabaseStore::reorder_connection` does internally, so the button's
    /// enabled state always matches whether the move would actually happen.
    fn selected_connection_move_bounds(
        &self,
        id: ConnectionId,
        cx: &Context<Self>,
    ) -> (bool, bool) {
        let store = self.store.read(cx);
        let Some(folder_id) = store
            .connections()
            .iter()
            .find(|conn| conn.config.id == id)
            .map(|conn| conn.config.folder_id)
        else {
            return (false, false);
        };
        let mut siblings: Vec<(ConnectionId, i64)> = store
            .connections()
            .iter()
            .filter(|conn| conn.config.folder_id == folder_id)
            .map(|conn| (conn.config.id, conn.config.order))
            .collect();
        siblings.sort_by_key(|(_, order)| *order);
        let Some(position) = siblings
            .iter()
            .position(|(sibling_id, _)| *sibling_id == id)
        else {
            return (false, false);
        };
        (position > 0, position + 1 < siblings.len())
    }

    /// The single action bar for whichever connection is currently selected
    /// in the tree (`self.selected_entity`), replacing the ten action icons
    /// that used to be duplicated on every connection row. Always rendered
    /// (so the layout doesn't jump on every click); every button disables
    /// itself when nothing selectable is selected.
    fn render_selection_action_bar(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let selected = self.selected_connection(cx);
        let has_selection = selected.is_some();
        let id = selected.as_ref().map(|conn| conn.config.id);
        let is_connected = selected
            .as_ref()
            .is_some_and(|conn| matches!(conn.status, ConnectionStatus::Connected));
        let label = selected.as_ref().map(|conn| conn.config.label.clone());
        let driver = selected.as_ref().map(|conn| conn.config.driver);
        let database = selected
            .as_ref()
            .and_then(|conn| conn.config.database.clone())
            .unwrap_or_default();
        let config_for_edit = selected.as_ref().map(|conn| conn.config.clone());
        let dump_label = driver.and_then(dump_menu_label);
        let (can_move_up, can_move_down) = id
            .map(|id| self.selected_connection_move_bounds(id, cx))
            .unwrap_or((false, false));

        // `IconButton`'s own debug selector defaults to `"ICON-{icon:?}"`
        // (see `IconButton::new`), which is not unique enough for tests to
        // target a specific action here — each button is wrapped in a plain
        // div carrying an explicit, stable selector instead.
        let label_for_exec = label.clone();
        let database_for_exec = database.clone();
        h_flex()
            .gap_1()
            .px_2()
            .py_1()
            .border_t_1()
            .bg(cx.theme().colors().editor_background)
            .child(
                div()
                    .debug_selector(|| "selection-new-query".to_string())
                    .child(
                        IconButton::new("selection-new-query", IconName::File)
                            .icon_size(IconSize::XSmall)
                            .disabled(!has_selection)
                            .tooltip(Tooltip::text(
                                driver.map_or("SQL Queries", new_query_button_label),
                            ))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let (Some(id), Some(label)) = (id, label.clone()) else {
                                    return;
                                };
                                if driver == Some(DatabaseDriver::Aerospike) {
                                    this.open_new_aerospike_view(
                                        id,
                                        label,
                                        database.clone(),
                                        window,
                                        cx,
                                    );
                                    return;
                                }
                                this.workspace
                                    .update(cx, |workspace, cx| {
                                        open_new_sql_query(workspace, id, label, window, cx);
                                    })
                                    .log_err();
                            })),
                    ),
            )
            .child(
                div().debug_selector(|| "selection-exec".to_string()).child(
                    IconButton::new("selection-exec", IconName::Terminal)
                        .icon_size(IconSize::XSmall)
                        .disabled(!has_selection || driver == Some(DatabaseDriver::Aerospike))
                        .tooltip(Tooltip::text(
                            "Exec — run a heavy or multi-statement script, separate from \
                             the SQL Queries console",
                        ))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            let (Some(id), Some(label)) = (id, label_for_exec.clone()) else {
                                return;
                            };
                            this.open_exec_dialog(
                                id,
                                database_for_exec.clone(),
                                label.into(),
                                window,
                                cx,
                            );
                        })),
                ),
            )
            .child(
                div()
                    .debug_selector(|| "selection-connect".to_string())
                    .child(
                        IconButton::new("selection-connect", IconName::PlayFilled)
                            .icon_size(IconSize::XSmall)
                            .disabled(!has_selection || is_connected)
                            .tooltip(Tooltip::text("Connect"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let Some(id) = id else { return };
                                this.store.update(cx, |store, cx| {
                                    store.connect(id, cx).detach_and_log_err(cx);
                                });
                            })),
                    ),
            )
            .child(
                div()
                    .debug_selector(|| "selection-refresh".to_string())
                    .child(
                        IconButton::new("selection-refresh", IconName::RefreshTitle)
                            .icon_size(IconSize::XSmall)
                            .disabled(!has_selection || !is_connected)
                            .tooltip(Tooltip::text("Refresh"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let Some(id) = id else { return };
                                this.store.update(cx, |store, cx| {
                                    store.refresh_schema_cache(id, cx).detach_and_log_err(cx);
                                });
                            })),
                    ),
            )
            .child(
                div()
                    .debug_selector(|| "selection-disconnect".to_string())
                    .child(
                        IconButton::new("selection-disconnect", IconName::Disconnected)
                            .icon_size(IconSize::XSmall)
                            .disabled(!has_selection || !is_connected)
                            .tooltip(Tooltip::text("Disconnect"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let Some(id) = id else { return };
                                this.store.update(cx, |store, cx| {
                                    store.disconnect(id, cx);
                                });
                            })),
                    ),
            )
            .child(
                div()
                    .debug_selector(|| "selection-move-up".to_string())
                    .child(
                        IconButton::new("selection-move-up", IconName::ChevronUp)
                            .icon_size(IconSize::XSmall)
                            .disabled(!can_move_up)
                            .tooltip(Tooltip::text("Move Up"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let Some(id) = id else { return };
                                this.store.update(cx, |store, cx| {
                                    store.reorder_connection(id, -1, cx);
                                });
                            })),
                    ),
            )
            .child(
                div()
                    .debug_selector(|| "selection-move-down".to_string())
                    .child(
                        IconButton::new("selection-move-down", IconName::ChevronDown)
                            .icon_size(IconSize::XSmall)
                            .disabled(!can_move_down)
                            .tooltip(Tooltip::text("Move Down"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let Some(id) = id else { return };
                                this.store.update(cx, |store, cx| {
                                    store.reorder_connection(id, 1, cx);
                                });
                            })),
                    ),
            )
            .child(
                div().debug_selector(|| "selection-edit".to_string()).child(
                    IconButton::new("selection-edit", IconName::Pencil)
                        .icon_size(IconSize::XSmall)
                        .disabled(!has_selection)
                        .tooltip(Tooltip::text("Edit Connection"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            let Some(config) = config_for_edit.clone() else {
                                return;
                            };
                            this.open_edit_connection_modal(config, window, cx);
                        })),
                ),
            )
            .child(
                div().debug_selector(|| "selection-dump".to_string()).child(
                    IconButton::new("selection-dump", IconName::Download)
                        .icon_size(IconSize::XSmall)
                        .disabled(!has_selection || dump_label.is_none())
                        .tooltip(Tooltip::text(dump_label.unwrap_or("Export with dump tool")))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            let Some(id) = id else { return };
                            this.open_dump_dialog(id, Vec::new(), Vec::new(), window, cx);
                        })),
                ),
            )
            .child(
                div()
                    .debug_selector(|| "selection-duplicate".to_string())
                    .child(
                        IconButton::new("selection-duplicate", IconName::Copy)
                            .icon_size(IconSize::XSmall)
                            .disabled(!has_selection)
                            .tooltip(Tooltip::text("Duplicate Connection"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let Some(id) = id else { return };
                                this.store.update(cx, |store, cx| {
                                    store.duplicate_connection(id, cx);
                                });
                            })),
                    ),
            )
            .child(
                div()
                    .debug_selector(|| "selection-delete".to_string())
                    .child(
                        IconButton::new("selection-delete", IconName::Trash)
                            .icon_size(IconSize::XSmall)
                            .disabled(!has_selection)
                            .tooltip(Tooltip::text("Remove Connection"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let Some(id) = id else { return };
                                this.store.update(cx, |store, cx| {
                                    store.remove_connection(id, cx);
                                });
                            })),
                    ),
            )
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        v_flex()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_between()
                    .items_center()
                    .px_2()
                    .py_1()
                    .child(Label::new("Database Explorer").size(LabelSize::Small))
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                IconButton::new("expand-all", IconName::ChevronUpDown)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("Expand all"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.expand_all(cx);
                                    })),
                            )
                            .child(
                                IconButton::new("collapse-all", IconName::ChevronDownUp)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("Collapse all"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.collapse_all(cx);
                                    })),
                            )
                            .child(
                                IconButton::new("open-ddl-source", IconName::FileCode)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("Open SQL schema file"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_ddl_source(window, cx);
                                    })),
                            )
                            .child(
                                IconButton::new("add-connection", IconName::Plus)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("Add Connection"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_add_connection_modal(window, cx);
                                    })),
                            ),
                    ),
            )
            .child(self.render_selection_action_bar(cx))
            .child(
                div().px_2().py_1().border_t_1().child(
                    h_flex()
                        .items_center()
                        .gap_1()
                        .px_1()
                        .py_0p5()
                        .rounded_sm()
                        .border_1()
                        .border_color(cx.theme().colors().border)
                        .bg(cx.theme().colors().editor_background)
                        .child(
                            Icon::new(IconName::MagnifyingGlass)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(div().flex_1().child(self.table_filter_editor.clone()))
                        .child(
                            IconButton::new("table-filter-regex", IconName::Regex)
                                .icon_size(IconSize::XSmall)
                                .toggle_state(self.table_filter_is_regex)
                                .tooltip(Tooltip::text("Match table names by regular expression"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.table_filter_is_regex = !this.table_filter_is_regex;
                                    cx.notify();
                                })),
                        ),
                ),
            )
    }

    fn render_connection_item(
        &self,
        conn: ActiveConnection,
        depth: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let id = conn.config.id;
        let is_collapsed = self.collapsed_connections.contains(&id);
        let indent = tree_indent(depth);
        let connection_folder = conn.config.folder_id;
        let drag_label: SharedString = conn.config.label.clone().into();
        let label = conn.config.label.clone();
        let driver_label = conn.config.driver.to_string();
        let driver = conn.config.driver;

        let status_color = match &conn.status {
            ConnectionStatus::Connected => Color::Success,
            ConnectionStatus::Connecting => Color::Modified,
            ConnectionStatus::Disconnected => Color::Muted,
            ConnectionStatus::Error(_) => Color::Error,
        };
        // Shape (not just color) must carry connection state so it reads under
        // color-vision deficiency: a colored dot alone was indistinguishable
        // between states for a deuteranopic viewer.
        let status_indicator = match &conn.status {
            ConnectionStatus::Connected => {
                Indicator::icon(Icon::new(IconName::Check)).color(status_color)
            }
            ConnectionStatus::Connecting => {
                Indicator::icon(Icon::new(IconName::LoadCircle).with_rotate_animation(2))
                    .color(status_color)
            }
            ConnectionStatus::Disconnected => {
                Indicator::icon(Icon::new(IconName::Dash)).color(status_color)
            }
            ConnectionStatus::Error(_) => {
                Indicator::icon(Icon::new(IconName::XCircleFilled)).color(status_color)
            }
        };
        let is_connected = matches!(conn.status, ConnectionStatus::Connected);
        let is_server_objects_expanded = self.server_objects_expanded.contains(&id);
        let server_users = self.server_users.get(&id).cloned();
        let error_message = if let ConnectionStatus::Error(ref msg) = conn.status {
            Some(msg.clone())
        } else {
            None
        };

        let is_active = self.store.read(cx).active_connection_id() == Some(id);
        let is_selected = self.selected_entity == Some(SelectedEntity::Connection(id));
        let is_before_target = self.drag_target == Some(DropTarget::BeforeConnection(id));
        let is_after_target = self.drag_target == Some(DropTarget::AfterConnection(id));
        let databases = conn.databases.clone();
        let expanded_databases = conn.expanded_databases.clone();
        let expanded_database_set = conn.expanded_database_set.clone();
        let expanded_tables = conn.expanded_tables.clone();
        let expanded_table_set = conn.expanded_table_set;
        let table_filter_raw = self.table_filter_editor.read(cx).text(cx);
        let filter_is_regex = self.table_filter_is_regex;
        let filter_regex = if filter_is_regex && !table_filter_raw.is_empty() {
            regex::Regex::new(&table_filter_raw).ok()
        } else {
            None
        };
        let entity = cx.entity();
        let env_color = conn.config.env_color.clone();
        let db_views = conn.db_views.clone();
        let db_procedures = conn.db_procedures.clone();
        let db_sequences = conn.db_sequences.clone();
        let db_events = conn.db_events.clone();
        let table_indexes = conn.table_indexes.clone();
        let table_fks = conn.table_fks.clone();
        let table_triggers = conn.table_triggers;
        let views_expanded = self.views_expanded.clone();
        let procedures_expanded = self.procedures_expanded.clone();
        let sequences_expanded = self.sequences_expanded.clone();
        let events_expanded = self.events_expanded.clone();
        let indexes_expanded = self.table_indexes_expanded.clone();
        let fks_expanded = self.table_fks_expanded.clone();
        let triggers_expanded = self.table_triggers_expanded.clone();

        div()
            .flex()
            .flex_col()
            .child(
                div()
                    .id(ElementId::from(SharedString::from(format!("conn-header-{}", id))))
                    .debug_selector(|| format!("conn-header-{}", id))
                    .flex()
                    .flex_row()
                    .min_w_full()
                    .items_center()
                    .gap_1()
                    .pr_2()
                    .pl(indent)
                    .py_1()
                    .rounded_sm()
                    .relative()
                    .hover(|style| style.bg(cx.theme().colors().element_hover))
                    .when(is_active || is_selected, |el| {
                        el.bg(cx.theme().colors().element_selected)
                    })
                    .when(is_active || is_selected, |el| {
                        // Absolutely positioned so the accent never perturbs row
                        // layout (a real border would shift every child right by
                        // its width, which previously broke a fixed-offset click
                        // test that clicks the row at a hardcoded x-offset).
                        el.child(
                            div()
                                .absolute()
                                .left_0()
                                .top_0()
                                .bottom_0()
                                .w(px(2.))
                                .bg(cx.theme().colors().text_accent),
                        )
                    })
                    .when(is_before_target, |el| {
                        el.border_t_2().border_color(cx.theme().colors().text_accent)
                    })
                    .when(is_after_target, |el| {
                        el.border_b_2().border_color(cx.theme().colors().text_accent)
                    })
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.selected_entity = Some(SelectedEntity::Connection(id));
                        if is_connected {
                            this.store.update(cx, |store, cx| {
                                store.set_active_connection(id, cx);
                            });
                        }
                        cx.notify();
                    }))
                    .on_drag(DraggedDbItem::Connection(id), move |_, _, _, cx| {
                        Self::drag_preview(drag_label.clone(), IconName::DatabaseZap, cx)
                    })
                    .on_drag_move(cx.listener(move |this, event: &DragMoveEvent<DraggedDbItem>, _, cx| {
                        if !event.bounds.contains(&event.event.position) {
                            return;
                        }
                        let relative_y = (event.event.position.y - event.bounds.origin.y)
                            / event.bounds.size.height;
                        let new_target = Self::connection_drop_zone(relative_y, id);
                        if this.drag_target != Some(new_target) {
                            this.drag_target = Some(new_target);
                            cx.notify();
                        }
                    }))
                    .on_drop(cx.listener(move |this, item: &DraggedDbItem, _, cx| {
                        let fallback = match connection_folder {
                            Some(folder_id) => DropTarget::Folder(folder_id),
                            None => DropTarget::TopLevel,
                        };
                        let target = this.drag_target.unwrap_or(fallback);
                        this.handle_drop(*item, target, cx);
                    }))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            this.deploy_connection_context_menu(id, event.position, window, cx);
                            cx.stop_propagation();
                        }),
                    )
                    .child(
                        div()
                            .id(ElementId::from(SharedString::from(format!(
                                "conn-chevron-{}",
                                id
                            ))))
                            .debug_selector(move || format!("CONN-CHEVRON-{id}"))
                            .flex_none()
                            .cursor_pointer()
                            .child(
                                Icon::new(if is_collapsed {
                                    IconName::ChevronRight
                                } else {
                                    IconName::ChevronDown
                                })
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.toggle_connection_collapsed(id, cx);
                                cx.stop_propagation();
                            })),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .items_center()
                            .when_some(
                                env_color.as_deref().and_then(parse_env_color),
                                |el, color| {
                                    el.child(
                                        div()
                                            .w(px(8.))
                                            .h(px(8.))
                                            .rounded_full()
                                            .bg(color),
                                    )
                                },
                            )
                            .child(
                                div()
                                    .relative()
                                    .flex_none()
                                    .child(brand_icon(driver, IconSize::Small))
                                    .child(
                                        div()
                                            .absolute()
                                            .bottom_neg_0p5()
                                            .right_neg_0p5()
                                            .rounded_full()
                                            .border_1()
                                            .border_color(cx.theme().colors().panel_background)
                                            .child(status_indicator),
                                    ),
                            ),
                    )
                    .child(
                        h_flex()
                            .flex_1()
                            .items_baseline()
                            .gap_1()
                            .overflow_hidden()
                            .child(Label::new(label).size(LabelSize::Small).single_line())
                            .child(
                                Label::new(driver_label)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .single_line(),
                            ),
                    ),
            )
            .when_some(error_message, |el, msg| {
                el.child(
                    h_flex()
                        .gap_1()
                        .items_center()
                        .px_4()
                        .py_1()
                        .child(
                            Icon::new(IconName::Warning)
                                .size(IconSize::XSmall)
                                .color(Color::Error),
                        )
                        .child(Label::new(msg).size(LabelSize::XSmall).color(Color::Error)),
                )
            })
            .when(is_connected && !is_collapsed, |el| {
                el.child(
                    div()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .id(ElementId::from(SharedString::from(format!("server-objects-{}", id))))
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap_1()
                                .pl(tree_indent(depth + 1))
                                .pr_2()
                                .py_1()
                                .cursor_pointer()
                                .hover(|s| s.bg(cx.theme().colors().element_hover))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.toggle_server_objects(id, cx);
                                }))
                                .child(
                                    Icon::new(if is_server_objects_expanded {
                                        IconName::ChevronDown
                                    } else {
                                        IconName::ChevronRight
                                    })
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                                )
                                .child(Icon::new(IconName::Server).size(IconSize::XSmall).color(Color::Muted))
                                .child(Label::new("Server Objects").size(LabelSize::XSmall).color(Color::Muted)),
                        )
                        .when(is_server_objects_expanded, |el| {
                            el.child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap_1()
                                    .pl(tree_indent(depth + 2))
                                    .pr_2()
                                    .py_1()
                                    .child(Icon::new(IconName::Person).size(IconSize::XSmall).color(Color::Muted))
                                    .child(
                                        Label::new(format!(
                                            "Users ({})",
                                            server_users.as_ref().map_or(0, |u| u.len())
                                        ))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                    ),
                            )
                            .when_some(server_users, |el, users| {
                                el.children(users.into_iter().map(|(name, host)| {
                                    div()
                                        .flex()
                                        .flex_row()
                                        .items_center()
                                        .gap_1()
                                        .pl(tree_indent(depth + 3))
                                        .pr_2()
                                        .py_1()
                                        .child(Label::new(name).size(LabelSize::XSmall).single_line())
                                        .child(
                                            Label::new(format!("@{host}"))
                                                .size(LabelSize::XSmall)
                                                .color(Color::Muted)
                                                .single_line(),
                                        )
                                }))
                            })
                        }),
                )
            })
            .when(!is_collapsed, |el| el.when_some(databases, |el, dbs| {
                el.children(dbs.into_iter().map(|db| {
                    let db_name = db.name;
                    let is_db_expanded = expanded_database_set.contains(&db_name);
                    let db_tables = expanded_databases.get(&db_name).cloned();
                    let db_name_for_click = db_name.clone();

                    let db_row = div()
                        .id(ElementId::from(SharedString::from(format!("db-row-{}-{}", id, db_name))))
                        .debug_selector({
                            let db_name = db_name.clone();
                            move || format!("db-row-{}-{}", id, db_name)
                        })
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_1()
                        .pl(tree_indent(depth + 1))
                        .pr_2()
                        .py_1()
                        .cursor_pointer()
                        .hover(|s| s.bg(cx.theme().colors().element_hover))
                        .on_click(cx.listener({
                            let db_name = db_name_for_click;
                            move |this, event: &ClickEvent, window, cx| {
                                this.selected_tree_node = Some(SelectedTreeNode {
                                    connection_id: id,
                                    database: db_name.clone(),
                                    table: None,
                                });
                                if event.modifiers().control && event.click_count() == 1 {
                                    this.open_database_ddl(id, db_name.clone(), window, cx);
                                } else if !event.modifiers().control {
                                    this.store.update(cx, |store, cx| {
                                        store
                                            .toggle_database_expanded(id, db_name.clone(), cx)
                                            .detach_and_log_err(cx);
                                    });
                                }
                            }
                        }))
                        .child(
                            Icon::new(if is_db_expanded {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            })
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                        )
                        .child(
                            Icon::new(IconName::DatabaseZap)
                                .size(IconSize::XSmall)
                                .color(Color::Accent),
                        )
                        .child(Label::new(db_name.clone()).size(LabelSize::Small));

                    let db_ctx_menu = {
                        let entity = entity.clone();
                        let db = db_name.clone();
                        let workspace = self.workspace.clone();
                        move |window: &mut Window, cx: &mut App| {
                            ContextMenu::build(window, cx, {
                                let entity = entity.clone();
                                let db = db.clone();
                                let workspace = workspace.clone();
                                move |menu, _, _| {
                                    menu
                                    .entry(new_query_button_label(driver), None, {
                                        let entity = entity.clone();
                                        let db = db.clone();
                                        let workspace = workspace.clone();
                                        move |window, cx| {
                                            entity.update(cx, |panel, cx| {
                                                let connection = panel
                                                    .store
                                                    .read(cx)
                                                    .connections()
                                                    .iter()
                                                    .find(|conn| conn.config.id == id)
                                                    .cloned();
                                                let Some(connection) = connection else {
                                                    return;
                                                };
                                                if connection.config.driver == DatabaseDriver::Aerospike {
                                                    panel.open_new_aerospike_view(
                                                        id,
                                                        connection.config.label,
                                                        db.clone(),
                                                        window,
                                                        cx,
                                                    );
                                                    return;
                                                }
                                                let sql = format!("SELECT * FROM {db} LIMIT 1;");
                                                workspace
                                                    .update(cx, |workspace, cx| {
                                                        open_sql_query_console_appending(
                                                            workspace,
                                                            id,
                                                            connection.config.label,
                                                            sql,
                                                            window,
                                                            cx,
                                                        );
                                                    })
                                                    .log_err();
                                            });
                                        }
                                    })
                                    .entry("Go to DDL", None, {
                                        let entity = entity.clone();
                                        let db = db.clone();
                                        move |window, cx| {
                                            entity.update(cx, |panel, cx| {
                                                panel.open_database_ddl(id, db.clone(), window, cx);
                                            });
                                        }
                                    })
                                    .entry("Refresh Tables", None, {
                                        let entity = entity.clone();
                                        move |_, cx| {
                                            entity.update(cx, |panel, cx| {
                                                panel.store.update(cx, |store, cx| {
                                                    store.refresh_schema_cache(id, cx).detach_and_log_err(cx);
                                                });
                                            });
                                        }
                                    })
                                    .when_some(dump_menu_label(driver), |menu, label| {
                                        menu.entry(label, None, {
                                            let entity = entity.clone();
                                            let db = db.clone();
                                            move |window, cx| {
                                                entity.update(cx, |panel, cx| {
                                                    panel.open_dump_dialog(
                                                        id,
                                                        vec![db.clone()],
                                                        Vec::new(),
                                                        window,
                                                        cx,
                                                    );
                                                });
                                            }
                                        })
                                    })
                                    .separator()
                                    .entry("View Procedures", None, {
                                        let entity = entity.clone();
                                        let db = db.clone();
                                        let workspace = workspace.clone();
                                        move |window, cx| {
                                            entity.update(cx, |panel, cx| {
                                                let task = panel.store.update(cx, |store, cx| {
                                                    store.list_procedures(id, db.clone(), cx)
                                                });
                                                let title = SharedString::from(format!("{db} – Procedures"));
                                                let store_weak = panel.store.downgrade();
                                                let env_color = connection_env_color(&store_weak, id, cx);
                                                let result_view = cx.new(|cx| {
                                                    ResultView::new(title, cx).with_env_color(env_color)
                                                });
                                                let rv = result_view.clone();
                                                let ws = workspace.clone();
                                                cx.spawn_in(window, async move |_, cx| {
                                                    let result = task.await;
                                                    rv.update(cx, |view, cx| match result {
                                                        Ok(procedures) => {
                                                            let rows: Vec<Vec<Option<String>>> = procedures.iter().map(|p| vec![
                                                                Some(p.name.clone()),
                                                                Some(match p.kind {
                                                                    ProcedureKind::Function => "Function",
                                                                    ProcedureKind::Procedure => "Procedure",
                                                                }.to_string()),
                                                            ]).collect();
                                                            view.set_result(QueryResult {
                                                                columns: vec!["Name".to_string(), "Type".to_string()],
                                                                rows,
                                                                rows_affected: procedures.len() as u64,
                                                                execution_time_ms: 0,
                                                            }, cx);
                                                        }
                                                        Err(e) => view.set_error(format_query_error(&e), cx),
                                                    });
                                                    ws.update_in(cx, |ws, window, cx| {
                                                        ws.add_item_to_active_pane(Box::new(result_view), None, true, window, cx);
                                                    }).log_err();
                                                    anyhow::Ok(())
                                                }).detach_and_log_err(cx);
                                            });
                                        }
                                    })
                                    .entry("View Users", None, {
                                        let entity = entity.clone();
                                        let workspace = workspace.clone();
                                        move |window, cx| {
                                            entity.update(cx, |panel, cx| {
                                                let task = panel.store.update(cx, |store, cx| {
                                                    store.list_users(id, cx)
                                                });
                                                let title = SharedString::from("Users");
                                                let store_weak = panel.store.downgrade();
                                                let env_color = connection_env_color(&store_weak, id, cx);
                                                let result_view = cx.new(|cx| {
                                                    ResultView::new(title, cx).with_env_color(env_color)
                                                });
                                                let rv = result_view.clone();
                                                let ws = workspace.clone();
                                                cx.spawn_in(window, async move |_, cx| {
                                                    let result = task.await;
                                                    rv.update(cx, |view, cx| match result {
                                                        Ok(users) => {
                                                            let rows: Vec<Vec<Option<String>>> = users.iter().map(|u| vec![
                                                                Some(u.name.clone()),
                                                                Some(u.host.clone()),
                                                            ]).collect();
                                                            view.set_result(QueryResult {
                                                                columns: vec!["Name".to_string(), "Host".to_string()],
                                                                rows,
                                                                rows_affected: users.len() as u64,
                                                                execution_time_ms: 0,
                                                            }, cx);
                                                        }
                                                        Err(e) => view.set_error(format_query_error(&e), cx),
                                                    });
                                                    ws.update_in(cx, |ws, window, cx| {
                                                        ws.add_item_to_active_pane(Box::new(result_view), None, true, window, cx);
                                                    }).log_err();
                                                    anyhow::Ok(())
                                                }).detach_and_log_err(cx);
                                            });
                                        }
                                    })
                                    .entry("Show Diagram", None, {
                                        let entity = entity.clone();
                                        let db = db.clone();
                                        move |window, cx| {
                                            entity.update(cx, |panel, cx| {
                                                panel.open_erd_diagram(id, db.clone(), window, cx);
                                            });
                                        }
                                    })
                                    .when(supports_relational_query_features(driver), |menu| {
                                        menu.entry("Search Data…", None, {
                                            let entity = entity.clone();
                                            let db = db.clone();
                                            move |window, cx| {
                                                entity.update(cx, |panel, cx| {
                                                    panel.open_full_text_search(id, db.clone(), window, cx);
                                                });
                                            }
                                        })
                                    })
                                    .separator()
                                    .entry("Copy Name", None, {
                                        move |_, cx| {
                                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(db.clone()));
                                        }
                                    })
                                }
                            })
                        }
                    };

                    div()
                        .flex()
                        .flex_col()
                        .child(
                            right_click_menu(SharedString::from(format!("db-ctx-{}-{}", id, db_name)))
                                .trigger(move |_, _, _| db_row)
                                .menu(db_ctx_menu),
                        )
                        .when(is_db_expanded, |el| {
                            el.when_some(db_tables, |el, tables| {
                                el.children(tables.into_iter().filter_map(|table| {
                                    let table_name = table.name;
                                    let highlight_indices = match Self::table_filter_match(
                                        &table_name,
                                        &table_filter_raw,
                                        filter_regex.as_ref(),
                                        filter_is_regex,
                                    ) {
                                        Some(indices) => indices,
                                        None => return None,
                                    };
                                    let tbl_key = (db_name.clone(), table_name.clone());
                                    let is_table_expanded = expanded_table_set.contains(&tbl_key);
                                    let table_columns = expanded_tables.get(&tbl_key).cloned();
                                    let db_for_table = db_name.clone();
                                    let table_idx_data = table_indexes.get(&tbl_key).cloned().unwrap_or_default();
                                    let table_fk_data = table_fks.get(&tbl_key).cloned().unwrap_or_default();
                                    let fk_columns: HashSet<String> = table_fk_data
                                        .iter()
                                        .map(|fk| fk.from_column.clone())
                                        .collect();
                                    let table_trig_data = table_triggers.get(&tbl_key).cloned().unwrap_or_default();
                                    let is_idx_expanded = indexes_expanded.contains(&(id, db_for_table.clone(), table_name.clone()));
                                    let is_fk_expanded = fks_expanded.contains(&(id, db_for_table.clone(), table_name.clone()));
                                    let is_trig_expanded = triggers_expanded.contains(&(id, db_for_table.clone(), table_name.clone()));

                                    let table_row = div()
                                        .id(ElementId::from(SharedString::from(format!("tbl-row-{}-{}-{}", id, db_for_table, table_name))))
                                        .flex()
                                        .flex_row()
                                        .items_center()
                                        .gap_1()
                                        .pl(tree_indent(depth + 2))
                                        .pr_2()
                                        .py_1()
                                        .cursor_pointer()
                                        .on_click(cx.listener({
                                            let db = db_for_table.clone();
                                            let tbl = table_name.clone();
                                            let workspace = self.workspace.clone();
                                            move |this, event: &ClickEvent, window, cx| {
                                                this.selected_tree_node = Some(SelectedTreeNode {
                                                    connection_id: id,
                                                    database: db.clone(),
                                                    table: Some(tbl.clone()),
                                                });
                                                if event.modifiers().control && event.click_count() == 1 {
                                                    let ddl_task = this.store.update(cx, |store, cx| {
                                                        store.get_table_ddl(id, db.clone(), tbl.clone(), cx)
                                                    });
                                                    let ws = workspace.clone();
                                                    cx.spawn_in(window, async move |this, cx| {
                                                        let ddl = ddl_task.await?;
                                                        this.update_in(cx, |panel, window, cx| {
                                                            Self::open_sql_query_with_text(ws.clone(), panel.store.downgrade(), id, ddl, window, cx);
                                                        }).log_err();
                                                        anyhow::Ok(())
                                                    })
                                                    .detach_and_log_err(cx);
                                                } else if !event.modifiers().control {
                                                    this.store.update(cx, |store, cx| {
                                                        store
                                                            .toggle_table_expanded(
                                                                id, db.clone(), tbl.clone(), cx,
                                                            )
                                                            .detach_and_log_err(cx);
                                                    });
                                                }
                                            }
                                        }))
                                        .child(
                                            Icon::new(if is_table_expanded {
                                                IconName::ChevronDown
                                            } else {
                                                IconName::ChevronRight
                                            })
                                            .size(IconSize::XSmall)
                                            .color(Color::Muted),
                                        )
                                        .child(
                                            Icon::new(IconName::Server)
                                                .size(IconSize::XSmall)
                                                .color(Color::Muted),
                                        )
                                        .child(
                                            div()
                                                .id(ElementId::from(SharedString::from(format!("tbl-label-{}-{}-{}", id, db_for_table, table_name))))
                                                .child(HighlightedLabel::new(table_name.clone(), highlight_indices).size(LabelSize::Small).single_line())
                                                .tooltip(Tooltip::text("Ctrl+click to view DDL")),
                                        )
                                        .child(
                                            IconButton::new(
                                                SharedString::from(format!("view-data-{}-{}-{}", id, db_for_table, table_name)),
                                                IconName::PlayFilled,
                                            )
                                            .icon_size(IconSize::XSmall)
                                            .tooltip(Tooltip::text("View Table Data"))
                                            .on_click(cx.listener({
                                                let db = db_for_table.clone();
                                                let tbl = table_name.clone();
                                                move |this, _, window, cx| {
                                                    let sql = format!(
                                                        "SELECT * FROM {} LIMIT 500",
                                                        driver.quote_identifier(&tbl)
                                                    );
                                                    let store_weak = this.store.downgrade();
                                                    let task = this.store.update(cx, |store, cx| {
                                                        store.execute_query(id, db.clone(), sql, cx)
                                                    });
                                                    let title = SharedString::from(tbl.as_str());
                                                    let workspace = this.workspace.clone();
                                                    let env_color = connection_env_color(&store_weak, id, cx);
                                                    let result_view = cx.new(|cx| {
                                                        ResultView::new(title, cx)
                                                            .with_table_context(store_weak, id, db.clone(), tbl.clone(), window, cx)
                                                            .with_workspace(workspace.clone())
                                                            .with_env_color(env_color)
                                                    });
                                                    let rv = result_view.clone();
                                                    cx.spawn_in(window, async move |_, cx| {
                                                        let outcome = task.await;
                                                        rv.update(cx, |view, cx| match outcome {
                                                            Ok(r) => view.set_result(r, cx),
                                                            Err(e) => view.set_error(format_query_error(&e), cx),
                                                        });
                                                        workspace.update_in(cx, |ws, window, cx| {
                                                            ws.add_item_to_active_pane(Box::new(result_view), None, true, window, cx);
                                                        }).log_err();
                                                        anyhow::Ok(())
                                                    })
                                                    .detach_and_log_err(cx);
                                                }
                                            })),
                                        )
                                        .child(
                                            IconButton::new(
                                                SharedString::from(format!("ddl-{}-{}-{}", id, db_for_table, table_name)),
                                                IconName::Code,
                                            )
                                            .icon_size(IconSize::XSmall)
                                            .tooltip(Tooltip::text("Script as CREATE"))
                                            .on_click(cx.listener({
                                                let db = db_for_table.clone();
                                                let tbl = table_name.clone();
                                                let workspace = self.workspace.clone();
                                                move |this, _, window, cx| {
                                                    let ddl_task = this.store.update(cx, |store, cx| {
                                                        store.get_table_ddl(id, db.clone(), tbl.clone(), cx)
                                                    });
                                                    let tbl_title = tbl.clone();
                                                    let ws = workspace.clone();
                                                    cx.spawn_in(window, async move |this, cx| {
                                                        let ddl = ddl_task.await?;
                                                        this.update_in(cx, |panel, window, cx| {
                                                            Self::open_sql_query_with_text(ws.clone(), panel.store.downgrade(), id, ddl, window, cx);
                                                            let _ = tbl_title;
                                                        }).log_err();
                                                        anyhow::Ok(())
                                                    })
                                                    .detach_and_log_err(cx);
                                                }
                                            })),
                                        )
                                        .child(
                                            IconButton::new(
                                                SharedString::from(format!("insert-{}-{}-{}", id, db_for_table, table_name)),
                                                IconName::TextSnippet,
                                            )
                                            .icon_size(IconSize::XSmall)
                                            .tooltip(Tooltip::text("Script as INSERT / UPDATE / DELETE"))
                                            .on_click(cx.listener({
                                                let tbl = table_name.clone();
                                                let cols = table_columns.clone().unwrap_or_default();
                                                let workspace = self.workspace.clone();
                                                move |this, _, window, cx| {
                                                    let insert = Self::generate_insert_template(&tbl, driver, &cols);
                                                    let update = Self::generate_update_template(&tbl, driver, &cols);
                                                    let delete = Self::generate_delete_template(&tbl, driver, &cols);
                                                    let sql = format!("{}\n\n{}\n\n{}", insert, update, delete);
                                                    Self::open_sql_query_with_text(workspace.clone(), this.store.downgrade(), id, sql, window, cx);
                                                }
                                            })),
                                        );

                                    let ctx_menu = {
                                        let entity = entity.clone();
                                        let db = db_for_table.clone();
                                        let tbl = table_name.clone();
                                        let workspace = self.workspace.clone();
                                        let cols = table_columns.clone().unwrap_or_default();
                                        move |window: &mut Window, cx: &mut App| {
                                            ContextMenu::build(window, cx, {
                                                let entity = entity.clone();
                                                let db = db.clone();
                                                let tbl = tbl.clone();
                                                let workspace = workspace.clone();
                                                let cols = cols.clone();
                                                move |menu, _, _| {
                                                    menu
                                                    .entry("View Table Data", None, {
                                                        let entity = entity.clone();
                                                        let db = db.clone();
                                                        let tbl = tbl.clone();
                                                        let workspace = workspace.clone();
                                                        move |window, cx| {
                                                            entity.update(cx, |panel, cx| {
                                                                let sql = format!(
                                                                    "SELECT * FROM {} LIMIT 500",
                                                                    driver.quote_identifier(&tbl)
                                                                );
                                                                let store_weak = panel.store.downgrade();
                                                                let task = panel.store.update(cx, |store, cx| {
                                                                    store.execute_query(id, db.clone(), sql, cx)
                                                                });
                                                                let title = SharedString::from(tbl.as_str());
                                                                let ws = workspace.clone();
                                                                let env_color = connection_env_color(&store_weak, id, cx);
                                                                let result_view = cx.new(|cx| {
                                                                    ResultView::new(title, cx)
                                                                        .with_table_context(store_weak, id, db.clone(), tbl.clone(), window, cx)
                                                                        .with_workspace(workspace.clone())
                                                                        .with_env_color(env_color)
                                                                });
                                                                let rv = result_view.clone();
                                                                cx.spawn_in(window, async move |_, cx| {
                                                                    let outcome = task.await;
                                                                    rv.update(cx, |view, cx| match outcome {
                                                                        Ok(r) => view.set_result(r, cx),
                                                                        Err(e) => view.set_error(format_query_error(&e), cx),
                                                                    });
                                                                    ws.update_in(cx, |ws, window, cx| {
                                                                        ws.add_item_to_active_pane(Box::new(result_view), None, true, window, cx);
                                                                    }).log_err();
                                                                    anyhow::Ok(())
                                                                })
                                                                .detach_and_log_err(cx);
                                                            });
                                                        }
                                                    })
                                                    .entry("Script as SELECT", None, {
                                                        let entity = entity.clone();
                                                        let tbl = tbl.clone();
                                                        let cols = cols.clone();
                                                        let workspace = workspace.clone();
                                                        move |window, cx| {
                                                            let col_list = if cols.is_empty() {
                                                                "*".to_string()
                                                            } else {
                                                                cols.iter()
                                                                    .map(|c| driver.quote_identifier(&c.name))
                                                                    .collect::<Vec<_>>()
                                                                    .join(", ")
                                                            };
                                                            let sql = format!(
                                                                "SELECT {}\nFROM {};",
                                                                col_list,
                                                                driver.quote_identifier(&tbl)
                                                            );
                                                            entity.update(cx, |panel, cx| {
                                                                Self::open_sql_query_with_text(workspace.clone(), panel.store.downgrade(), id, sql, window, cx);
                                                            });
                                                        }
                                                    })
                                                    .separator()
                                                    .entry("Go to DDL", None, {
                                                        let entity = entity.clone();
                                                        let db = db.clone();
                                                        let tbl = tbl.clone();
                                                        move |window, cx| {
                                                            entity.update(cx, |panel, cx| {
                                                                panel.open_table_ddl(id, db.clone(), tbl.clone(), window, cx);
                                                            });
                                                        }
                                                    })
                                                    .entry("Quick Documentation", None, {
                                                        let entity = entity.clone();
                                                        let db = db.clone();
                                                        let tbl = tbl.clone();
                                                        move |window, cx| {
                                                            entity.update(cx, |panel, cx| {
                                                                panel.open_quick_doc(id, db.clone(), tbl.clone(), window, cx);
                                                            });
                                                        }
                                                    })
                                                    .entry("Modify Table…", None, {
                                                        let entity = entity.clone();
                                                        let db = db.clone();
                                                        let tbl = tbl.clone();
                                                        move |window, cx| {
                                                            entity.update(cx, |panel, cx| {
                                                                panel.open_modify_table(id, db.clone(), tbl.clone(), window, cx);
                                                            });
                                                        }
                                                    })
                                                    .entry("Import Data…", None, {
                                                        let entity = entity.clone();
                                                        let db = db.clone();
                                                        let tbl = tbl.clone();
                                                        move |window, cx| {
                                                            entity.update(cx, |panel, cx| {
                                                                panel.open_data_import(id, db.clone(), tbl.clone(), window, cx);
                                                            });
                                                        }
                                                    })
                                                    .entry("Compare Data…", None, {
                                                        let entity = entity.clone();
                                                        let db = db.clone();
                                                        let tbl = tbl.clone();
                                                        move |window, cx| {
                                                            entity.update(cx, |panel, cx| {
                                                                panel.open_compare_picker(
                                                                    id,
                                                                    db.clone(),
                                                                    tbl.clone(),
                                                                    window,
                                                                    cx,
                                                                );
                                                            });
                                                        }
                                                    })
                                                    .when(supports_relational_query_features(driver), |menu| {
                                                        menu.entry("Compare Schema…", None, {
                                                            let entity = entity.clone();
                                                            let db = db.clone();
                                                            let tbl = tbl.clone();
                                                            move |window, cx| {
                                                                entity.update(cx, |panel, cx| {
                                                                    panel.open_schema_compare_picker(
                                                                        id,
                                                                        db.clone(),
                                                                        tbl.clone(),
                                                                        window,
                                                                        cx,
                                                                    );
                                                                });
                                                            }
                                                        })
                                                    })
                                                    .entry("Copy Table to…", None, {
                                                        let entity = entity.clone();
                                                        let db = db.clone();
                                                        let tbl = tbl.clone();
                                                        move |window, cx| {
                                                            entity.update(cx, |panel, cx| {
                                                                panel.open_copy_table_picker(
                                                                    id,
                                                                    db.clone(),
                                                                    tbl.clone(),
                                                                    window,
                                                                    cx,
                                                                );
                                                            });
                                                        }
                                                    })
                                                    .when_some(dump_menu_label(driver), |menu, label| {
                                                        menu.entry(label, None, {
                                                            let entity = entity.clone();
                                                            let db = db.clone();
                                                            let tbl = tbl.clone();
                                                            move |window, cx| {
                                                                entity.update(cx, |panel, cx| {
                                                                    panel.open_dump_dialog(
                                                                        id,
                                                                        vec![db.clone()],
                                                                        vec![tbl.clone()],
                                                                        window,
                                                                        cx,
                                                                    );
                                                                });
                                                            }
                                                        })
                                                    })
                                                    .entry("Script as CREATE", None, {
                                                        let entity = entity.clone();
                                                        let db = db.clone();
                                                        let tbl = tbl.clone();
                                                        let workspace = workspace.clone();
                                                        move |window, cx| {
                                                            entity.update(cx, |panel, cx| {
                                                                let ddl_task = panel.store.update(cx, |store, cx| {
                                                                    store.get_table_ddl(id, db.clone(), tbl.clone(), cx)
                                                                });
                                                                let ws = workspace.clone();
                                                                cx.spawn_in(window, async move |this, cx| {
                                                                    let ddl = ddl_task.await?;
                                                                    this.update_in(cx, |panel, window, cx| {
                                                                        Self::open_sql_query_with_text(ws.clone(), panel.store.downgrade(), id, ddl, window, cx);
                                                                    }).log_err();
                                                                    anyhow::Ok(())
                                                                })
                                                                .detach_and_log_err(cx);
                                                            });
                                                        }
                                                    })
                                                    .entry("Copy DDL to Clipboard", None, {
                                                        let entity = entity.clone();
                                                        let db = db.clone();
                                                        let tbl = tbl.clone();
                                                        move |window, cx| {
                                                            entity.update(cx, |panel, cx| {
                                                                let ddl_task = panel.store.update(cx, |store, cx| {
                                                                    store.get_table_ddl(id, db.clone(), tbl.clone(), cx)
                                                                });
                                                                cx.spawn_in(window, async move |_, cx| {
                                                                    let ddl = ddl_task.await?;
                                                                    cx.update(|_window, cx| {
                                                                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(ddl));
                                                                    })?;
                                                                    anyhow::Ok(())
                                                                })
                                                                .detach_and_log_err(cx);
                                                            });
                                                        }
                                                    })
                                                    .entry("Script as INSERT", None, {
                                                        let entity = entity.clone();
                                                        let tbl = tbl.clone();
                                                        let cols = cols.clone();
                                                        let workspace = workspace.clone();
                                                        move |window, cx| {
                                                            let sql = Self::generate_insert_template(&tbl, driver, &cols);
                                                            entity.update(cx, |panel, cx| {
                                                                Self::open_sql_query_with_text(workspace.clone(), panel.store.downgrade(), id, sql, window, cx);
                                                            });
                                                        }
                                                    })
                                                    .entry("Script as UPDATE", None, {
                                                        let entity = entity.clone();
                                                        let tbl = tbl.clone();
                                                        let cols = cols.clone();
                                                        let workspace = workspace.clone();
                                                        move |window, cx| {
                                                            let sql = Self::generate_update_template(&tbl, driver, &cols);
                                                            entity.update(cx, |panel, cx| {
                                                                Self::open_sql_query_with_text(workspace.clone(), panel.store.downgrade(), id, sql, window, cx);
                                                            });
                                                        }
                                                    })
                                                    .entry("Script as DELETE", None, {
                                                        let entity = entity.clone();
                                                        let tbl = tbl.clone();
                                                        let cols = cols.clone();
                                                        let workspace = workspace.clone();
                                                        move |window, cx| {
                                                            let sql = Self::generate_delete_template(&tbl, driver, &cols);
                                                            entity.update(cx, |panel, cx| {
                                                                Self::open_sql_query_with_text(workspace.clone(), panel.store.downgrade(), id, sql, window, cx);
                                                            });
                                                        }
                                                    })
                                                    .separator()
                                                    .entry("View Indexes", None, {
                                                        let entity = entity.clone();
                                                        let db = db.clone();
                                                        let tbl = tbl.clone();
                                                        let workspace = workspace.clone();
                                                        move |window, cx| {
                                                            entity.update(cx, |panel, cx| {
                                                                let task = panel.store.update(cx, |store, cx| {
                                                                    store.list_indexes(id, db.clone(), tbl.clone(), cx)
                                                                });
                                                                let title = SharedString::from(format!("{tbl} – Indexes"));
                                                                let store_weak = panel.store.downgrade();
                                                                let env_color = connection_env_color(&store_weak, id, cx);
                                                                let result_view = cx.new(|cx| {
                                                                    ResultView::new(title, cx).with_env_color(env_color)
                                                                });
                                                                let rv = result_view.clone();
                                                                let ws = workspace.clone();
                                                                cx.spawn_in(window, async move |_, cx| {
                                                                    let result = task.await;
                                                                    rv.update(cx, |view, cx| match result {
                                                                        Ok(indexes) => {
                                                                            let rows: Vec<Vec<Option<String>>> = indexes.iter().map(|idx| vec![
                                                                                Some(idx.name.clone()),
                                                                                Some(idx.columns.join(", ")),
                                                                                Some(if idx.unique { "YES" } else { "NO" }.to_string()),
                                                                                Some(idx.index_type.clone()),
                                                                            ]).collect();
                                                                            view.set_result(QueryResult {
                                                                                columns: vec!["Name".to_string(), "Columns".to_string(), "Unique".to_string(), "Type".to_string()],
                                                                                rows,
                                                                                rows_affected: indexes.len() as u64,
                                                                                execution_time_ms: 0,
                                                                            }, cx);
                                                                        }
                                                                        Err(e) => view.set_error(format_query_error(&e), cx),
                                                                    });
                                                                    ws.update_in(cx, |ws, window, cx| {
                                                                        ws.add_item_to_active_pane(Box::new(result_view), None, true, window, cx);
                                                                    }).log_err();
                                                                    anyhow::Ok(())
                                                                }).detach_and_log_err(cx);
                                                            });
                                                        }
                                                    })
                                                    .entry("View Triggers", None, {
                                                        let entity = entity.clone();
                                                        let db = db.clone();
                                                        let tbl = tbl.clone();
                                                        let workspace = workspace.clone();
                                                        move |window, cx| {
                                                            entity.update(cx, |panel, cx| {
                                                                let task = panel.store.update(cx, |store, cx| {
                                                                    store.list_triggers(id, db.clone(), tbl.clone(), cx)
                                                                });
                                                                let title = SharedString::from(format!("{tbl} – Triggers"));
                                                                let store_weak = panel.store.downgrade();
                                                                let env_color = connection_env_color(&store_weak, id, cx);
                                                                let result_view = cx.new(|cx| {
                                                                    ResultView::new(title, cx).with_env_color(env_color)
                                                                });
                                                                let rv = result_view.clone();
                                                                let ws = workspace.clone();
                                                                cx.spawn_in(window, async move |_, cx| {
                                                                    let result = task.await;
                                                                    rv.update(cx, |view, cx| match result {
                                                                        Ok(triggers) => {
                                                                            let rows: Vec<Vec<Option<String>>> = triggers.iter().map(|t| vec![
                                                                                Some(t.name.clone()),
                                                                                Some(t.event.clone()),
                                                                                Some(t.timing.clone()),
                                                                                Some(t.table_name.clone()),
                                                                            ]).collect();
                                                                            view.set_result(QueryResult {
                                                                                columns: vec!["Name".to_string(), "Event".to_string(), "Timing".to_string(), "Table".to_string()],
                                                                                rows,
                                                                                rows_affected: triggers.len() as u64,
                                                                                execution_time_ms: 0,
                                                                            }, cx);
                                                                        }
                                                                        Err(e) => view.set_error(format_query_error(&e), cx),
                                                                    });
                                                                    ws.update_in(cx, |ws, window, cx| {
                                                                        ws.add_item_to_active_pane(Box::new(result_view), None, true, window, cx);
                                                                    }).log_err();
                                                                    anyhow::Ok(())
                                                                }).detach_and_log_err(cx);
                                                            });
                                                        }
                                                    })
                                                    .separator()
                                                    .entry("Rename Table...", None, {
                                                        let entity = entity.clone();
                                                        let db = db.clone();
                                                        let tbl = tbl.clone();
                                                        move |window, cx| {
                                                            entity.update(cx, |panel, cx| {
                                                                panel.open_rename_table_dialog(
                                                                    id,
                                                                    db.clone(),
                                                                    tbl.clone(),
                                                                    window,
                                                                    cx,
                                                                );
                                                            });
                                                        }
                                                    })
                                                    .entry("Truncate Table", None, {
                                                        let entity = entity.clone();
                                                        let db = db.clone();
                                                        let tbl = tbl.clone();
                                                        move |window, cx| {
                                                            entity.update(cx, |panel, cx| {
                                                                let msg = format!("Delete all rows from '{tbl}'? This cannot be undone.");
                                                                let receiver = window.prompt(PromptLevel::Warning, &msg, None, &["Truncate", "Cancel"], cx);
                                                                let store = panel.store.clone();
                                                                let db = db.clone();
                                                                let tbl = tbl.clone();
                                                                cx.spawn_in(window, async move |_, cx| {
                                                                    if receiver.await == Ok(0) {
                                                                        let task = store.update(cx, |store, cx| {
                                                                            store.truncate_table(id, db, tbl, cx)
                                                                        });
                                                                        task.await.log_err();
                                                                    }
                                                                    anyhow::Ok(())
                                                                }).detach_and_log_err(cx);
                                                            });
                                                        }
                                                    })
                                                    .entry("Drop Table", None, {
                                                        let entity = entity.clone();
                                                        let db = db.clone();
                                                        let tbl = tbl.clone();
                                                        move |window, cx| {
                                                            entity.update(cx, |panel, cx| {
                                                                let msg = format!("Drop table '{tbl}'? The table and all its data will be permanently deleted.");
                                                                let receiver = window.prompt(PromptLevel::Warning, &msg, None, &["Drop", "Cancel"], cx);
                                                                let store = panel.store.clone();
                                                                let db = db.clone();
                                                                let tbl = tbl.clone();
                                                                cx.spawn_in(window, async move |_, cx| {
                                                                    if receiver.await == Ok(0) {
                                                                        let drop_task = store.update(cx, |store, cx| {
                                                                            store.drop_table(id, db.clone(), tbl, cx)
                                                                        });
                                                                        if drop_task.await.log_err().is_some() {
                                                                            store.update(cx, |store, cx| {
                                                                                store.refresh_schema_cache(id, cx).detach_and_log_err(cx);
                                                                            });
                                                                        }
                                                                    }
                                                                    anyhow::Ok(())
                                                                }).detach_and_log_err(cx);
                                                            });
                                                        }
                                                    })
                                                    .separator()
                                                    .entry("Generate Mock Data (10 rows)", None, {
                                                        let entity = entity.clone();
                                                        let tbl = tbl.clone();
                                                        let cols = cols.clone();
                                                        let workspace = workspace.clone();
                                                        move |window, cx| {
                                                            let sql = Self::generate_mock_data(&tbl, driver, &cols, 10);
                                                            entity.update(cx, |panel, cx| {
                                                                Self::open_sql_query_with_text(workspace.clone(), panel.store.downgrade(), id, sql, window, cx);
                                                            });
                                                        }
                                                    })
                                                }
                                            })
                                        }
                                    };

                                    Some(div()
                                        .flex()
                                        .flex_col()
                                        .child(
                                            right_click_menu(SharedString::from(format!("tbl-ctx-{}-{}-{}", id, db_for_table, table_name)))
                                                .trigger(move |_, _, _| table_row)
                                                .menu(ctx_menu),
                                        )
                                        .when(is_table_expanded, |el| {
                                            el.when_some(table_columns, |el, columns| {
                                                let fk_columns = fk_columns.clone();
                                                el.children(columns.into_iter().map(move |col| {
                                                    let is_fk = fk_columns.contains(&col.name);
                                                    let overlays = Self::column_overlay_icons(&col, is_fk);
                                                    div()
                                                        .flex()
                                                        .flex_row()
                                                        .items_center()
                                                        .gap_1()
                                                        .pl(tree_indent(depth + 3))
                                                        .pr_2()
                                                        .py_1()
                                                        .child(
                                                            Label::new(col.name)
                                                                .size(LabelSize::XSmall),
                                                        )
                                                        .child(
                                                            Label::new(col.data_type)
                                                                .size(LabelSize::XSmall)
                                                                .color(Color::Muted),
                                                        )
                                                        .children(overlays.into_iter().map(
                                                            |(icon, color)| {
                                                                Icon::new(icon)
                                                                    .size(IconSize::XSmall)
                                                                    .color(color)
                                                            },
                                                        ))
                                                }))
                                            })
                                            .when(!table_idx_data.is_empty(), |el| {
                                                let idx_key = (id, db_for_table.clone(), table_name.clone());
                                                el.child(
                                                    div()
                                                        .id(ElementId::from(SharedString::from(format!("idx-group-row-{}-{}-{}", id, db_for_table, table_name))))
                                                        .flex()
                                                        .flex_col()
                                                        .child(
                                                            div()
                                                                .id(ElementId::from(SharedString::from(format!("idx-group-{}-{}-{}", id, db_for_table, table_name))))
                                                                .flex()
                                                                .flex_row()
                                                                .items_center()
                                                                .gap_1()
                                                                .pl(tree_indent(depth + 3))
                                                                .pr_2()
                                                                .py_1()
                                                                .cursor_pointer()
                                                                .hover(|s| s.bg(cx.theme().colors().element_hover))
                                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                                    if this.table_indexes_expanded.contains(&idx_key) {
                                                                        this.table_indexes_expanded.remove(&idx_key);
                                                                    } else {
                                                                        this.table_indexes_expanded.insert(idx_key.clone());
                                                                    }
                                                                    this.serialize_tree_state(cx);
                                                                    cx.notify();
                                                                }))
                                                                .child(
                                                                    Icon::new(if is_idx_expanded { IconName::ChevronDown } else { IconName::ChevronRight })
                                                                        .size(IconSize::XSmall)
                                                                        .color(Color::Muted),
                                                                )
                                                                .child(Icon::new(IconName::Hash).size(IconSize::XSmall).color(Color::Muted))
                                                                .child(Label::new(format!("Indexes ({})", table_idx_data.len())).size(LabelSize::XSmall).color(Color::Muted)),
                                                        )
                                                        .when(is_idx_expanded, |el| {
                                                            el.children(table_idx_data.into_iter().map(|idx| {
                                                                h_flex()
                                                                    .gap_1()
                                                                    .items_center()
                                                                    .pl(tree_indent(depth + 4))
                                                                    .pr_2()
                                                                    .py_1()
                                                                    .child(Icon::new(IconName::Hash).size(IconSize::XSmall).color(Color::Muted))
                                                                    .child(Label::new(idx.name).size(LabelSize::XSmall))
                                                                    .child(Label::new(format!("({})", idx.columns.join(", "))).size(LabelSize::XSmall).color(Color::Muted))
                                                                    .when(idx.unique, |el| {
                                                                        el.child(Label::new("UNIQUE").size(LabelSize::XSmall).color(Color::Accent))
                                                                    })
                                                            }))
                                                        }),
                                                )
                                            })
                                            .when(!table_fk_data.is_empty(), |el| {
                                                let fk_key = (id, db_for_table.clone(), table_name.clone());
                                                el.child(
                                                    div()
                                                        .flex()
                                                        .flex_col()
                                                        .child(
                                                            div()
                                                                .id(ElementId::from(SharedString::from(format!("fk-group-{}-{}-{}", id, db_for_table, table_name))))
                                                                .flex()
                                                                .flex_row()
                                                                .items_center()
                                                                .gap_1()
                                                                .pl(tree_indent(depth + 3))
                                                                .pr_2()
                                                                .py_1()
                                                                .cursor_pointer()
                                                                .hover(|s| s.bg(cx.theme().colors().element_hover))
                                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                                    if this.table_fks_expanded.contains(&fk_key) {
                                                                        this.table_fks_expanded.remove(&fk_key);
                                                                    } else {
                                                                        this.table_fks_expanded.insert(fk_key.clone());
                                                                    }
                                                                    this.serialize_tree_state(cx);
                                                                    cx.notify();
                                                                }))
                                                                .child(
                                                                    Icon::new(if is_fk_expanded { IconName::ChevronDown } else { IconName::ChevronRight })
                                                                        .size(IconSize::XSmall)
                                                                        .color(Color::Muted),
                                                                )
                                                                .child(Icon::new(IconName::Link).size(IconSize::XSmall).color(Color::Muted))
                                                                .child(Label::new(format!("Foreign Keys ({})", table_fk_data.len())).size(LabelSize::XSmall).color(Color::Muted)),
                                                        )
                                                        .when(is_fk_expanded, |el| {
                                                            el.children(table_fk_data.into_iter().map(|fk| {
                                                                h_flex()
                                                                    .gap_1()
                                                                    .items_center()
                                                                    .pl(tree_indent(depth + 4))
                                                                    .pr_2()
                                                                    .py_1()
                                                                    .child(Icon::new(IconName::Link).size(IconSize::XSmall).color(Color::Muted))
                                                                    .child(Label::new(fk.from_column).size(LabelSize::XSmall))
                                                                    .child(Label::new(format!("→ {}.{}", fk.to_table, fk.to_column)).size(LabelSize::XSmall).color(Color::Muted))
                                                            }))
                                                        }),
                                                )
                                            })
                                            .when(!table_trig_data.is_empty(), |el| {
                                                let trig_key = (id, db_for_table.clone(), table_name.clone());
                                                el.child(
                                                    div()
                                                        .flex()
                                                        .flex_col()
                                                        .child(
                                                            div()
                                                                .id(ElementId::from(SharedString::from(format!("trig-group-{}-{}-{}", id, db_for_table, table_name))))
                                                                .flex()
                                                                .flex_row()
                                                                .items_center()
                                                                .gap_1()
                                                                .pl(tree_indent(depth + 3))
                                                                .pr_2()
                                                                .py_1()
                                                                .cursor_pointer()
                                                                .hover(|s| s.bg(cx.theme().colors().element_hover))
                                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                                    if this.table_triggers_expanded.contains(&trig_key) {
                                                                        this.table_triggers_expanded.remove(&trig_key);
                                                                    } else {
                                                                        this.table_triggers_expanded.insert(trig_key.clone());
                                                                    }
                                                                    this.serialize_tree_state(cx);
                                                                    cx.notify();
                                                                }))
                                                                .child(
                                                                    Icon::new(if is_trig_expanded { IconName::ChevronDown } else { IconName::ChevronRight })
                                                                        .size(IconSize::XSmall)
                                                                        .color(Color::Muted),
                                                                )
                                                                .child(Icon::new(IconName::BoltFilled).size(IconSize::XSmall).color(Color::Muted))
                                                                .child(Label::new(format!("Triggers ({})", table_trig_data.len())).size(LabelSize::XSmall).color(Color::Muted)),
                                                        )
                                                        .when(is_trig_expanded, |el| {
                                                            el.children(table_trig_data.into_iter().map(|t| {
                                                                let entity = entity.clone();
                                                                let workspace = self.workspace.clone();
                                                                let source = t.definition.clone().unwrap_or_else(|| {
                                                                    format!("-- source unavailable for {}", t.name)
                                                                });
                                                                h_flex()
                                                                    .id(ElementId::from(SharedString::from(format!("trigger-{}-{}-{}-{}", id, db_for_table, table_name, t.name))))
                                                                    .debug_selector({
                                                                        let db = db_for_table.clone();
                                                                        let tbl = table_name.clone();
                                                                        let name = t.name.clone();
                                                                        move || format!("trigger-{}-{}-{}-{}", id, db, tbl, name)
                                                                    })
                                                                    .gap_1()
                                                                    .items_center()
                                                                    .pl(tree_indent(depth + 4))
                                                                    .pr_2()
                                                                    .py_1()
                                                                    .cursor_pointer()
                                                                    .hover(|s| s.bg(cx.theme().colors().element_hover))
                                                                    .on_click(move |_, window, cx| {
                                                                        let source = source.clone();
                                                                        entity.update(cx, |panel, cx| {
                                                                            Self::open_sql_query_with_text(workspace.clone(), panel.store.downgrade(), id, source, window, cx);
                                                                        });
                                                                    })
                                                                    .child(Icon::new(IconName::BoltFilled).size(IconSize::XSmall).color(Color::Muted))
                                                                    .child(Label::new(t.name).size(LabelSize::XSmall))
                                                                    .child(Label::new(format!("{} {}", t.timing, t.event)).size(LabelSize::XSmall).color(Color::Muted))
                                                            }))
                                                        }),
                                                )
                                            })
                                        }))
                                }))
                            })
                            .when_some(db_views.get(&db_name).cloned(), |el, view_names| {
                                if view_names.is_empty() {
                                    return el;
                                }
                                let views_key = (id, db_name.clone());
                                let is_views_expanded = views_expanded.contains(&views_key);
                                el.child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .child(
                                            div()
                                                .id(ElementId::from(SharedString::from(format!("views-group-{}-{}", id, db_name))))
                                                .flex()
                                                .flex_row()
                                                .items_center()
                                                .gap_1()
                                                .pl(tree_indent(depth + 2))
                                                .pr_2()
                                                .py_1()
                                                .cursor_pointer()
                                                .hover(|s| s.bg(cx.theme().colors().element_hover))
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    if this.views_expanded.contains(&views_key) {
                                                        this.views_expanded.remove(&views_key);
                                                    } else {
                                                        this.views_expanded.insert(views_key.clone());
                                                    }
                                                    this.serialize_tree_state(cx);
                                                    cx.notify();
                                                }))
                                                .child(
                                                    Icon::new(if is_views_expanded { IconName::ChevronDown } else { IconName::ChevronRight })
                                                        .size(IconSize::XSmall)
                                                        .color(Color::Muted),
                                                )
                                                .child(Icon::new(IconName::Eye).size(IconSize::XSmall).color(Color::Muted))
                                                .child(Label::new(format!("Views ({})", view_names.len())).size(LabelSize::Small).color(Color::Muted)),
                                        )
                                        .when(is_views_expanded, |el| {
                                            el.children(view_names.into_iter().enumerate().map(|(vi, view_name)| {
                                                let view_row = h_flex()
                                                    .gap_1()
                                                    .items_center()
                                                    .pl(tree_indent(depth + 3))
                                                    .pr_2()
                                                    .py_1()
                                                    .child(Icon::new(IconName::Eye).size(IconSize::XSmall).color(Color::Muted))
                                                    .child(Label::new(view_name.clone()).size(LabelSize::Small).single_line());

                                                let view_ctx_menu = {
                                                    let entity = entity.clone();
                                                    let db = db_name.clone();
                                                    let vw = view_name;
                                                    let workspace = self.workspace.clone();
                                                    move |window: &mut Window, cx: &mut App| {
                                                        ContextMenu::build(window, cx, {
                                                            let entity = entity.clone();
                                                            let db = db.clone();
                                                            let vw = vw.clone();
                                                            let workspace = workspace.clone();
                                                            move |menu, _, _| {
                                                                menu
                                                                .entry("Script as CREATE", None, {
                                                                    let entity = entity.clone();
                                                                    let db = db.clone();
                                                                    let vw = vw.clone();
                                                                    let workspace = workspace.clone();
                                                                    move |window, cx| {
                                                                        entity.update(cx, |panel, cx| {
                                                                            let ddl_task = panel.store.update(cx, |store, cx| {
                                                                                store.get_table_ddl(id, db.clone(), vw.clone(), cx)
                                                                            });
                                                                            let ws = workspace.clone();
                                                                            cx.spawn_in(window, async move |this, cx| {
                                                                                let ddl = ddl_task.await?;
                                                                                this.update_in(cx, |panel, window, cx| {
                                                                                    Self::open_sql_query_with_text(ws.clone(), panel.store.downgrade(), id, ddl, window, cx);
                                                                                }).log_err();
                                                                                anyhow::Ok(())
                                                                            })
                                                                            .detach_and_log_err(cx);
                                                                        });
                                                                    }
                                                                })
                                                                .entry("Copy DDL to Clipboard", None, {
                                                                    let entity = entity.clone();
                                                                    let db = db.clone();
                                                                    let vw = vw.clone();
                                                                    move |window, cx| {
                                                                        entity.update(cx, |panel, cx| {
                                                                            let ddl_task = panel.store.update(cx, |store, cx| {
                                                                                store.get_table_ddl(id, db.clone(), vw.clone(), cx)
                                                                            });
                                                                            cx.spawn_in(window, async move |_, cx| {
                                                                                let ddl = ddl_task.await?;
                                                                                cx.update(|_window, cx| {
                                                                                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(ddl));
                                                                                })?;
                                                                                anyhow::Ok(())
                                                                            })
                                                                            .detach_and_log_err(cx);
                                                                        });
                                                                    }
                                                                })
                                                                .entry("View Data", None, {
                                                                    let entity = entity.clone();
                                                                    let db = db.clone();
                                                                    let vw = vw.clone();
                                                                    let workspace = workspace.clone();
                                                                    move |window, cx| {
                                                                        entity.update(cx, |panel, cx| {
                                                                            let sql = format!(
                                                                                "SELECT * FROM `{}`.`{}` LIMIT 500",
                                                                                db, vw
                                                                            );
                                                                            let task = panel.store.update(cx, |store, cx| {
                                                                                store.execute_query(id, db.clone(), sql, cx)
                                                                            });
                                                                            let title = SharedString::from(vw.as_str());
                                                                            let ws = workspace.clone();
                                                                            let store_weak = panel.store.downgrade();
                                                                            let env_color = connection_env_color(&store_weak, id, cx);
                                                                            let result_view = cx.new(|cx| {
                                                                                ResultView::new(title, cx).with_env_color(env_color)
                                                                            });
                                                                            let rv = result_view.clone();
                                                                            cx.spawn_in(window, async move |_, cx| {
                                                                                let outcome = task.await;
                                                                                rv.update(cx, |view, cx| match outcome {
                                                                                    Ok(r) => view.set_result(r, cx),
                                                                                    Err(e) => view.set_error(format_query_error(&e), cx),
                                                                                });
                                                                                ws.update_in(cx, |ws, window, cx| {
                                                                                    ws.add_item_to_active_pane(Box::new(result_view), None, true, window, cx);
                                                                                }).log_err();
                                                                                anyhow::Ok(())
                                                                            })
                                                                            .detach_and_log_err(cx);
                                                                        });
                                                                    }
                                                                })
                                                            }
                                                        })
                                                    }
                                                };

                                                right_click_menu(SharedString::from(format!("view-ctx-{}-{}-{}", id, db_name, vi)))
                                                    .trigger(move |_, _, _| view_row)
                                                    .menu(view_ctx_menu)
                                            }))
                                        }),
                                )
                            })
                            .when_some(db_procedures.get(&db_name).cloned(), |el, procedures| {
                                if procedures.is_empty() {
                                    return el;
                                }
                                let procedures_key = (id, db_name.clone());
                                let is_procedures_expanded = procedures_expanded.contains(&procedures_key);
                                el.child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .child(
                                            div()
                                                .id(ElementId::from(SharedString::from(format!("procedures-group-{}-{}", id, db_name))))
                                                .debug_selector({
                                                    let db_name = db_name.clone();
                                                    move || format!("procedures-group-{}-{}", id, db_name)
                                                })
                                                .flex()
                                                .flex_row()
                                                .items_center()
                                                .gap_1()
                                                .pl(tree_indent(depth + 2))
                                                .pr_2()
                                                .py_1()
                                                .cursor_pointer()
                                                .hover(|s| s.bg(cx.theme().colors().element_hover))
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    if this.procedures_expanded.contains(&procedures_key) {
                                                        this.procedures_expanded.remove(&procedures_key);
                                                    } else {
                                                        this.procedures_expanded.insert(procedures_key.clone());
                                                    }
                                                    this.serialize_tree_state(cx);
                                                    cx.notify();
                                                }))
                                                .child(
                                                    Icon::new(if is_procedures_expanded { IconName::ChevronDown } else { IconName::ChevronRight })
                                                        .size(IconSize::XSmall)
                                                        .color(Color::Muted),
                                                )
                                                .child(Icon::new(IconName::Code).size(IconSize::XSmall).color(Color::Muted))
                                                .child(Label::new(format!("Routines ({})", procedures.len())).size(LabelSize::Small).color(Color::Muted)),
                                        )
                                        .when(is_procedures_expanded, |el| {
                                            el.children(procedures.into_iter().map(|procedure| {
                                                let entity = entity.clone();
                                                let workspace = self.workspace.clone();
                                                let source = procedure.definition.clone().unwrap_or_else(|| {
                                                    format!("-- source unavailable for {}", procedure.name)
                                                });
                                                h_flex()
                                                    .id(ElementId::from(SharedString::from(format!("procedure-{}-{}-{}", id, db_name, procedure.name))))
                                                    .debug_selector({
                                                        let db_name = db_name.clone();
                                                        let name = procedure.name.clone();
                                                        move || format!("procedure-{}-{}-{}", id, db_name, name)
                                                    })
                                                    .gap_1()
                                                    .items_center()
                                                    .pl(tree_indent(depth + 3))
                                                    .pr_2()
                                                    .py_1()
                                                    .cursor_pointer()
                                                    .hover(|s| s.bg(cx.theme().colors().element_hover))
                                                    .on_click(move |_, window, cx| {
                                                        let source = source.clone();
                                                        entity.update(cx, |panel, cx| {
                                                            Self::open_sql_query_with_text(workspace.clone(), panel.store.downgrade(), id, source, window, cx);
                                                        });
                                                    })
                                                    .child(Icon::new(IconName::Code).size(IconSize::XSmall).color(Color::Muted))
                                                    .child(Label::new(procedure.name).size(LabelSize::Small))
                                                    .child(Label::new(match procedure.kind {
                                                        db_client::schema::ProcedureKind::Function => "FUNCTION",
                                                        db_client::schema::ProcedureKind::Procedure => "PROCEDURE",
                                                    }).size(LabelSize::XSmall).color(Color::Muted))
                                                    .into_any_element()
                                            }))
                                        }),
                                )
                            })
                            .when_some(db_sequences.get(&db_name).cloned(), |el, sequences| {
                                if sequences.is_empty() {
                                    return el;
                                }
                                let sequences_key = (id, db_name.clone());
                                let is_sequences_expanded = sequences_expanded.contains(&sequences_key);
                                el.child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .child(
                                            div()
                                                .id(ElementId::from(SharedString::from(format!("sequences-group-{}-{}", id, db_name))))
                                                .debug_selector({
                                                    let db_name = db_name.clone();
                                                    move || format!("sequences-group-{}-{}", id, db_name)
                                                })
                                                .flex()
                                                .flex_row()
                                                .items_center()
                                                .gap_1()
                                                .pl(tree_indent(depth + 2))
                                                .pr_2()
                                                .py_1()
                                                .cursor_pointer()
                                                .hover(|s| s.bg(cx.theme().colors().element_hover))
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    if this.sequences_expanded.contains(&sequences_key) {
                                                        this.sequences_expanded.remove(&sequences_key);
                                                    } else {
                                                        this.sequences_expanded.insert(sequences_key.clone());
                                                    }
                                                    this.serialize_tree_state(cx);
                                                    cx.notify();
                                                }))
                                                .child(
                                                    Icon::new(if is_sequences_expanded { IconName::ChevronDown } else { IconName::ChevronRight })
                                                        .size(IconSize::XSmall)
                                                        .color(Color::Muted),
                                                )
                                                .child(Icon::new(IconName::SquareDot).size(IconSize::XSmall).color(Color::Muted))
                                                .child(Label::new(format!("Sequences ({})", sequences.len())).size(LabelSize::Small).color(Color::Muted)),
                                        )
                                        .when(is_sequences_expanded, |el| {
                                            el.children(sequences.into_iter().map(|seq| {
                                                h_flex()
                                                    .gap_1()
                                                    .items_center()
                                                    .pl(tree_indent(depth + 3))
                                                    .pr_2()
                                                    .py_1()
                                                    .child(Icon::new(IconName::SquareDot).size(IconSize::XSmall).color(Color::Muted))
                                                    .child(Label::new(seq.name).size(LabelSize::Small))
                                                    .when_some(seq.current_value, |el, value| {
                                                        el.child(Label::new(format!("current: {value}")).size(LabelSize::XSmall).color(Color::Muted))
                                                    })
                                            }))
                                        }),
                                )
                            })
                            .when_some(db_events.get(&db_name).cloned(), |el, events| {
                                if events.is_empty() {
                                    return el;
                                }
                                let events_key = (id, db_name.clone());
                                let is_events_expanded = events_expanded.contains(&events_key);
                                el.child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .child(
                                            div()
                                                .id(ElementId::from(SharedString::from(format!("events-group-{}-{}", id, db_name))))
                                                .debug_selector({
                                                    let db_name = db_name.clone();
                                                    move || format!("events-group-{}-{}", id, db_name)
                                                })
                                                .flex()
                                                .flex_row()
                                                .items_center()
                                                .gap_1()
                                                .pl(tree_indent(depth + 2))
                                                .pr_2()
                                                .py_1()
                                                .cursor_pointer()
                                                .hover(|s| s.bg(cx.theme().colors().element_hover))
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    if this.events_expanded.contains(&events_key) {
                                                        this.events_expanded.remove(&events_key);
                                                    } else {
                                                        this.events_expanded.insert(events_key.clone());
                                                    }
                                                    this.serialize_tree_state(cx);
                                                    cx.notify();
                                                }))
                                                .child(
                                                    Icon::new(if is_events_expanded { IconName::ChevronDown } else { IconName::ChevronRight })
                                                        .size(IconSize::XSmall)
                                                        .color(Color::Muted),
                                                )
                                                .child(Icon::new(IconName::ArrowCircle).size(IconSize::XSmall).color(Color::Muted))
                                                .child(Label::new(format!("Events ({})", events.len())).size(LabelSize::Small).color(Color::Muted)),
                                        )
                                        .when(is_events_expanded, |el| {
                                            el.children(events.into_iter().map(|event| {
                                                let entity = entity.clone();
                                                let workspace = self.workspace.clone();
                                                let source = event.definition.clone().unwrap_or_else(|| {
                                                    format!("-- source unavailable for {}", event.name)
                                                });
                                                h_flex()
                                                    .id(ElementId::from(SharedString::from(format!("event-{}-{}-{}", id, db_name, event.name))))
                                                    .debug_selector({
                                                        let db_name = db_name.clone();
                                                        let name = event.name.clone();
                                                        move || format!("event-{}-{}-{}", id, db_name, name)
                                                    })
                                                    .gap_1()
                                                    .items_center()
                                                    .pl(tree_indent(depth + 3))
                                                    .pr_2()
                                                    .py_1()
                                                    .cursor_pointer()
                                                    .hover(|s| s.bg(cx.theme().colors().element_hover))
                                                    .on_click(move |_, window, cx| {
                                                        let source = source.clone();
                                                        entity.update(cx, |panel, cx| {
                                                            Self::open_sql_query_with_text(workspace.clone(), panel.store.downgrade(), id, source, window, cx);
                                                        });
                                                    })
                                                    .child(Icon::new(IconName::ArrowCircle).size(IconSize::XSmall).color(Color::Muted))
                                                    .child(Label::new(event.name).size(LabelSize::Small))
                                                    .when_some(event.status, |el, status| {
                                                        el.child(Label::new(status).size(LabelSize::XSmall).color(Color::Muted))
                                                    })
                                                    .into_any_element()
                                            }))
                                        }),
                                )
                            })
                        })
                }))
            }))
    }

    fn render_history(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let history = self.store.read(cx).query_history().to_vec();
        let is_expanded = self.history_expanded;
        // History is not tied to a connection; bind reopened queries to the
        // active one (run_current_sql_query falls back if it is gone).
        let history_conn_id = self
            .store
            .read(cx)
            .active_connection_id()
            .unwrap_or_default();

        let mut history_items = Vec::new();
        if is_expanded {
            for (i, query) in history.into_iter().take(20).enumerate() {
                let display = if query.len() > 60 {
                    format!("{}…", &query[..60])
                } else {
                    query.clone()
                };
                let item = div()
                    .id(ElementId::from(SharedString::from(format!("history-{i}"))))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .px_3()
                    .py_1()
                    .cursor_pointer()
                    .hover(|style| style.bg(cx.theme().colors().element_hover))
                    .on_click(cx.listener({
                        let workspace = self.workspace.clone();
                        let store = self.store.downgrade();
                        move |_, _, window, cx| {
                            Self::open_sql_query_with_text(
                                workspace.clone(),
                                store.clone(),
                                history_conn_id,
                                query.clone(),
                                window,
                                cx,
                            );
                        }
                    }))
                    .child(
                        Label::new(display)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    );
                history_items.push(item);
            }
        }

        div()
            .flex()
            .flex_col()
            .border_t_1()
            .child(
                div()
                    .id("history-header")
                    .flex()
                    .flex_row()
                    .items_center()
                    .px_2()
                    .py_1()
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.history_expanded = !this.history_expanded;
                        cx.notify();
                    }))
                    .child(
                        Icon::new(if is_expanded {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                    )
                    .child(
                        Label::new("Query History")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .children(history_items)
    }
}

impl DatabasePanel {
    /// Recursively renders folder and connection rows, indenting by `depth`.
    /// Collapsed folders skip their children.
    fn render_tree_nodes(
        &self,
        nodes: Vec<TreeNode>,
        connections: &[ActiveConnection],
        depth: usize,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut elements = Vec::new();
        for node in nodes {
            match node {
                TreeNode::Folder { folder, children } => {
                    let is_collapsed = self.collapsed_folders.contains(&folder.id);
                    let row = self.render_folder_row(&folder, depth, is_collapsed, cx);
                    if is_collapsed || children.is_empty() {
                        elements.push(row);
                    } else {
                        let child_elements =
                            self.render_tree_nodes(children, connections, depth + 1, cx);
                        // A continuous guide line sits under the parent's chevron and runs
                        // down the whole child block, so nesting reads at a glance.
                        let guide_color = cx.theme().colors().border_variant;
                        let guide_left = px(
                            TREE_ROW_BASE_INDENT + depth as f32 * TREE_ROW_INDENT_STEP + 6.,
                        );
                        elements.push(
                            v_flex()
                                .child(row)
                                .child(
                                    div()
                                        .relative()
                                        .child(
                                            div()
                                                .absolute()
                                                .left(guide_left)
                                                .top_0()
                                                .bottom_0()
                                                .w(px(1.))
                                                .bg(guide_color),
                                        )
                                        .child(v_flex().children(child_elements)),
                                )
                                .into_any_element(),
                        );
                    }
                }
                TreeNode::Connection { index } => {
                    if let Some(conn) = connections.get(index) {
                        elements.push(
                            self.render_connection_item(conn.clone(), depth, cx)
                                .into_any_element(),
                        );
                    }
                }
            }
        }
        elements
    }

    /// Right-click target covering the tree and the empty space below it, so a
    /// click on blank panel space offers New Folder / New Connection at the top
    /// level, and a drop there moves the item out of any folder.
    fn render_tree_background(
        &self,
        tree_elements: Vec<AnyElement>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        let is_top_level_target = self.drag_target == Some(DropTarget::TopLevel);
        let show_empty_state = tree_elements.is_empty();
        // The blank-area menu lives on a filler below the rows (and fills the whole
        // panel when there are no rows). Keeping it off the rows means each row's own
        // right-click menu is never shadowed: `right_click_menu` registers a global
        // bubble-phase handler, so a parent menu wrapping the rows would intercept
        // their right-clicks before the row menus could open.
        let background_menu = right_click_menu(ElementId::from("db-tree-background-menu"))
            .trigger(move |_, _, _| {
                v_flex()
                    .id("db-tree-filler")
                    .size_full()
                    .min_h(px(48.))
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .when(show_empty_state, |el| {
                        el.p_4()
                            .child(
                                Icon::new(IconName::DatabaseZap)
                                    .size(IconSize::Medium)
                                    .color(Color::Muted),
                            )
                            .child(
                                Label::new("No connections")
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(
                                Label::new("Right-click to add a folder or connection")
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                    })
            })
            .menu(move |window, cx| new_items_context_menu(entity.clone(), window, cx));

        v_flex()
            .id("db-tree-background")
            .debug_selector(|| "DB-TREE-BACKGROUND".into())
            .items_start()
            .flex_shrink_0()
            .min_w_full()
            .min_h_full()
            .when(is_top_level_target, |el| {
                el.bg(cx.theme().colors().drop_target_background)
            })
            .on_drag_move(
                cx.listener(|this, event: &DragMoveEvent<DraggedDbItem>, _, cx| {
                    if event.bounds.contains(&event.event.position)
                        && this.drag_target != Some(DropTarget::TopLevel)
                    {
                        this.drag_target = Some(DropTarget::TopLevel);
                        cx.notify();
                    }
                }),
            )
            .on_drop(cx.listener(|this, item: &DraggedDbItem, _, cx| {
                this.handle_drop(*item, DropTarget::TopLevel, cx);
            }))
            .children(tree_elements)
            .child(div().flex_1().w_full().child(background_menu))
            .into_any_element()
    }

    /// Opens the New Folder / New Connection menu at `position`. Driven by a
    /// right-click on the panel background that reaches no row with its own menu.
    fn deploy_panel_context_menu(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let menu = new_items_context_menu(cx.entity(), window, cx);
        window.focus(&menu.focus_handle(cx), cx);
        let subscription = cx.subscribe(&menu, |this, _, _: &DismissEvent, cx| {
            this.context_menu.take();
            cx.notify();
        });
        self.context_menu = Some((menu, position, subscription));
        cx.notify();
    }

    fn deploy_connection_context_menu(
        &mut self,
        id: ConnectionId,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = self.store.read(cx);
        let Some(connection) = store
            .connections()
            .iter()
            .find(|connection| connection.config.id == id)
        else {
            return;
        };
        let config = connection.config.clone();
        let is_connected = matches!(connection.status, ConnectionStatus::Connected);
        let driver = config.driver;
        let label = config.label.clone();
        let default_database = config.database.clone();
        let panel = cx.entity();
        let workspace = self.workspace.clone();

        let menu = ContextMenu::build(window, cx, move |menu, _, _| {
            menu.when(is_connected, |menu| {
                menu.entry("Disconnect", None, {
                    let panel = panel.clone();
                    move |_, cx| {
                        panel.update(cx, |panel, cx| {
                            panel.store.update(cx, |store, cx| store.disconnect(id, cx));
                        });
                    }
                })
            })
            .when(!is_connected, |menu| {
                menu.entry("Connect", None, {
                    let panel = panel.clone();
                    move |_, cx| {
                        panel.update(cx, |panel, cx| {
                            panel.store.update(cx, |store, cx| {
                                store.connect(id, cx).detach_and_log_err(cx);
                            });
                        });
                    }
                })
            })
            .entry(new_query_button_label(driver), None, {
                let panel = panel.clone();
                let workspace = workspace.clone();
                let label = label.clone();
                let default_database = default_database.clone();
                move |window, cx| {
                    if driver == DatabaseDriver::Aerospike {
                        panel.update(cx, |panel, cx| {
                            panel.open_new_aerospike_view(
                                id,
                                label.clone(),
                                default_database.clone().unwrap_or_default(),
                                window,
                                cx,
                            );
                        });
                        return;
                    }
                    workspace
                        .update(cx, |workspace, cx| {
                            open_new_sql_query(workspace, id, label.clone(), window, cx);
                        })
                        .log_err();
                }
            })
            .entry("Refresh", None, {
                let panel = panel.clone();
                move |_, cx| {
                    panel.update(cx, |panel, cx| {
                        panel.store.update(cx, |store, cx| {
                            store.refresh_schema_cache(id, cx).detach_and_log_err(cx);
                        });
                    });
                }
            })
            .separator()
            .entry("Edit Connection…", None, {
                let panel = panel.clone();
                let config = config.clone();
                move |window, cx| {
                    panel.update(cx, |panel, cx| {
                        panel.open_edit_connection_modal(config.clone(), window, cx);
                    });
                }
            })
            .entry("Duplicate", None, {
                let panel = panel.clone();
                move |_, cx| {
                    panel.update(cx, |panel, cx| {
                        panel
                            .store
                            .update(cx, |store, cx| store.duplicate_connection(id, cx));
                    });
                }
            })
            .when_some(dump_menu_label(driver), |menu, dump_label| {
                menu.entry(dump_label, None, {
                    let panel = panel.clone();
                    move |window, cx| {
                        panel.update(cx, |panel, cx| {
                            panel.open_dump_dialog(id, Vec::new(), Vec::new(), window, cx);
                        });
                    }
                })
            })
            .when_some(default_database.clone(), |menu, database| {
                menu.entry("Show Diagram", None, {
                    let panel = panel.clone();
                    move |window, cx| {
                        panel.update(cx, |panel, cx| {
                            panel.open_erd_diagram(id, database.clone(), window, cx);
                        });
                    }
                })
            })
            .entry("Copy Name", None, {
                let label = label.clone();
                move |_, cx| {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(label.clone()));
                }
            })
            .separator()
            .entry("Remove", None, {
                let panel = panel.clone();
                move |_, cx| {
                    panel.update(cx, |panel, cx| {
                        panel
                            .store
                            .update(cx, |store, cx| store.remove_connection(id, cx));
                    });
                }
            })
        });
        window.focus(&menu.focus_handle(cx), cx);
        let subscription = cx.subscribe(&menu, |this, _, _: &DismissEvent, cx| {
            this.context_menu.take();
            cx.notify();
        });
        self.context_menu = Some((menu, position, subscription));
        cx.notify();
    }
}

/// Builds the panel's "create" menu (New Folder / New Connection at the top
/// level). Shared by the tree's blank-area menu and the whole-panel right-click.
fn new_items_context_menu(
    panel: Entity<DatabasePanel>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ContextMenu> {
    ContextMenu::build(window, cx, move |menu, _, _| {
        menu.entry("New Folder", None, {
            let panel = panel.clone();
            move |window, cx| {
                panel.update(cx, |panel, cx| panel.start_new_folder(None, window, cx));
            }
        })
        .entry("New Connection", None, {
            let panel = panel.clone();
            move |window, cx| {
                panel.update(cx, |panel, cx| {
                    panel.new_connection_in_folder(None, window, cx)
                });
            }
        })
        .separator()
        .entry("Export Database Explorer…", None, {
            let panel = panel.clone();
            move |window, cx| {
                panel.update(cx, |panel, cx| panel.export_database_explorer(window, cx));
            }
        })
        .entry("Import Database Explorer…", None, move |window, cx| {
            panel.update(cx, |panel, cx| panel.import_database_explorer(window, cx));
        })
    })
}

impl Render for DatabasePanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let connections: Vec<ActiveConnection> =
            self.store.read(cx).connections().iter().cloned().collect();
        let folders: Vec<Folder> = self.store.read(cx).folders().to_vec();
        let nodes = build_folder_tree(&folders, &connections, None, 1);
        let tree_elements = self.render_tree_nodes(nodes, &connections, 0, cx);
        let tree_background = self.render_tree_background(tree_elements, cx);

        v_flex()
            .key_context("DatabasePanel")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &GoToDdl, window, cx| {
                this.go_to_ddl_for_selection(window, cx);
            }))
            .on_action(cx.listener(|this, _: &QuickDocumentation, window, cx| {
                this.quick_doc_for_selection(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ShowDiagram, window, cx| {
                this.show_diagram_for_selection(window, cx);
            }))
            .on_action(cx.listener(|this, _: &GoToObject, window, cx| {
                this.open_go_to_object_palette(window, cx);
            }))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::confirm_selected))
            .on_action(cx.listener(|this, _: &CollapseSelectedEntry, _window, cx| {
                this.collapse_selected(cx);
            }))
            .on_action(cx.listener(|this, _: &ExpandSelectedEntry, _window, cx| {
                this.expand_selected(cx);
            }))
            .on_action(cx.listener(|this, _: &MoveSelectedUp, _window, cx| {
                this.move_selected(-1, cx);
            }))
            .on_action(cx.listener(|this, _: &MoveSelectedDown, _window, cx| {
                this.move_selected(1, cx);
            }))
            .size_full()
            .relative()
            .overflow_hidden()
            .child(div().absolute().inset_0().on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    this.deploy_panel_context_menu(event.position, window, cx);
                }),
            ))
            .child(self.render_toolbar(cx))
            .child(
                div()
                    .id("db-panel-scroll")
                    .debug_selector(|| "DB-TREE-SCROLL".into())
                    .flex()
                    .flex_col()
                    .items_start()
                    .flex_1()
                    .min_h_0()
                    .overflow_scroll()
                    .track_scroll(&self.tree_scroll_handle)
                    .child(tree_background)
                    .custom_scrollbars(
                        Scrollbars::always_visible(ScrollAxes::Both)
                            .tracked_scroll_handle(&self.tree_scroll_handle),
                        window,
                        cx,
                    ),
            )
            .when(!self.store.read(cx).query_history().is_empty(), |el| {
                el.child(self.render_history(cx))
            })
            .when(!self.dump.tasks.is_empty(), |el| {
                el.child(self.render_dump_status(cx))
            })
            .when(!self.export.tasks.is_empty(), |el| {
                el.child(self.render_export_status(cx))
            })
            .children(self.context_menu.as_ref().map(|(menu, position, _)| {
                deferred(
                    anchored()
                        .position(*position)
                        .anchor(gpui::Anchor::TopLeft)
                        .child(menu.clone()),
                )
                .with_priority(3)
            }))
    }
}

impl Focusable for DatabasePanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for DatabasePanel {}

impl Panel for DatabasePanel {
    fn persistent_name() -> &'static str {
        "DatabasePanel"
    }

    fn panel_key() -> &'static str {
        DATABASE_PANEL_KEY
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Left
    }

    fn position_is_valid(&self, _position: DockPosition) -> bool {
        true
    }

    fn set_position(
        &mut self,
        _position: DockPosition,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(260.)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<ui::IconName> {
        Some(IconName::DatabaseZap)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Database Explorer")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        8
    }
}

#[cfg(test)]
mod keybinding_precedence_tests {
    use gpui::{KeyBinding, KeyContext, Keymap, Keystroke, actions};

    actions!(db_console_test, [RunQueryProbe, InlineAssistProbe]);

    // Mirrors the real ctrl-enter conflict exactly: the inline assistant binds it
    // on `!AcpThread > Editor && mode == full`, our SQL console on the SAME-depth
    // `Editor && mode == full` added LAST. Both match at the Editor node, so the
    // console wins only by load index. The editor context has NO `DbQueryEditor`
    // atom on purpose — the live binding no longer relies on one, so this guards
    // the real index-precedence rather than a more-specific context that would
    // pass even if precedence were broken.
    #[test]
    fn db_console_ctrl_enter_beats_inline_assist() {
        let keymap = Keymap::new(vec![
            KeyBinding::new(
                "ctrl-enter",
                InlineAssistProbe,
                Some("!AcpThread > Editor && mode == full"),
            ),
            KeyBinding::new("ctrl-enter", RunQueryProbe, Some("Editor && mode == full")),
        ]);

        let mut editor_context = KeyContext::default();
        editor_context.add("Editor");
        editor_context.set("mode", "full");
        let context_stack = vec![KeyContext::default(), editor_context];

        let keystroke = Keystroke::parse("ctrl-enter").expect("valid keystroke");
        let (bindings, _) = keymap.bindings_for_input(&[keystroke], &context_stack);

        assert_eq!(
            bindings.first().map(|binding| binding.action().name()),
            Some("db_console_test::RunQueryProbe"),
            "the SQL console binding must take ctrl-enter over the inline assistant"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::{TestAppContext, VisualTestContext, actions};
    use project::Project;
    use settings::SettingsStore;
    use workspace::MultiWorkspace;
    use zed_actions::database_panel::ToggleFocus;

    // Stands in for the inline assistant's ctrl-enter binding in the conflict
    // test below, so the real `assistant` crate need not be linked.
    actions!(db_console_probe, [CompetingAssistProbe]);

    #[test]
    fn new_query_button_label_is_driver_specific_for_mongo() {
        assert_eq!(new_query_button_label(DatabaseDriver::MongoDB), "Queries");
        assert_eq!(new_query_button_label(DatabaseDriver::MySQL), "SQL Queries");
        assert_eq!(
            new_query_button_label(DatabaseDriver::Cassandra),
            "SQL Queries"
        );
        assert_eq!(
            new_query_button_label(DatabaseDriver::Aerospike),
            "Get / Put / Scan"
        );
        assert_eq!(new_query_button_label(DatabaseDriver::Redis), "Commands");
    }

    // Before this fix, every driver's console file (including Redis's) got a
    // hardcoded `.sql` extension, so Zed's language-by-extension detection
    // always applied the SQL grammar to Redis's whitespace-tokenized
    // commands (`GET key`, `HGETALL key`) -- neither highlighting them
    // correctly nor leaving them honestly unhighlighted.
    #[test]
    fn console_file_extension_gives_redis_plain_text_not_sql() {
        assert_eq!(console_file_extension(DatabaseDriver::Redis), "txt");
        assert_eq!(console_file_extension(DatabaseDriver::MongoDB), "js");
        assert_eq!(console_file_extension(DatabaseDriver::MySQL), "sql");
        assert_eq!(console_file_extension(DatabaseDriver::Cassandra), "sql");
    }

    // Before this fix, "Search Data…" and "Compare Schema…" were offered
    // unconditionally for every driver, including MongoDB, whose collections
    // have no fixed schema and no SQL WHERE-clause query surface -- clicking
    // either entry could only fail. This predicate is what gates both menu
    // entries via `.when(...)` in `render_connection_item`.
    #[test]
    fn relational_query_features_are_hidden_only_for_mongodb() {
        assert!(!supports_relational_query_features(DatabaseDriver::MongoDB));
        for driver in [
            DatabaseDriver::MySQL,
            DatabaseDriver::PostgreSQL,
            DatabaseDriver::SQLite,
            DatabaseDriver::ClickHouse,
            DatabaseDriver::Cassandra,
            DatabaseDriver::Redis,
            DatabaseDriver::Aerospike,
        ] {
            assert!(
                supports_relational_query_features(driver),
                "{driver:?} should still offer Search Data…/Compare Schema…"
            );
        }
    }

    #[test]
    fn tree_indent_grows_linearly_and_matches_the_previous_hardcoded_offsets() {
        // These four levels correspond to the connection tree's nesting
        // (connection -> server-objects/databases -> tables/users -> columns)
        // and must land close to the pixel values the tree used before every
        // row's indentation was unified onto this one formula.
        assert_eq!(tree_indent(0), px(8.));
        assert_eq!(tree_indent(1), px(20.));
        assert_eq!(tree_indent(2), px(32.));
        assert_eq!(tree_indent(3), px(44.));
        assert_eq!(tree_indent(4), px(56.));
    }

    #[test]
    fn parse_env_color_accepts_valid_hex_and_rejects_junk() {
        assert!(parse_env_color("#f85149").is_some());
        assert!(parse_env_color("3fb950").is_some());
        assert!(parse_env_color("").is_none());
        assert!(parse_env_color("#fff").is_none());
        assert!(parse_env_color("#zzzzzz").is_none());
    }

    #[test]
    fn statement_at_cursor_picks_only_the_statement_under_the_cursor() {
        let text = "SELECT 1;\nSELECT 2;\nSELECT 3;";
        // cursor inside the second statement (after the first ';')
        assert_eq!(super::statement_at_cursor(text, 12), "SELECT 2");
        // cursor in the first statement
        assert_eq!(super::statement_at_cursor(text, 3), "SELECT 1");
        // cursor in the last statement (no trailing ';')
        assert_eq!(super::statement_at_cursor(text, 26), "SELECT 3");
        // single statement, no semicolons
        assert_eq!(super::statement_at_cursor("SELECT 42", 4), "SELECT 42");
    }

    #[test]
    fn statement_runs_in_range_split_sql_and_track_first_rows() {
        let text = "SELECT 1;\n\nSELECT *\nFROM schema.table;\n  SHOW CREATE TABLE schema.table;";
        let runs = super::statement_runs_in_range(text, 0..text.len());

        assert_eq!(
            runs,
            vec![
                super::SqlStatementRun {
                    sql: "SELECT 1".to_string(),
                    start_row: 0,
                    end_row: 0,
                },
                super::SqlStatementRun {
                    sql: "SELECT *\nFROM schema.table".to_string(),
                    start_row: 2,
                    end_row: 3,
                },
                super::SqlStatementRun {
                    sql: "SHOW CREATE TABLE schema.table".to_string(),
                    start_row: 4,
                    end_row: 4,
                },
            ]
        );
    }

    // Fails against the pre-fix code (`statement_at_cursor`/`statement_range_at_cursor`
    // used a plain `rfind(';')`/`find(';')` pair): a semicolon embedded inside a
    // quoted string value -- e.g. a PHP-serialized column value like
    // `a:9:{i:60;i:1;...}` -- was mistaken for a statement boundary, truncating
    // "Run statement at cursor" to a syntactically broken fragment ending mid-string.
    #[test]
    fn statement_at_cursor_does_not_split_on_a_semicolon_inside_a_string_literal() {
        let text = "INSERT INTO t (data) VALUES ('a:9:{i:60;i:1;s:4:\"week\";i:1;}');\nSELECT 1;";
        assert_eq!(
            super::statement_at_cursor(text, 10),
            "INSERT INTO t (data) VALUES ('a:9:{i:60;i:1;s:4:\"week\";i:1;}')"
        );
        // The cursor in the second, unrelated statement must still resolve to
        // just that statement, not accidentally swallow the first one too.
        let second_start = text.rfind("SELECT 1").expect("SELECT 1 present");
        assert_eq!(
            super::statement_at_cursor(text, second_start + 3),
            "SELECT 1"
        );
    }

    #[test]
    fn statement_runs_in_range_does_not_split_a_multi_row_insert_on_embedded_semicolons() {
        let text = "INSERT INTO t (data) VALUES ('a:9:{i:60;i:1;}'), ('b;c');\nSELECT 1;";
        let runs = super::statement_runs_in_range(text, 0..text.len());

        assert_eq!(
            runs,
            vec![
                super::SqlStatementRun {
                    sql: "INSERT INTO t (data) VALUES ('a:9:{i:60;i:1;}'), ('b;c')".to_string(),
                    start_row: 0,
                    end_row: 0,
                },
                super::SqlStatementRun {
                    sql: "SELECT 1".to_string(),
                    start_row: 1,
                    end_row: 1,
                },
            ]
        );
    }

    // Statement splitting is driver-agnostic text scanning, not a SQL parser,
    // so a multi-command MongoDB shell script (commands separated by `;`,
    // filters as JS object literals) must split the same way SQL scripts do.
    #[test]
    fn statement_runs_in_range_splits_a_multi_command_mongo_shell_script() {
        let text =
            "db.users.find({name: \"a;b\"});\ndb.orders.insertOne({total: 10, note: 'x;y'});";
        let runs = super::statement_runs_in_range(text, 0..text.len());

        assert_eq!(
            runs,
            vec![
                super::SqlStatementRun {
                    sql: "db.users.find({name: \"a;b\"})".to_string(),
                    start_row: 0,
                    end_row: 0,
                },
                super::SqlStatementRun {
                    sql: "db.orders.insertOne({total: 10, note: 'x;y'})".to_string(),
                    start_row: 1,
                    end_row: 1,
                },
            ]
        );
    }

    // Redis's console reuses the same `;`-delimited splitting as every other
    // driver (see `console_file_extension`'s doc comment for why Redis
    // doesn't get its own statement-boundary logic): a single command with
    // no trailing `;` is correctly treated as one whole statement, and
    // multiple commands must be `;`-terminated to run individually through
    // the Exec/"run statement at cursor" features, exactly like SQL/Mongo
    // scripts already are.
    #[test]
    fn statement_runs_in_range_splits_a_multi_command_redis_script() {
        let text = "SET greeting \"hi;there\";\nHSET user:1 name \"a;b\"";
        let runs = super::statement_runs_in_range(text, 0..text.len());

        assert_eq!(
            runs,
            vec![
                super::SqlStatementRun {
                    sql: "SET greeting \"hi;there\"".to_string(),
                    start_row: 0,
                    end_row: 0,
                },
                super::SqlStatementRun {
                    sql: "HSET user:1 name \"a;b\"".to_string(),
                    start_row: 1,
                    end_row: 1,
                },
            ]
        );
    }

    #[test]
    fn skip_leading_whitespace_and_comments_handles_all_cases() {
        use super::skip_leading_whitespace_and_comments as skip;

        assert_eq!(skip(""), 0);
        assert_eq!(skip("   \n\t "), 6);
        assert_eq!(skip("-- comment\nSELECT 1"), "-- comment\n".len());
        assert_eq!(skip("/* comment */SELECT 1"), "/* comment */".len());
        assert_eq!(
            skip("/* multi\nline\ncomment */SELECT 1"),
            "/* multi\nline\ncomment */".len()
        );
        assert_eq!(
            skip("  -- one\n  /* two */  -- three\nSELECT 1"),
            "  -- one\n  /* two */  -- three\n".len()
        );
        // No trailing content after the comment at all.
        assert_eq!(skip("-- only a comment"), "-- only a comment".len());
        assert_eq!(skip("   "), 3);
        // Unterminated block comment consumes to the end.
        assert_eq!(skip("/* never closed"), "/* never closed".len());
    }

    // The exact text from the bug report: three statements separated by `;`,
    // the first and third followed by a same-line trailing `-- N` comment.
    const CURSOR_BUG_REPORT_SQL: &str = "SELECT COUNT(*) FROM instruments.financials_values; -- 228475\nSELECT * FROM instruments.financials_values;\n\nSELECT COUNT(*) FROM instruments.financials_indicators; -- 233\n";

    fn run_at_cursor(text: &str, cursor: usize) -> Option<super::SqlStatementRun> {
        super::statement_range_at_cursor(text, cursor)
            .map(|range| super::statement_runs_in_range(text, range))
            .and_then(|runs| runs.into_iter().next())
    }

    #[test]
    fn cursor_on_second_statement_does_not_pick_up_previous_trailing_comment() {
        let text = CURSOR_BUG_REPORT_SQL;
        // A few cursor positions inside "SELECT * FROM instruments.financials_values".
        let second_statement_start = text.find("SELECT *").expect("second SELECT");
        for delta in [0usize, 5, 15, 30] {
            let cursor = second_statement_start + delta;
            let run = run_at_cursor(text, cursor)
                .unwrap_or_else(|| panic!("expected a statement run at cursor {cursor}"));
            assert!(
                !run.sql.contains("228475"),
                "the trailing comment from statement 1 leaked into statement 2's sql: {:?}",
                run.sql
            );
            assert!(
                run.sql
                    .contains("SELECT * FROM instruments.financials_values"),
                "expected statement 2's sql, got {:?}",
                run.sql
            );
            assert_eq!(
                run.start_row, 1,
                "statement 2 must be attributed to row 1 (its own line), not row 0 \
                 (statement 1's line, where the trailing comment sits); cursor={cursor}"
            );
        }
    }

    #[test]
    fn cursor_right_after_second_statements_own_semicolon_still_runs_second_statement() {
        let text = CURSOR_BUG_REPORT_SQL;
        let semicolon = text
            .find("financials_values;\n\nSELECT COUNT")
            .expect("second statement's semicolon")
            + "financials_values;".len()
            - 1;
        assert_eq!(text.as_bytes()[semicolon], b';');
        let cursor = semicolon + 1;

        let run = run_at_cursor(text, cursor).expect("expected a statement run");
        assert!(
            run.sql.contains("financials_values") && !run.sql.contains("financials_indicators"),
            "cursor right after statement 2's own ';' must still run statement 2, got {:?}",
            run.sql
        );
        assert_eq!(run.start_row, 1);
    }

    #[test]
    fn cursor_on_first_and_third_statements_still_resolve_correctly() {
        let text = CURSOR_BUG_REPORT_SQL;

        let first_cursor = text
            .find("COUNT(*) FROM instruments.financials_values")
            .unwrap()
            + 5;
        let first = run_at_cursor(text, first_cursor).expect("first statement run");
        assert!(first.sql.contains("COUNT(*)") && first.sql.contains("financials_values"));
        assert_eq!(first.start_row, 0);

        let third_cursor = text.find("financials_indicators").unwrap() + 5;
        let third = run_at_cursor(text, third_cursor).expect("third statement run");
        assert!(third.sql.contains("financials_indicators"));
        assert_eq!(third.start_row, 3);
    }

    #[test]
    fn table_reference_at_offset_resolves_schema_qualified_table() {
        let text = "SELECT * FROM public.users WHERE id = 1";
        let offset = text.find("users").expect("users in sql") + 2;

        assert_eq!(
            super::table_reference_at_offset(text, offset),
            Some(super::SqlTableReference {
                database: Some("public".to_string()),
                table: "users".to_string(),
                start: text.find("users").expect("users in sql"),
                end: text.find(" WHERE").expect("where in sql"),
            })
        );
    }

    #[test]
    fn table_reference_at_offset_rejects_schema_part() {
        let text = "SELECT * FROM public.users WHERE id = 1";
        let offset = text.find("public").expect("public in sql") + 2;

        assert_eq!(super::table_reference_at_offset(text, offset), None);
    }

    #[test]
    fn database_reference_at_offset_resolves_qualified_database_part() {
        let text = "SELECT * FROM instruments.splits;";
        let database_start = text.find("instruments").expect("database in sql");
        let offset = database_start + 2;

        assert_eq!(
            super::database_reference_at_offset(text, offset),
            Some(super::SqlDatabaseReference {
                database: "instruments".to_string(),
                start: database_start,
                end: database_start + "instruments".len(),
            })
        );
    }

    #[test]
    fn database_reference_at_offset_rejects_table_part() {
        let text = "SELECT * FROM instruments.splits;";
        let offset = text.find("splits").expect("table in sql") + 1;

        assert_eq!(super::database_reference_at_offset(text, offset), None);
    }

    #[test]
    fn database_reference_at_offset_resolves_show_create_database() {
        let text = "SHOW CREATE DATABASE instruments;";
        let database_start = text.find("instruments").expect("database in sql");
        let offset = database_start + 3;

        assert_eq!(
            super::database_reference_at_offset(text, offset),
            Some(super::SqlDatabaseReference {
                database: "instruments".to_string(),
                start: database_start,
                end: database_start + "instruments".len(),
            })
        );
    }

    #[test]
    fn database_reference_at_offset_resolves_show_create_schema() {
        let text = "SHOW CREATE SCHEMA public;";
        let database_start = text.find("public").expect("schema in sql");
        let offset = database_start + 1;

        assert_eq!(
            super::database_reference_at_offset(text, offset),
            Some(super::SqlDatabaseReference {
                database: "public".to_string(),
                start: database_start,
                end: database_start + "public".len(),
            })
        );
    }

    #[test]
    fn database_reference_at_offset_rejects_arbitrary_qualified_word() {
        let text = "SET a.b = 1";
        let offset = text.find("a.b").expect("qualified word in sql");

        assert_eq!(super::database_reference_at_offset(text, offset), None);
    }

    #[test]
    fn column_reference_at_offset_resolves_alias_qualified_column() {
        let text = "SELECT * FROM instruments.splits AS s WHERE s.operation;";
        let column_start = text.rfind("operation").expect("column in sql");
        let offset = column_start + 3;

        assert_eq!(
            super::column_reference_at_offset(text, offset),
            Some(super::SqlColumnReference {
                qualifier: Some("s".to_string()),
                column: "operation".to_string(),
                start: column_start,
                end: column_start + "operation".len(),
            })
        );
    }

    #[test]
    fn column_reference_at_offset_rejects_qualifier_part() {
        let text = "SELECT s.operation FROM instruments.splits AS s;";
        let offset = text.find("s.operation").expect("alias in sql");

        assert_eq!(super::column_reference_at_offset(text, offset), None);
    }

    #[test]
    fn column_reference_at_offset_resolves_bare_column() {
        let text = "SELECT operation FROM splits;";
        let column_start = text.find("operation").expect("column in sql");
        let offset = column_start + 1;

        assert_eq!(
            super::column_reference_at_offset(text, offset),
            Some(super::SqlColumnReference {
                qualifier: None,
                column: "operation".to_string(),
                start: column_start,
                end: column_start + "operation".len(),
            })
        );
    }

    #[test]
    fn from_tables_at_offset_resolves_alias_to_table() {
        let text = "SELECT * FROM instruments.splits AS s WHERE s.operation;";
        let offset = text.rfind("operation").expect("column in sql");
        let tables = super::from_tables_at_offset(text, offset);

        let resolved = crate::sql_completion_provider::resolve_table_ref("s", &tables)
            .expect("alias resolves");
        assert_eq!(resolved.name, "splits");
        assert_eq!(resolved.schema.as_deref(), Some("instruments"));
    }

    #[test]
    fn from_tables_at_offset_resolves_alias_before_from_in_select_list() {
        let text = "SELECT s.opera FROM instruments.splits AS s WHERE s.operation;";
        let offset = text.find("opera").expect("column in sql");
        let tables = super::from_tables_at_offset(text, offset);

        let resolved = crate::sql_completion_provider::resolve_table_ref("s", &tables)
            .expect("alias resolves from a SELECT-list offset before FROM is parsed");
        assert_eq!(resolved.name, "splits");
        assert_eq!(resolved.schema.as_deref(), Some("instruments"));
    }

    #[test]
    fn from_tables_at_offset_scopes_a_reused_alias_to_its_own_subquery() {
        let text = "SELECT outer_s.name FROM accounts AS outer_s WHERE outer_s.id IN \
            (SELECT inner_s.id FROM instruments.splits AS inner_s WHERE inner_s.operation = 1)";
        let inner_offset = text
            .rfind("inner_s.operation")
            .expect("inner column in sql");
        let inner_tables = super::from_tables_at_offset(text, inner_offset);
        let inner_resolved =
            crate::sql_completion_provider::resolve_table_ref("inner_s", &inner_tables)
                .expect("subquery alias resolves");
        assert_eq!(inner_resolved.name, "splits");
        assert!(
            crate::sql_completion_provider::resolve_table_ref("outer_s", &inner_tables).is_none(),
            "the outer query's alias must not leak into the subquery's scope"
        );

        let outer_offset = text.find("outer_s.name").expect("outer column in sql");
        let outer_tables = super::from_tables_at_offset(text, outer_offset);
        let outer_resolved =
            crate::sql_completion_provider::resolve_table_ref("outer_s", &outer_tables)
                .expect("outer alias resolves");
        assert_eq!(outer_resolved.name, "accounts");
    }

    #[test]
    fn from_tables_at_offset_resolves_real_table_three_levels_deep_in_nested_where_in() {
        let text = "SELECT 1 FROM t1 WHERE t1.id IN \
            (SELECT id FROM t2 WHERE id IN \
            (SELECT id FROM t3 WHERE t3.flag = 1))";
        let offset = text.rfind("t3.flag").expect("column in sql");
        let tables = super::from_tables_at_offset(text, offset);

        let resolved = crate::sql_completion_provider::resolve_table_ref("t3", &tables)
            .expect("innermost table resolves three levels deep");
        assert_eq!(resolved.name, "t3");
        assert!(
            crate::sql_completion_provider::resolve_table_ref("t1", &tables).is_none(),
            "the outermost scope must not leak into the innermost subquery"
        );
    }

    #[test]
    fn insert_column_context_resolves_column_list_to_target_table() {
        let text = "INSERT INTO ec_fmedia.quotes_pair_translate (pair_ID, lang_id, shortname) SELECT 1, 2, 3;";
        let offset = text.find("shortname").expect("column in sql") + 2;
        let context = super::insert_column_context_at_offset(text, offset).expect("insert context");
        assert_eq!(context.database.as_deref(), Some("ec_fmedia"));
        assert_eq!(context.table, "quotes_pair_translate");
        let (start, end) = context.column_list.expect("column list span");
        assert!(offset >= start && offset <= end);
        assert_eq!(&text[start..end], "pair_ID, lang_id, shortname");
    }

    #[test]
    fn insert_column_context_finds_on_duplicate_key_update_span() {
        let text = "INSERT INTO t (a, b) SELECT 1, 2 ON DUPLICATE KEY UPDATE a = VALUES(a), b = VALUES(b);";
        let context = super::insert_column_context_at_offset(text, 0).expect("insert context");
        assert_eq!(context.table, "t");
        let clause_start = context.on_duplicate_key_update.expect("clause span");
        assert_eq!(&text[clause_start..clause_start + 1], "a");

        let values_a_offset = text.find("VALUES(a)").expect("VALUES call") + "VALUES(".len();
        assert!(values_a_offset >= clause_start);
    }

    #[test]
    fn derived_table_projections_resolve_simple_passthrough_and_computed_columns() {
        let subquery = "SELECT qdt.currency_ID, qca.currency_short_name, qdt.lang_id, \
            GROUP_CONCAT(qdt.fullname SEPARATOR ' ') AS fullname \
            FROM ec_fmedia.quotes_currency_attr qca \
            LEFT JOIN ec_fmedia.quotes_currency_dat_trans qdt ON qca.currency_ID = qdt.currency_ID \
            GROUP BY qdt.lang_id, qdt.currency_ID";
        let projections = super::derived_table_projections(subquery);

        assert_eq!(
            projections.get("currency_id"),
            Some(&(
                Some("ec_fmedia".to_string()),
                "quotes_currency_dat_trans".to_string(),
                "currency_ID".to_string()
            ))
        );
        assert_eq!(
            projections.get("currency_short_name"),
            Some(&(
                Some("ec_fmedia".to_string()),
                "quotes_currency_attr".to_string(),
                "currency_short_name".to_string()
            ))
        );
        assert_eq!(
            projections.get("lang_id"),
            Some(&(
                Some("ec_fmedia".to_string()),
                "quotes_currency_dat_trans".to_string(),
                "lang_id".to_string()
            ))
        );
        assert_eq!(
            projections.get("fullname"),
            Some(&(
                Some("ec_fmedia".to_string()),
                "quotes_currency_dat_trans".to_string(),
                "fullname".to_string()
            )),
            "a computed aggregate falls back to the table of its first referenced column"
        );
    }

    #[test]
    fn derived_tables_at_offset_resolves_alias_from_outer_select_list() {
        let text = "SELECT q1.lang_id, q1.currency_ID \
            FROM (SELECT qdt.currency_ID, qdt.lang_id FROM ec_fmedia.quotes_currency_dat_trans qdt) q1 \
            JOIN (SELECT qdt.currency_ID FROM ec_fmedia.quotes_currency_dat_trans qdt) q2 \
            ON q1.lang_id = q2.currency_ID;";
        let offset = text.find("q1.lang_id").expect("outer column reference");
        let derived_tables = super::derived_tables_at_offset(text, offset);

        let q1 = derived_tables
            .iter()
            .find(|derived| derived.alias == "q1")
            .expect("q1 recognized as a derived table");
        assert_eq!(
            q1.projections.get("lang_id"),
            Some(&(
                Some("ec_fmedia".to_string()),
                "quotes_currency_dat_trans".to_string(),
                "lang_id".to_string()
            ))
        );
        assert!(
            derived_tables.iter().any(|derived| derived.alias == "q2"),
            "q2 recognized as a derived table alongside q1"
        );
    }

    #[test]
    fn column_reference_at_offset_treats_output_only_select_alias_as_unqualified() {
        let text = "SELECT CONCAT(a.x, b.y) AS pair_shortname FROM a, b;";
        let offset = text.find("pair_shortname").expect("alias in sql") + 3;
        let reference = super::column_reference_at_offset(text, offset).expect("column reference");
        assert_eq!(reference.qualifier, None);
        assert_eq!(reference.column, "pair_shortname");
    }

    #[test]
    fn find_column_definition_offset_matches_backtick_quoted_column() {
        let ddl = "CREATE TABLE `splits` (\n  `id` bigint NOT NULL,\n  `operation` varchar(255) DEFAULT NULL,\n  PRIMARY KEY (`id`)\n)";
        let offset = super::find_column_definition_offset(ddl, "operation").expect("column found");
        let line_start = ddl[..offset].rfind('\n').map(|i| i + 1).unwrap_or(0);
        assert_eq!(&ddl[line_start..offset], "  ");
        assert!(ddl[offset..].starts_with("`operation`"));
    }

    #[test]
    fn find_column_definition_offset_matches_bare_column_case_insensitively() {
        let ddl = "CREATE TABLE splits (\n  id bigint,\n  Operation varchar(255)\n)";
        let offset = super::find_column_definition_offset(ddl, "operation").expect("column found");
        assert!(ddl[offset..].starts_with("Operation"));
    }

    #[test]
    fn find_column_definition_offset_prefers_definition_over_key_line() {
        let ddl = "CREATE TABLE `t` (\n  `key` int NOT NULL,\n  KEY `key` (`key`)\n)";
        let offset = super::find_column_definition_offset(ddl, "key").expect("column found");
        assert!(ddl[offset..].starts_with("`key` int"));
    }

    #[test]
    fn find_column_definition_offset_ignores_substring_columns() {
        let ddl = "CREATE TABLE `t` (\n  `operation_id` int,\n  `operation` varchar(8)\n)";
        let offset = super::find_column_definition_offset(ddl, "operation").expect("column found");
        assert!(ddl[offset..].starts_with("`operation` varchar"));
    }

    #[test]
    fn table_reference_at_offset_resolves_unqualified_table() {
        let text = "SELECT * FROM users WHERE id = 1";
        let offset = text.find("users").expect("users in sql") + 1;

        assert_eq!(
            super::table_reference_at_offset(text, offset),
            Some(super::SqlTableReference {
                database: None,
                table: "users".to_string(),
                start: text.find("users").expect("users in sql"),
                end: text.find(" WHERE").expect("where in sql"),
            })
        );
    }

    #[test]
    fn table_reference_at_offset_resolves_show_create_table() {
        let text = "SHOW CREATE TABLE instruments.splits;";
        let offset = text.find("splits").expect("splits in sql") + 2;

        assert_eq!(
            super::table_reference_at_offset(text, offset),
            Some(super::SqlTableReference {
                database: Some("instruments".to_string()),
                table: "splits".to_string(),
                start: text.find("splits").expect("splits in sql"),
                end: text.find(';').expect("semicolon in sql"),
            })
        );
    }

    #[test]
    fn table_reference_at_offset_rejects_incomplete_reference() {
        let text = "SELECT * FROM public.";
        assert_eq!(super::table_reference_at_offset(text, text.len()), None);
    }

    #[test]
    fn table_reference_at_offset_rejects_bare_reserved_keywords() {
        let text = "SELECT * FROM users WHERE id = 1";
        let select_offset = text.find("SELECT").expect("SELECT in sql") + 1;
        let from_offset = text.find("FROM").expect("FROM in sql") + 1;
        let where_offset = text.find("WHERE").expect("WHERE in sql") + 1;

        assert_eq!(super::table_reference_at_offset(text, select_offset), None);
        assert_eq!(super::table_reference_at_offset(text, from_offset), None);
        assert_eq!(super::table_reference_at_offset(text, where_offset), None);
        // A real table name is unaffected by the keyword filter.
        assert!(super::table_reference_at_offset(text, text.find("users").unwrap() + 1).is_some());
    }

    #[test]
    fn select_table_reference_resolves_multiline_schema_qualified_table() {
        let text = "SELECT *\n  FROM public.users\nWHERE id = 1";

        assert_eq!(
            super::select_table_reference(text),
            Some(super::SqlTableReference {
                database: Some("public".to_string()),
                table: "users".to_string(),
                start: text.find("users").expect("users in sql"),
                end: text.find("\nWHERE").expect("where in sql"),
            })
        );
    }

    #[test]
    fn select_table_reference_resolves_unqualified_table() {
        let text = " select id, name from users where active = 1";

        assert_eq!(
            super::select_table_reference(text),
            Some(super::SqlTableReference {
                database: None,
                table: "users".to_string(),
                start: text.find("users").expect("users in sql"),
                end: text.find(" where").expect("where in sql"),
            })
        );
    }

    #[test]
    fn select_table_reference_rejects_non_select_statement() {
        assert_eq!(
            super::select_table_reference("EXPLAIN SELECT * FROM users"),
            None
        );
    }

    #[test]
    fn show_create_table_reference_resolves_schema_qualified_table() {
        let text = "SHOW CREATE TABLE instruments.splits;";

        assert_eq!(
            super::show_create_table_reference(text),
            Some(super::SqlTableReference {
                database: Some("instruments".to_string()),
                table: "splits".to_string(),
                start: text.find("splits").expect("splits in sql"),
                end: text.find(';').expect("semicolon in sql"),
            })
        );
    }

    #[test]
    fn statement_table_reference_at_offset_resolves_show_create_table() {
        let text = "SHOW CREATE TABLE instruments.splits;";
        let offset = text.find("splits").expect("splits in sql") + 2;

        assert_eq!(
            super::statement_table_reference_at_offset(text, offset),
            Some(super::SqlTableReference {
                database: Some("instruments".to_string()),
                table: "splits".to_string(),
                start: text.find("splits").expect("splits in sql"),
                end: text.find(';').expect("semicolon in sql"),
            })
        );
    }

    #[test]
    fn statement_table_reference_at_offset_rejects_show_create_schema_part() {
        let text = "SHOW CREATE TABLE instruments.splits;";
        let offset = text.find("instruments").expect("schema in sql") + 2;

        assert_eq!(
            super::statement_table_reference_at_offset(text, offset),
            None
        );
    }

    // Guards the live Ctrl+Enter path: the RunQuery handler resolves a file-backed
    // console (no addon) by mapping its file path back to the connection. This
    // round-trips the real path builder so a refactor of either side is caught.
    #[test]
    fn console_path_round_trips_to_its_connection() {
        let target = uuid::Uuid::new_v4();
        let other = uuid::Uuid::new_v4();
        let path = super::connection_query_path(target, "Local MySQL", DatabaseDriver::MySQL);

        assert_eq!(
            super::connection_id_from_console_path(&path, &[other, target]),
            Some(target),
            "a console file path must resolve to the connection embedded in its name"
        );

        // A path outside the queries directory is not a console → no match.
        let unrelated = std::path::Path::new("/tmp/notes.sql");
        assert_eq!(
            super::connection_id_from_console_path(unrelated, &[target]),
            None
        );

        // A console file for an unknown connection resolves to nothing.
        assert_eq!(
            super::connection_id_from_console_path(&path, &[other]),
            None
        );
    }

    // MongoDB's shell syntax is JS method-chaining, not SQL; before this fix
    // `connection_query_path` hardcoded a `.sql` extension for every driver,
    // so a Mongo console's buffer was always highlighted (or not) as SQL.
    #[test]
    fn connection_query_path_gives_mongodb_a_javascript_extension() {
        let id = uuid::Uuid::new_v4();

        let mongo_path = super::connection_query_path(id, "Local Mongo", DatabaseDriver::MongoDB);
        assert_eq!(
            mongo_path.extension().and_then(|ext| ext.to_str()),
            Some("js"),
            "a MongoDB console must not be highlighted as SQL"
        );

        let sql_path = super::connection_query_path(id, "Local MySQL", DatabaseDriver::MySQL);
        assert_eq!(
            sql_path.extension().and_then(|ext| ext.to_str()),
            Some("sql")
        );

        let cassandra_path =
            super::connection_query_path(id, "Local Cassandra", DatabaseDriver::Cassandra);
        assert_eq!(
            cassandra_path.extension().and_then(|ext| ext.to_str()),
            Some("sql"),
            "CQL is close enough to SQL that the SQL grammar highlights it reasonably"
        );
    }

    // `open_new_sql_query` and `open_sql_query_console_appending` both resolve
    // their target file through the same `connection_query_path`, so the two
    // "New Query" entry points (toolbar button and context menu) always open
    // the identical persistent document by construction; a full round trip
    // through the real file-backed workspace item would additionally have to
    // touch the real `paths::config_dir()` (FakeFs does not resolve absolute
    // host paths outside its virtual root), which would risk writing a stray
    // file into a real ~/.config directory during tests, so that path is not
    // exercised end-to-end here. What genuinely needed proving is the
    // "append, don't clobber" merge behavior, tested directly below.
    #[test]
    fn append_sample_query_joins_generated_sql_without_discarding_existing_text() {
        assert_eq!(
            super::append_sample_query("", "SELECT * FROM users LIMIT 1;"),
            "SELECT * FROM users LIMIT 1;",
            "an empty console should be filled with the sample, not prefixed with a blank line"
        );
        assert_eq!(
            super::append_sample_query("   \n  ", "SELECT * FROM users LIMIT 1;"),
            "SELECT * FROM users LIMIT 1;",
            "a whitespace-only console counts as empty"
        );
        assert_eq!(
            super::append_sample_query("SELECT 1;", "SELECT * FROM users LIMIT 1;"),
            "SELECT 1;\nSELECT * FROM users LIMIT 1;",
            "existing content must be preserved, with the sample appended on a new line"
        );
        assert_eq!(
            super::append_sample_query("SELECT 1;\n\n", "SELECT * FROM users LIMIT 1;"),
            "SELECT 1;\nSELECT * FROM users LIMIT 1;",
            "trailing blank lines in the existing console must not accumulate on every append"
        );
    }

    fn connection_in(label: &str, folder_id: Option<FolderId>, order: i64) -> ActiveConnection {
        let config = db_client::ConnectionConfig {
            label: label.to_string(),
            folder_id,
            order,
            auto_connect: false,
            ..Default::default()
        };
        ActiveConnection {
            config,
            status: ConnectionStatus::Disconnected,
            provider: None,
            databases: None,
            expanded_databases: std::collections::HashMap::new(),
            expanded_tables: std::collections::HashMap::new(),
            expanded_database_set: std::collections::HashSet::new(),
            expanded_table_set: std::collections::HashSet::new(),
            db_views: std::collections::HashMap::new(),
            db_procedures: std::collections::HashMap::new(),
            db_sequences: std::collections::HashMap::new(),
            db_events: std::collections::HashMap::new(),
            table_indexes: std::collections::HashMap::new(),
            table_fks: std::collections::HashMap::new(),
            table_triggers: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn build_folder_tree_interleaves_folders_and_connections_by_shared_order() {
        // Folders and connections in this fixture deliberately occupy the
        // same shared `order` space per parent -- the space
        // `DatabaseStore::reposition_item`'s drag-and-drop writes into -- so
        // the render order must interleave them by `order`, not group all
        // folders before all connections.
        let personal = Folder::new("Personal".into(), None, 0);
        let sub = Folder::new("Sub".into(), Some(personal.id), 0);
        let work = Folder::new("Work".into(), None, 2);
        let sub_id = sub.id;
        let work_id = work.id;
        let folders = vec![personal, sub, work];
        let connections = vec![
            connection_in("alpha", None, 1),
            connection_in("beta", None, 3),
            connection_in("inner", Some(sub_id), 0),
            connection_in("w1", Some(work_id), 0),
        ];

        let nodes = super::build_folder_tree(&folders, &connections, None, 1);

        // Top-level order 0..3: Personal(folder,0), alpha(conn,1),
        // Work(folder,2), beta(conn,3) -- a folder and a connection
        // alternate rather than all folders rendering first.
        assert!(matches!(&nodes[0], TreeNode::Folder { folder, .. } if folder.name == "Personal"));
        assert!(
            matches!(&nodes[1], TreeNode::Connection { index } if connections[*index].config.label == "alpha")
        );
        assert!(matches!(&nodes[2], TreeNode::Folder { folder, .. } if folder.name == "Work"));
        assert!(
            matches!(&nodes[3], TreeNode::Connection { index } if connections[*index].config.label == "beta")
        );

        // Personal nests the Sub folder (which holds "inner").
        let TreeNode::Folder { children, .. } = &nodes[0] else {
            panic!("expected Personal folder");
        };
        assert!(matches!(&children[0], TreeNode::Folder { folder, .. } if folder.name == "Sub"));
        let TreeNode::Folder {
            children: sub_children,
            ..
        } = &children[0]
        else {
            panic!("expected Sub folder");
        };
        assert!(
            matches!(&sub_children[0], TreeNode::Connection { index } if connections[*index].config.label == "inner")
        );
    }

    #[test]
    fn build_folder_tree_stops_below_max_depth() {
        // A chain deeper than the limit must not recurse forever; folders past
        // MAX_FOLDER_DEPTH simply are not rendered.
        let mut folders = Vec::new();
        let mut parent = None;
        for level in 0..(db_client::MAX_FOLDER_DEPTH + 2) {
            let folder = Folder::new(format!("L{level}"), parent, 0);
            parent = Some(folder.id);
            folders.push(folder);
        }
        let connections: Vec<ActiveConnection> = Vec::new();

        let nodes = super::build_folder_tree(&folders, &connections, None, 1);
        let mut depth = 0;
        let mut current = nodes;
        while let Some(TreeNode::Folder { children, .. }) = current.into_iter().next() {
            depth += 1;
            current = children;
        }
        assert_eq!(depth, db_client::MAX_FOLDER_DEPTH);
    }

    #[test]
    fn flatten_navigable_entities_matches_render_order_when_expanded() {
        let work = Folder::new("Work".into(), None, 0);
        let personal = Folder::new("Personal".into(), None, 1);
        let work_id = work.id;
        let personal_id = personal.id;
        let folders = vec![work, personal];
        let connections = vec![
            connection_in("alpha", None, 0),
            connection_in("w1", Some(work_id), 0),
        ];
        let alpha_id = connections[0].config.id;
        let w1_id = connections[1].config.id;

        let nodes = super::build_folder_tree(&folders, &connections, None, 1);
        let flat = super::flatten_navigable_entities(&nodes, &connections, &HashSet::default());

        assert_eq!(
            flat,
            vec![
                SelectedEntity::Connection(alpha_id),
                SelectedEntity::Folder(work_id),
                SelectedEntity::Connection(w1_id),
                SelectedEntity::Folder(personal_id),
            ]
        );
    }

    #[test]
    fn flatten_navigable_entities_skips_children_of_collapsed_folders() {
        let work = Folder::new("Work".into(), None, 0);
        let work_id = work.id;
        let folders = vec![work];
        let connections = vec![connection_in("w1", Some(work_id), 0)];
        let nodes = super::build_folder_tree(&folders, &connections, None, 1);

        let mut collapsed = HashSet::default();
        collapsed.insert(work_id);
        let flat = super::flatten_navigable_entities(&nodes, &connections, &collapsed);

        assert_eq!(flat, vec![SelectedEntity::Folder(work_id)]);
    }

    #[test]
    fn flatten_navigable_entities_is_empty_for_no_folders_or_connections() {
        let flat = super::flatten_navigable_entities(&[], &[], &HashSet::default());
        assert!(flat.is_empty());
    }

    fn test_column(key: Option<&str>, is_nullable: bool) -> ColumnInfo {
        ColumnInfo {
            name: "c".to_string(),
            data_type: "int".to_string(),
            is_nullable,
            column_key: key.map(|s| s.to_string()),
            default_value: None,
            extra: String::new(),
        }
    }

    #[test]
    fn column_overlay_icons_reflect_key_and_nullability() {
        let pk = DatabasePanel::column_overlay_icons(&test_column(Some("PRI"), false), false);
        assert!(
            pk.iter()
                .any(|(icon, _)| matches!(icon, IconName::StarFilled))
        );
        assert!(
            pk.iter()
                .any(|(icon, _)| matches!(icon, IconName::SquareDot))
        );

        let fk = DatabasePanel::column_overlay_icons(&test_column(Some("MUL"), true), true);
        assert!(fk.iter().any(|(icon, _)| matches!(icon, IconName::Link)));
        assert!(
            !fk.iter().any(|(icon, _)| matches!(icon, IconName::Hash)),
            "an FK column must not also render a plain index hash"
        );

        let unique = DatabasePanel::column_overlay_icons(&test_column(Some("UNI"), true), false);
        assert!(
            unique
                .iter()
                .any(|(icon, _)| matches!(icon, IconName::Hash))
        );

        let plain = DatabasePanel::column_overlay_icons(&test_column(None, true), false);
        assert!(plain.is_empty());
    }

    #[test]
    fn table_filter_match_substring_and_regex() {
        assert_eq!(
            DatabasePanel::table_filter_match("users", "", None, false),
            Some(Vec::new())
        );
        assert_eq!(
            DatabasePanel::table_filter_match("payment_log", "PAY", None, false),
            Some(vec![0, 1, 2])
        );
        assert_eq!(
            DatabasePanel::table_filter_match("orders", "xyz", None, false),
            None
        );

        let regex = regex::Regex::new("^pay").expect("valid regex");
        assert_eq!(
            DatabasePanel::table_filter_match("payment", "^pay", Some(&regex), true),
            Some(vec![0, 1, 2])
        );
        let regex_miss = regex::Regex::new("^zzz").expect("valid regex");
        assert_eq!(
            DatabasePanel::table_filter_match("payment", "^zzz", Some(&regex_miss), true),
            None
        );
        assert_eq!(
            DatabasePanel::table_filter_match("payment", "(", None, true),
            Some(Vec::new()),
            "an invalid regex pattern must show all rows without panicking"
        );
    }

    // Returns a fixed result row so the end-to-end test runs deterministically
    // without a live database or a Tokio runtime (which would break the
    // GPUI test scheduler's determinism).
    struct MockProvider;

    #[async_trait::async_trait]
    impl db_client::DbProvider for MockProvider {
        async fn ping(&self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn list_databases(&self) -> anyhow::Result<Vec<db_client::DatabaseInfo>> {
            Ok(Vec::new())
        }
        async fn list_tables(&self, _database: &str) -> anyhow::Result<Vec<db_client::TableInfo>> {
            Ok(Vec::new())
        }
        async fn describe_table(
            &self,
            _database: &str,
            _table: &str,
        ) -> anyhow::Result<Vec<db_client::ColumnInfo>> {
            Ok(Vec::new())
        }
        async fn execute_query(
            &self,
            _database: &str,
            _sql: &str,
        ) -> anyhow::Result<db_client::schema::QueryResult> {
            Ok(db_client::schema::QueryResult {
                columns: vec!["one".to_string()],
                rows: vec![vec![Some("1".to_string())]],
                rows_affected: 1,
                execution_time_ms: 0,
            })
        }
        async fn get_table_ddl(&self, _database: &str, _table: &str) -> anyhow::Result<String> {
            Ok("TABLE_DDL".to_string())
        }
        async fn get_database_ddl(&self, _database: &str) -> anyhow::Result<String> {
            Ok("DATABASE_DDL".to_string())
        }
    }

    struct RecordingMockProvider {
        calls: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl db_client::DbProvider for RecordingMockProvider {
        async fn ping(&self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn list_databases(&self) -> anyhow::Result<Vec<db_client::DatabaseInfo>> {
            Ok(Vec::new())
        }
        async fn list_tables(&self, _database: &str) -> anyhow::Result<Vec<db_client::TableInfo>> {
            Ok(Vec::new())
        }
        async fn describe_table(
            &self,
            _database: &str,
            _table: &str,
        ) -> anyhow::Result<Vec<db_client::ColumnInfo>> {
            Ok(Vec::new())
        }
        async fn execute_query(
            &self,
            _database: &str,
            sql: &str,
        ) -> anyhow::Result<db_client::schema::QueryResult> {
            self.calls
                .lock()
                .expect("mock call log should not be poisoned")
                .push(sql.to_string());
            Ok(db_client::schema::QueryResult {
                columns: vec!["sql".to_string()],
                rows: vec![vec![Some(sql.to_string())]],
                rows_affected: 1,
                execution_time_ms: 0,
            })
        }
        async fn get_table_ddl(&self, _database: &str, _table: &str) -> anyhow::Result<String> {
            Ok(String::new())
        }
    }

    struct ErroringMockProvider {
        calls: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
        error: &'static str,
    }

    #[async_trait::async_trait]
    impl db_client::DbProvider for ErroringMockProvider {
        async fn ping(&self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn list_databases(&self) -> anyhow::Result<Vec<db_client::DatabaseInfo>> {
            Ok(Vec::new())
        }
        async fn list_tables(&self, _database: &str) -> anyhow::Result<Vec<db_client::TableInfo>> {
            Ok(Vec::new())
        }
        async fn describe_table(
            &self,
            _database: &str,
            _table: &str,
        ) -> anyhow::Result<Vec<db_client::ColumnInfo>> {
            Ok(Vec::new())
        }
        async fn execute_query(
            &self,
            database: &str,
            sql: &str,
        ) -> anyhow::Result<db_client::schema::QueryResult> {
            self.calls
                .lock()
                .expect("mock call log should not be poisoned")
                .push((database.to_string(), sql.to_string()));
            anyhow::bail!(self.error)
        }
        async fn get_table_ddl(&self, _database: &str, _table: &str) -> anyhow::Result<String> {
            Ok(String::new())
        }
    }

    // End-to-end, no user input: a connected console (mock provider) opens a
    // SQL editor, Ctrl+Enter is simulated, and the result table must appear as a
    // tab in the terminal panel's pane with the query output. Exercises the whole
    // chain — key dispatch → RunQuery handler → execute_query → ResultView tab
    // in the terminal panel's pane.
    #[gpui::test]
    async fn ctrl_enter_executes_query_and_shows_results(cx: &mut TestAppContext) {
        let config = db_client::ConnectionConfig {
            label: "e2e".to_string(),
            auto_connect: false,
            ..Default::default()
        };
        let connection_id = config.id;

        init_test(cx);
        // Load the real shipped keymaps in production order (default then the
        // JetBrains base keymap), exactly as load_default_keymap does. Actions
        // from crates not linked here (e.g. assistant::InlineAssist) are skipped
        // by allow_partial_failure, but editor:: and database_panel:: resolve —
        // so the genuine ctrl-enter conflict (editor::NewlineBelow at `Editor &&
        // mode == full`, same as the inline assistant) is reproduced against the
        // actual asset files and load order.
        cx.update(|cx| {
            let mut default_bindings = settings::KeymapFile::load_asset_allow_partial_failure(
                "keymaps/default-linux.json",
                cx,
            )
            .expect("load default-linux keymap");
            for binding in &mut default_bindings {
                binding.set_meta(settings::KeybindSource::Default.meta());
            }
            cx.bind_keys(default_bindings);

            let mut base_bindings = settings::KeymapFile::load_asset_allow_partial_failure(
                "keymaps/linux/jetbrains.json",
                cx,
            )
            .expect("load jetbrains keymap");
            for binding in &mut base_bindings {
                binding.set_meta(settings::KeybindSource::Base.meta());
            }
            cx.bind_keys(base_bindings);
        });

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs.clone(), [], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        // Register the action handlers exactly as zed::register_actions does in
        // the real app (RunQuery → run_current_sql_query). A competitor handler
        // mirrors the inline assistant: if its binding wins, RunQuery never runs
        // and no result table appears, so the test fails — catching the conflict.
        workspace.update_in(cx, |workspace, _window, _cx| {
            workspace.register_action(
                |workspace, _: &zed_actions::database_panel::RunQuery, window, cx| {
                    run_current_sql_query(workspace, window, cx);
                },
            );
            workspace.register_action(|_, _: &CompetingAssistProbe, _, _| {});
        });

        let store = workspace.update_in(cx, |workspace, window, cx| {
            let store = cx.new(DatabaseStore::new);
            let focus_handle = cx.focus_handle();
            let workspace_handle = workspace.weak_handle();
            let table_filter_editor = cx.new(|cx| Editor::single_line(window, cx));
            let panel = cx.new(|cx| {
                let sub = cx.subscribe(
                    &store,
                    |_: &mut DatabasePanel,
                     _: Entity<DatabaseStore>,
                     _: &DatabaseStoreEvent,
                     cx: &mut Context<DatabasePanel>| {
                        cx.notify();
                    },
                );
                DatabasePanel {
                    focus_handle,
                    store: store.clone(),
                    workspace: workspace_handle,
                    history_expanded: false,
                    table_filter_editor,
                    collapsed_folders: HashSet::default(),
                    collapsed_connections: HashSet::default(),
                    editing_folder: None,
                    drag_target: None,
                    views_expanded: HashSet::default(),
                    procedures_expanded: HashSet::default(),
                    sequences_expanded: HashSet::default(),
                    events_expanded: HashSet::default(),
                    table_indexes_expanded: HashSet::default(),
                    table_fks_expanded: HashSet::default(),
                    table_triggers_expanded: HashSet::default(),
                    server_objects_expanded: HashSet::default(),
                    server_users: HashMap::default(),
                    table_filter_is_regex: false,
                    selected_tree_node: None,
                    selected_entity: None,
                    initial_collapse_pending: false,
                    pending_tree_state_serialization: Task::ready(None),
                    dump: DumpUiState::default(),
                    export: ExportUiState::default(),
                    context_menu: None,
                    tree_scroll_handle: ScrollHandle::new(),
                    _subscriptions: vec![sub],
                }
            });
            workspace.add_panel(panel, window, cx);
            store
        });

        // Results land as tabs in the terminal panel's pane (bottom dock), so it
        // must exist in the test workspace exactly as zed::initialize_panels adds it.
        let terminal_panel = workspace.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| TerminalPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });

        store.update(cx, |store, cx| {
            store.add_connected_for_test(config, std::sync::Arc::new(MockProvider), cx);
        });
        cx.run_until_parked();

        let connected = store.read_with(cx, |store, _| {
            store
                .connections()
                .iter()
                .any(|c| matches!(c.status, ConnectionStatus::Connected))
        });
        assert!(
            connected,
            "connection must be established before running the query"
        );

        let editor = workspace.update_in(cx, |workspace, window, cx| {
            let buffer = cx.new(|cx| language::Buffer::local("SELECT * FROM public.users", cx));
            let multi = cx.new(|cx| MultiBuffer::singleton(buffer, cx));
            let editor = cx.new(|cx| {
                let mut editor = Editor::for_multibuffer(multi, None, window, cx);
                editor.register_addon(DbQueryEditorAddon::new(connection_id));
                editor.set_show_runnables(true, cx);
                editor
            });
            workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
            editor
        });
        // Run several times in a row (refocusing the editor each time, as the
        // user does) to catch crashes from the result-pane placement logic on
        // repeated runs.
        for _ in 0..3 {
            editor.update_in(cx, |editor, window, cx| {
                let handle = editor.focus_handle(cx);
                window.focus(&handle, cx);
            });
            cx.run_until_parked();
            cx.simulate_keystrokes("ctrl-enter");
            cx.run_until_parked();
        }
        let markers = editor.read_with(cx, |editor, _| {
            editor
                .addon::<DbQueryEditorAddon>()
                .map(|addon| addon.query_markers().to_vec())
                .unwrap_or_default()
        });
        assert_eq!(
            markers,
            vec![QueryExecutionMarker {
                row: 0,
                status: QueryExecutionStatus::Success,
            }]
        );

        let pane = terminal_panel
            .read_with(cx, |panel, _| panel.pane())
            .expect("terminal panel must have a pane");
        let (result, table_context) = pane.read_with(cx, |pane, cx| {
            let view = pane
                .items_of_type::<crate::result_view::ResultView>()
                .next();
            let result = view.as_ref().and_then(|view| view.read(cx).result.clone());
            let table_context = view.and_then(|view| view.read(cx).table_context_for_test());
            (result, table_context)
        });

        let result =
            result.expect("Ctrl+Enter must execute the query and open a results table below");
        assert_eq!(
            table_context,
            Some(("public".to_string(), "users".to_string())),
            "SELECT * FROM schema.table must make the result view table-backed"
        );
        assert!(
            result
                .rows
                .iter()
                .flatten()
                .any(|cell| cell.as_deref() == Some("1")),
            "the results table must contain the query output, got rows {:?}",
            result.rows
        );
    }

    #[gpui::test]
    async fn ctrl_enter_runs_selected_statements_sequentially(cx: &mut TestAppContext) {
        let config = db_client::ConnectionConfig {
            label: "batch".to_string(),
            auto_connect: false,
            ..Default::default()
        };
        let connection_id = config.id;
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs.clone(), [], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        let store = workspace.update_in(cx, |workspace, window, cx| {
            let store = cx.new(DatabaseStore::new);
            let focus_handle = cx.focus_handle();
            let workspace_handle = workspace.weak_handle();
            let table_filter_editor = cx.new(|cx| Editor::single_line(window, cx));
            let panel = cx.new(|cx| {
                let sub = cx.subscribe(
                    &store,
                    |_: &mut DatabasePanel,
                     _: Entity<DatabaseStore>,
                     _: &DatabaseStoreEvent,
                     cx: &mut Context<DatabasePanel>| {
                        cx.notify();
                    },
                );
                DatabasePanel {
                    focus_handle,
                    store: store.clone(),
                    workspace: workspace_handle,
                    history_expanded: false,
                    table_filter_editor,
                    collapsed_folders: HashSet::default(),
                    collapsed_connections: HashSet::default(),
                    editing_folder: None,
                    drag_target: None,
                    views_expanded: HashSet::default(),
                    procedures_expanded: HashSet::default(),
                    sequences_expanded: HashSet::default(),
                    events_expanded: HashSet::default(),
                    table_indexes_expanded: HashSet::default(),
                    table_fks_expanded: HashSet::default(),
                    table_triggers_expanded: HashSet::default(),
                    server_objects_expanded: HashSet::default(),
                    server_users: HashMap::default(),
                    table_filter_is_regex: false,
                    selected_tree_node: None,
                    selected_entity: None,
                    initial_collapse_pending: false,
                    pending_tree_state_serialization: Task::ready(None),
                    dump: DumpUiState::default(),
                    export: ExportUiState::default(),
                    context_menu: None,
                    tree_scroll_handle: ScrollHandle::new(),
                    _subscriptions: vec![sub],
                }
            });
            workspace.add_panel(panel, window, cx);
            store
        });
        workspace.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| TerminalPanel::new(workspace, window, cx));
            workspace.add_panel(panel, window, cx);
        });
        store.update(cx, |store, cx| {
            store.add_connected_for_test(
                config,
                std::sync::Arc::new(RecordingMockProvider {
                    calls: calls.clone(),
                }),
                cx,
            );
        });
        cx.run_until_parked();

        let sql = "SELECT 1;\nSELECT 2;\nSELECT 3;";
        let editor = workspace.update_in(cx, |workspace, window, cx| {
            let buffer = cx.new(|cx| language::Buffer::local(sql, cx));
            let multi = cx.new(|cx| MultiBuffer::singleton(buffer, cx));
            let editor = cx.new(|cx| {
                let mut editor = Editor::for_multibuffer(multi, None, window, cx);
                editor.register_addon(DbQueryEditorAddon::new(connection_id));
                editor.set_show_runnables(true, cx);
                editor
            });
            workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
            editor
        });
        editor.update_in(cx, |editor, window, cx| {
            editor.change_selections(editor::SelectionEffects::no_scroll(), window, cx, |s| {
                s.select_ranges([
                    editor::MultiBufferOffset(0)..editor::MultiBufferOffset(sql.len())
                ]);
            });
            let handle = editor.focus_handle(cx);
            window.focus(&handle, cx);
        });
        cx.run_until_parked();

        workspace.update_in(cx, |workspace, window, cx| {
            run_current_sql_query(workspace, window, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            calls
                .lock()
                .expect("mock call log should not be poisoned")
                .as_slice(),
            &[
                "SELECT 1".to_string(),
                "SELECT 2".to_string(),
                "SELECT 3".to_string(),
            ]
        );
        let markers = editor.read_with(cx, |editor, _| {
            editor
                .addon::<DbQueryEditorAddon>()
                .map(|addon| addon.query_markers().to_vec())
                .unwrap_or_default()
        });
        assert_eq!(
            markers,
            vec![
                QueryExecutionMarker {
                    row: 0,
                    status: QueryExecutionStatus::Success,
                },
                QueryExecutionMarker {
                    row: 1,
                    status: QueryExecutionStatus::Success,
                },
                QueryExecutionMarker {
                    row: 2,
                    status: QueryExecutionStatus::Success,
                },
            ]
        );
    }

    async fn open_run_configuration_test_editor(
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) -> Entity<Editor> {
        let item = workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_abs_path(
                    std::path::PathBuf::from("/fake-scripts/seed.sql"),
                    OpenOptions::default(),
                    window,
                    cx,
                )
            })
            .await
            .expect("seed.sql must open");
        let editor = workspace
            .read_with(cx, |_, cx| item.act_as::<Editor>(cx))
            .expect("seed.sql must open as an editor");
        editor.update_in(cx, |editor, window, cx| {
            let handle = editor.focus_handle(cx);
            window.focus(&handle, cx);
        });
        editor
    }

    #[gpui::test]
    async fn save_and_run_sql_file_targets_the_saved_connection_even_after_switching_active_connection(
        cx: &mut TestAppContext,
    ) {
        let config_a = db_client::ConnectionConfig {
            label: "connection-a".to_string(),
            auto_connect: false,
            ..Default::default()
        };
        let config_b = db_client::ConnectionConfig {
            label: "connection-b".to_string(),
            auto_connect: false,
            ..Default::default()
        };
        let connection_a = config_a.id;
        let connection_b = config_b.id;
        let calls_a = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let calls_b = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

        init_test(cx);
        cx.update(|cx| editor::init(cx));
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/fake-scripts",
            serde_json::json!({ "seed.sql": "SELECT 1;" }),
        )
        .await;
        let project = Project::test(fs.clone(), ["/fake-scripts".as_ref()], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        let store = workspace.update_in(cx, |workspace, window, cx| {
            let store = cx.new(DatabaseStore::new);
            let focus_handle = cx.focus_handle();
            let workspace_handle = workspace.weak_handle();
            let table_filter_editor = cx.new(|cx| Editor::single_line(window, cx));
            let panel = cx.new(|cx| {
                let sub = cx.subscribe(
                    &store,
                    |_: &mut DatabasePanel,
                     _: Entity<DatabaseStore>,
                     _: &DatabaseStoreEvent,
                     cx: &mut Context<DatabasePanel>| {
                        cx.notify();
                    },
                );
                DatabasePanel {
                    focus_handle,
                    store: store.clone(),
                    workspace: workspace_handle,
                    history_expanded: false,
                    table_filter_editor,
                    collapsed_folders: HashSet::default(),
                    collapsed_connections: HashSet::default(),
                    editing_folder: None,
                    drag_target: None,
                    views_expanded: HashSet::default(),
                    procedures_expanded: HashSet::default(),
                    sequences_expanded: HashSet::default(),
                    events_expanded: HashSet::default(),
                    table_indexes_expanded: HashSet::default(),
                    table_fks_expanded: HashSet::default(),
                    table_triggers_expanded: HashSet::default(),
                    server_objects_expanded: HashSet::default(),
                    server_users: HashMap::default(),
                    table_filter_is_regex: false,
                    selected_tree_node: None,
                    selected_entity: None,
                    initial_collapse_pending: false,
                    pending_tree_state_serialization: Task::ready(None),
                    dump: DumpUiState::default(),
                    export: ExportUiState::default(),
                    context_menu: None,
                    tree_scroll_handle: ScrollHandle::new(),
                    _subscriptions: vec![sub],
                }
            });
            workspace.add_panel(panel, window, cx);
            store
        });

        let terminal_panel = workspace.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| TerminalPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });
        let _ = terminal_panel;
        store.update(cx, |store, cx| {
            store.add_connected_for_test(
                config_a,
                std::sync::Arc::new(RecordingMockProvider {
                    calls: calls_a.clone(),
                }),
                cx,
            );
            store.add_connected_for_test(
                config_b,
                std::sync::Arc::new(RecordingMockProvider {
                    calls: calls_b.clone(),
                }),
                cx,
            );
            store.set_active_connection(connection_a, cx);
        });
        cx.run_until_parked();

        open_run_configuration_test_editor(&workspace, cx).await;
        cx.run_until_parked();

        // Save while connection A is active, then switch the active connection
        // to B before running -- the run configuration must still target A.
        workspace.update_in(cx, |workspace, window, cx| {
            save_run_configuration(workspace, window, cx);
        });
        store.update(cx, |store, cx| {
            store.set_active_connection(connection_b, cx);
        });
        cx.run_until_parked();

        workspace.update_in(cx, |workspace, window, cx| {
            run_sql_file(workspace, window, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            calls_a
                .lock()
                .expect("mock call log should not be poisoned")
                .as_slice(),
            &["SELECT 1".to_string()],
            "the saved run configuration must run against connection A regardless of B being active"
        );
        assert!(
            calls_b
                .lock()
                .expect("mock call log should not be poisoned")
                .is_empty(),
            "connection B must never see this file's query"
        );
    }

    #[gpui::test]
    async fn run_sql_file_without_a_saved_configuration_does_not_run_anything(
        cx: &mut TestAppContext,
    ) {
        let config = db_client::ConnectionConfig {
            label: "connection-a".to_string(),
            auto_connect: false,
            ..Default::default()
        };
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

        init_test(cx);
        cx.update(|cx| editor::init(cx));
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/fake-scripts",
            serde_json::json!({ "seed.sql": "SELECT 1;" }),
        )
        .await;
        let project = Project::test(fs.clone(), ["/fake-scripts".as_ref()], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        let store = workspace.update_in(cx, |workspace, window, cx| {
            let store = cx.new(DatabaseStore::new);
            let focus_handle = cx.focus_handle();
            let workspace_handle = workspace.weak_handle();
            let table_filter_editor = cx.new(|cx| Editor::single_line(window, cx));
            let panel = cx.new(|cx| {
                let sub = cx.subscribe(
                    &store,
                    |_: &mut DatabasePanel,
                     _: Entity<DatabaseStore>,
                     _: &DatabaseStoreEvent,
                     cx: &mut Context<DatabasePanel>| {
                        cx.notify();
                    },
                );
                DatabasePanel {
                    focus_handle,
                    store: store.clone(),
                    workspace: workspace_handle,
                    history_expanded: false,
                    table_filter_editor,
                    collapsed_folders: HashSet::default(),
                    collapsed_connections: HashSet::default(),
                    editing_folder: None,
                    drag_target: None,
                    views_expanded: HashSet::default(),
                    procedures_expanded: HashSet::default(),
                    sequences_expanded: HashSet::default(),
                    events_expanded: HashSet::default(),
                    table_indexes_expanded: HashSet::default(),
                    table_fks_expanded: HashSet::default(),
                    table_triggers_expanded: HashSet::default(),
                    server_objects_expanded: HashSet::default(),
                    server_users: HashMap::default(),
                    table_filter_is_regex: false,
                    selected_tree_node: None,
                    selected_entity: None,
                    initial_collapse_pending: false,
                    pending_tree_state_serialization: Task::ready(None),
                    dump: DumpUiState::default(),
                    export: ExportUiState::default(),
                    context_menu: None,
                    tree_scroll_handle: ScrollHandle::new(),
                    _subscriptions: vec![sub],
                }
            });
            workspace.add_panel(panel, window, cx);
            store
        });

        let terminal_panel = workspace.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| TerminalPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });
        let _ = terminal_panel;
        store.update(cx, |store, cx| {
            store.add_connected_for_test(
                config,
                std::sync::Arc::new(RecordingMockProvider {
                    calls: calls.clone(),
                }),
                cx,
            );
        });
        cx.run_until_parked();

        open_run_configuration_test_editor(&workspace, cx).await;
        cx.run_until_parked();

        // No run configuration was ever saved for this file.
        workspace.update_in(cx, |workspace, window, cx| {
            run_sql_file(workspace, window, cx);
        });
        cx.run_until_parked();

        assert!(
            calls
                .lock()
                .expect("mock call log should not be poisoned")
                .is_empty(),
            "a file with no saved run configuration must not run against any connection"
        );
    }

    #[gpui::test]
    async fn run_sql_file_with_a_deleted_connection_fails_gracefully(cx: &mut TestAppContext) {
        let config = db_client::ConnectionConfig {
            label: "connection-a".to_string(),
            auto_connect: false,
            ..Default::default()
        };
        let connection_id = config.id;
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

        init_test(cx);
        cx.update(|cx| editor::init(cx));
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/fake-scripts",
            serde_json::json!({ "seed.sql": "SELECT 1;" }),
        )
        .await;
        let project = Project::test(fs.clone(), ["/fake-scripts".as_ref()], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        let store = workspace.update_in(cx, |workspace, window, cx| {
            let store = cx.new(DatabaseStore::new);
            let focus_handle = cx.focus_handle();
            let workspace_handle = workspace.weak_handle();
            let table_filter_editor = cx.new(|cx| Editor::single_line(window, cx));
            let panel = cx.new(|cx| {
                let sub = cx.subscribe(
                    &store,
                    |_: &mut DatabasePanel,
                     _: Entity<DatabaseStore>,
                     _: &DatabaseStoreEvent,
                     cx: &mut Context<DatabasePanel>| {
                        cx.notify();
                    },
                );
                DatabasePanel {
                    focus_handle,
                    store: store.clone(),
                    workspace: workspace_handle,
                    history_expanded: false,
                    table_filter_editor,
                    collapsed_folders: HashSet::default(),
                    collapsed_connections: HashSet::default(),
                    editing_folder: None,
                    drag_target: None,
                    views_expanded: HashSet::default(),
                    procedures_expanded: HashSet::default(),
                    sequences_expanded: HashSet::default(),
                    events_expanded: HashSet::default(),
                    table_indexes_expanded: HashSet::default(),
                    table_fks_expanded: HashSet::default(),
                    table_triggers_expanded: HashSet::default(),
                    server_objects_expanded: HashSet::default(),
                    server_users: HashMap::default(),
                    table_filter_is_regex: false,
                    selected_tree_node: None,
                    selected_entity: None,
                    initial_collapse_pending: false,
                    pending_tree_state_serialization: Task::ready(None),
                    dump: DumpUiState::default(),
                    export: ExportUiState::default(),
                    context_menu: None,
                    tree_scroll_handle: ScrollHandle::new(),
                    _subscriptions: vec![sub],
                }
            });
            workspace.add_panel(panel, window, cx);
            store
        });

        let terminal_panel = workspace.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| TerminalPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });
        let _ = terminal_panel;
        store.update(cx, |store, cx| {
            store.add_connected_for_test(
                config,
                std::sync::Arc::new(RecordingMockProvider {
                    calls: calls.clone(),
                }),
                cx,
            );
            store.set_active_connection(connection_id, cx);
        });
        cx.run_until_parked();

        open_run_configuration_test_editor(&workspace, cx).await;
        cx.run_until_parked();

        workspace.update_in(cx, |workspace, window, cx| {
            save_run_configuration(workspace, window, cx);
        });
        cx.run_until_parked();

        // The connection this run configuration points at is now gone.
        store.update(cx, |store, cx| {
            store.remove_connection(connection_id, cx);
        });
        cx.run_until_parked();

        workspace.update_in(cx, |workspace, window, cx| {
            run_sql_file(workspace, window, cx);
        });
        cx.run_until_parked();

        assert!(
            calls
                .lock()
                .expect("mock call log should not be poisoned")
                .is_empty(),
            "a run configuration whose connection was deleted must fail gracefully, not panic \
             or silently run against some other connection"
        );
    }

    #[gpui::test]
    async fn run_query_sends_unrecognized_sql_to_database_and_shows_error(cx: &mut TestAppContext) {
        let config = db_client::ConnectionConfig {
            label: "invalid sql".to_string(),
            database: Some("scratch".to_string()),
            auto_connect: false,
            ..Default::default()
        };
        let connection_id = config.id;
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = std::sync::Arc::new(ErroringMockProvider {
            calls: calls.clone(),
            error: "database rejected statement",
        });

        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs.clone(), [], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        let store = workspace.update_in(cx, |workspace, window, cx| {
            let store = cx.new(DatabaseStore::new);
            let focus_handle = cx.focus_handle();
            let workspace_handle = workspace.weak_handle();
            let table_filter_editor = cx.new(|cx| Editor::single_line(window, cx));
            let panel = cx.new(|cx| {
                let sub = cx.subscribe(
                    &store,
                    |_: &mut DatabasePanel,
                     _: Entity<DatabaseStore>,
                     _: &DatabaseStoreEvent,
                     cx: &mut Context<DatabasePanel>| {
                        cx.notify();
                    },
                );
                DatabasePanel {
                    focus_handle,
                    store: store.clone(),
                    workspace: workspace_handle,
                    history_expanded: false,
                    table_filter_editor,
                    collapsed_folders: HashSet::default(),
                    collapsed_connections: HashSet::default(),
                    editing_folder: None,
                    drag_target: None,
                    views_expanded: HashSet::default(),
                    procedures_expanded: HashSet::default(),
                    sequences_expanded: HashSet::default(),
                    events_expanded: HashSet::default(),
                    table_indexes_expanded: HashSet::default(),
                    table_fks_expanded: HashSet::default(),
                    table_triggers_expanded: HashSet::default(),
                    server_objects_expanded: HashSet::default(),
                    server_users: HashMap::default(),
                    table_filter_is_regex: false,
                    selected_tree_node: None,
                    selected_entity: None,
                    initial_collapse_pending: false,
                    pending_tree_state_serialization: Task::ready(None),
                    dump: DumpUiState::default(),
                    export: ExportUiState::default(),
                    context_menu: None,
                    tree_scroll_handle: ScrollHandle::new(),
                    _subscriptions: vec![sub],
                }
            });
            workspace.add_panel(panel, window, cx);
            store
        });
        let terminal_panel = workspace.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| TerminalPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });

        store.update(cx, |store, cx| {
            store.add_connected_for_test(config, provider, cx);
        });
        cx.run_until_parked();

        let editor = workspace.update_in(cx, |workspace, window, cx| {
            let buffer = cx.new(|cx| language::Buffer::local("трали вали", cx));
            let multi = cx.new(|cx| MultiBuffer::singleton(buffer, cx));
            let editor = cx.new(|cx| {
                let mut editor = Editor::for_multibuffer(multi, None, window, cx);
                editor.register_addon(DbQueryEditorAddon::new(connection_id));
                editor.set_show_runnables(true, cx);
                editor
            });
            workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
            editor
        });
        editor.update_in(cx, |editor, window, cx| {
            let handle = editor.focus_handle(cx);
            window.focus(&handle, cx);
        });
        cx.run_until_parked();

        workspace.update_in(cx, |workspace, window, cx| {
            run_current_sql_query(workspace, window, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            calls
                .lock()
                .expect("mock call log should not be poisoned")
                .as_slice(),
            &[("scratch".to_string(), "трали вали".to_string())],
            "unrecognized SQL must be sent to the database unchanged"
        );

        editor.update_in(cx, |editor, window, cx| {
            editor.set_text("SHOW CREATE TABLE instruments.splits", window, cx);
            let handle = editor.focus_handle(cx);
            window.focus(&handle, cx);
        });
        cx.run_until_parked();

        workspace.update_in(cx, |workspace, window, cx| {
            run_current_sql_query(workspace, window, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            calls
                .lock()
                .expect("mock call log should not be poisoned")
                .as_slice(),
            &[
                ("scratch".to_string(), "трали вали".to_string()),
                (
                    "scratch".to_string(),
                    "SHOW CREATE TABLE instruments.splits".to_string()
                )
            ],
            "SHOW CREATE TABLE must be sent to the database unchanged"
        );
        let markers = editor.read_with(cx, |editor, _| {
            editor
                .addon::<DbQueryEditorAddon>()
                .map(|addon| addon.query_markers().to_vec())
                .unwrap_or_default()
        });
        assert_eq!(
            markers,
            vec![QueryExecutionMarker {
                row: 0,
                status: QueryExecutionStatus::Error,
            }]
        );

        let pane = terminal_panel
            .read_with(cx, |panel, _| panel.pane())
            .expect("terminal panel must have a pane");
        let error = pane.read_with(cx, |pane, cx| {
            pane.items_of_type::<crate::result_view::ResultView>()
                .next()
                .and_then(|view| view.read(cx).error.clone())
        });
        assert_eq!(error.as_deref(), Some("database rejected statement"));
    }

    #[gpui::test]
    async fn format_query_reformats_valid_sql_and_leaves_malformed_sql_untouched(
        cx: &mut TestAppContext,
    ) {
        let config = db_client::ConnectionConfig {
            label: "format console".to_string(),
            database: Some("scratch".to_string()),
            auto_connect: false,
            ..Default::default()
        };
        let connection_id = config.id;

        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs.clone(), [], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        let store = workspace.update_in(cx, |workspace, window, cx| {
            let store = cx.new(DatabaseStore::new);
            let focus_handle = cx.focus_handle();
            let workspace_handle = workspace.weak_handle();
            let table_filter_editor = cx.new(|cx| Editor::single_line(window, cx));
            let panel = cx.new(|cx| {
                let sub = cx.subscribe(
                    &store,
                    |_: &mut DatabasePanel,
                     _: Entity<DatabaseStore>,
                     _: &DatabaseStoreEvent,
                     cx: &mut Context<DatabasePanel>| {
                        cx.notify();
                    },
                );
                DatabasePanel {
                    focus_handle,
                    store: store.clone(),
                    workspace: workspace_handle,
                    history_expanded: false,
                    table_filter_editor,
                    collapsed_folders: HashSet::default(),
                    collapsed_connections: HashSet::default(),
                    editing_folder: None,
                    drag_target: None,
                    views_expanded: HashSet::default(),
                    procedures_expanded: HashSet::default(),
                    sequences_expanded: HashSet::default(),
                    events_expanded: HashSet::default(),
                    table_indexes_expanded: HashSet::default(),
                    table_fks_expanded: HashSet::default(),
                    table_triggers_expanded: HashSet::default(),
                    server_objects_expanded: HashSet::default(),
                    server_users: HashMap::default(),
                    table_filter_is_regex: false,
                    selected_tree_node: None,
                    selected_entity: None,
                    initial_collapse_pending: false,
                    pending_tree_state_serialization: Task::ready(None),
                    dump: DumpUiState::default(),
                    export: ExportUiState::default(),
                    context_menu: None,
                    tree_scroll_handle: ScrollHandle::new(),
                    _subscriptions: vec![sub],
                }
            });
            workspace.add_panel(panel, window, cx);
            store
        });
        store.update(cx, |store, cx| {
            store.add_connected_for_test(config, std::sync::Arc::new(MockProvider), cx);
        });
        cx.run_until_parked();

        let editor = workspace.update_in(cx, |workspace, window, cx| {
            let buffer =
                cx.new(|cx| language::Buffer::local("select id, name from users where id = 1", cx));
            let multi = cx.new(|cx| MultiBuffer::singleton(buffer, cx));
            let editor = cx.new(|cx| {
                let mut editor = Editor::for_multibuffer(multi, None, window, cx);
                editor.register_addon(DbQueryEditorAddon::new(connection_id));
                editor
            });
            workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
            editor
        });
        editor.update_in(cx, |editor, window, cx| {
            let handle = editor.focus_handle(cx);
            window.focus(&handle, cx);
        });
        cx.run_until_parked();

        workspace.update_in(cx, |workspace, window, cx| {
            format_current_sql_query(workspace, window, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            editor.read_with(cx, |editor, cx| editor.text(cx)),
            "SELECT\n  id,\n  name\nFROM\n  users\nWHERE\n  id = 1;\n",
            "a real FormatQuery dispatch must reformat the console buffer in place"
        );

        editor.update_in(cx, |editor, window, cx| {
            editor.set_text("select id, from where;", window, cx);
            let handle = editor.focus_handle(cx);
            window.focus(&handle, cx);
        });
        cx.run_until_parked();

        workspace.update_in(cx, |workspace, window, cx| {
            format_current_sql_query(workspace, window, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            editor.read_with(cx, |editor, cx| editor.text(cx)),
            "select id, from where;",
            "malformed SQL being edited must be left byte-for-byte untouched, never partially \
             formatted or truncated"
        );
    }

    // Guards the bottom-dock requirement: each connection gets exactly one
    // results tab, reused across runs; a different connection gets its own tab.
    #[gpui::test]
    async fn results_panel_keeps_one_tab_per_connection(cx: &mut TestAppContext) {
        use std::sync::Arc;
        use std::sync::atomic::AtomicUsize;

        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs.clone(), [], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        // A plain pane stands in for the terminal panel's pane: the placement
        // helper only needs somewhere to add and find ResultView items.
        let pane = workspace.update_in(cx, |workspace, window, cx| {
            let project = workspace.project().clone();
            let handle = workspace.weak_handle();
            cx.new(|cx| {
                Pane::new(
                    handle,
                    project,
                    Arc::new(AtomicUsize::new(0)),
                    None,
                    Box::new(zed_actions::database_panel::NewQuery),
                    false,
                    window,
                    cx,
                )
            })
        });

        let conn_a = uuid::Uuid::new_v4();
        let conn_b = uuid::Uuid::new_v4();

        let view_a1 = workspace.update_in(cx, |_, window, cx| {
            show_result_in_pane(&pane, conn_a, "A — Results".into(), None, window, cx)
        });
        let view_a2 = workspace.update_in(cx, |_, window, cx| {
            show_result_in_pane(&pane, conn_a, "A — Results".into(), None, window, cx)
        });
        let view_b = workspace.update_in(cx, |_, window, cx| {
            show_result_in_pane(&pane, conn_b, "B — Results".into(), None, window, cx)
        });

        assert_eq!(
            view_a1.entity_id(),
            view_a2.entity_id(),
            "re-running the same connection must reuse its tab"
        );
        assert_ne!(
            view_a1.entity_id(),
            view_b.entity_id(),
            "a different connection must get its own tab"
        );

        let tab_count = pane.read_with(cx, |pane, _| {
            pane.items_of_type::<crate::result_view::ResultView>()
                .count()
        });
        assert_eq!(
            tab_count, 2,
            "two connections must produce exactly two result tabs"
        );
    }

    #[gpui::test]
    async fn quick_documentation_action_uses_selected_tree_node(cx: &mut TestAppContext) {
        let config = db_client::ConnectionConfig {
            label: "doc".to_string(),
            auto_connect: false,
            ..Default::default()
        };
        let connection_id = config.id;

        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs.clone(), [], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        let (store, panel) = workspace.update_in(cx, |workspace, window, cx| {
            let store = cx.new(DatabaseStore::new);
            let focus_handle = cx.focus_handle();
            let workspace_handle = workspace.weak_handle();
            let table_filter_editor = cx.new(|cx| Editor::single_line(window, cx));
            let panel = cx.new(|_| DatabasePanel {
                focus_handle,
                store: store.clone(),
                workspace: workspace_handle,
                history_expanded: false,
                table_filter_editor,
                collapsed_folders: HashSet::default(),
                collapsed_connections: HashSet::default(),
                editing_folder: None,
                drag_target: None,
                views_expanded: HashSet::default(),
                procedures_expanded: HashSet::default(),
                sequences_expanded: HashSet::default(),
                events_expanded: HashSet::default(),
                table_indexes_expanded: HashSet::default(),
                table_fks_expanded: HashSet::default(),
                table_triggers_expanded: HashSet::default(),
                server_objects_expanded: HashSet::default(),
                server_users: HashMap::default(),
                table_filter_is_regex: false,
                selected_tree_node: None,
                selected_entity: None,
                initial_collapse_pending: false,
                pending_tree_state_serialization: Task::ready(None),
                dump: DumpUiState::default(),
                export: ExportUiState::default(),
                context_menu: None,
                tree_scroll_handle: ScrollHandle::new(),
                _subscriptions: Vec::new(),
            });
            workspace.add_panel(panel.clone(), window, cx);
            (store, panel)
        });

        store.update(cx, |store, cx| {
            store.add_connected_for_test(config, std::sync::Arc::new(MockProvider), cx);
        });
        cx.run_until_parked();

        panel.update_in(cx, |panel, window, cx| {
            panel.selected_tree_node = Some(SelectedTreeNode {
                connection_id,
                database: "public".to_string(),
                table: Some("users".to_string()),
            });
            panel.quick_doc_for_selection(window, cx);
        });
        cx.run_until_parked();

        let has_modal = workspace.read_with(cx, |workspace, cx| {
            workspace.active_modal::<QuickDocView>(cx).is_some()
        });
        assert!(
            has_modal,
            "Quick Documentation must open as a workspace modal for the selected tree node"
        );
    }

    #[gpui::test]
    async fn show_diagram_opens_erd_as_workspace_tab(cx: &mut TestAppContext) {
        let config = db_client::ConnectionConfig {
            label: "erd".to_string(),
            auto_connect: false,
            ..Default::default()
        };
        let connection_id = config.id;

        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs.clone(), [], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        let (store, panel) = workspace.update_in(cx, |workspace, window, cx| {
            let store = cx.new(DatabaseStore::new);
            let focus_handle = cx.focus_handle();
            let workspace_handle = workspace.weak_handle();
            let table_filter_editor = cx.new(|cx| Editor::single_line(window, cx));
            let panel = cx.new(|_| DatabasePanel {
                focus_handle,
                store: store.clone(),
                workspace: workspace_handle,
                history_expanded: false,
                table_filter_editor,
                collapsed_folders: HashSet::default(),
                collapsed_connections: HashSet::default(),
                editing_folder: None,
                drag_target: None,
                views_expanded: HashSet::default(),
                procedures_expanded: HashSet::default(),
                sequences_expanded: HashSet::default(),
                events_expanded: HashSet::default(),
                table_indexes_expanded: HashSet::default(),
                table_fks_expanded: HashSet::default(),
                table_triggers_expanded: HashSet::default(),
                server_objects_expanded: HashSet::default(),
                server_users: HashMap::default(),
                table_filter_is_regex: false,
                selected_tree_node: None,
                selected_entity: None,
                initial_collapse_pending: false,
                pending_tree_state_serialization: Task::ready(None),
                dump: DumpUiState::default(),
                export: ExportUiState::default(),
                context_menu: None,
                tree_scroll_handle: ScrollHandle::new(),
                _subscriptions: Vec::new(),
            });
            workspace.add_panel(panel.clone(), window, cx);
            (store, panel)
        });

        store.update(cx, |store, cx| {
            store.add_connected_for_test(config, std::sync::Arc::new(MockProvider), cx);
        });
        cx.run_until_parked();

        panel.update_in(cx, |panel, window, cx| {
            panel.open_erd_diagram(connection_id, "public".to_string(), window, cx);
        });
        cx.run_until_parked();

        let erd_tabs = workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_pane()
                .read(cx)
                .items_of_type::<ErdView>()
                .count()
        });
        assert_eq!(
            erd_tabs, 1,
            "Show Diagram must open the ERD as a tab in the active pane"
        );
    }

    #[gpui::test]
    async fn compare_data_opens_as_workspace_tab(cx: &mut TestAppContext) {
        let config = db_client::ConnectionConfig {
            label: "compare".to_string(),
            auto_connect: false,
            ..Default::default()
        };
        let connection_id = config.id;
        let (panel, mut cx) = load_connected_panel(cx, config).await;
        let workspace = panel
            .read_with(&cx, |panel, _| panel.workspace.upgrade())
            .expect("workspace handle must be live");

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.start_compare(
                connection_id,
                "public".to_string(),
                "left".to_string(),
                "right".to_string(),
                window,
                cx,
            );
        });
        cx.run_until_parked();

        let compare_tabs = workspace.read_with(&cx, |workspace, cx| {
            workspace
                .active_pane()
                .read(cx)
                .items_of_type::<CompareDataView>()
                .count()
        });
        assert_eq!(
            compare_tabs, 1,
            "Compare Data must open as a tab in the active pane"
        );
    }

    // Returns a different column list per table name so a real structural
    // diff exists between "left" (missing "email", has a nullable "legacy")
    // and "right" (has "email" as NOT NULL, no "legacy").
    struct SchemaCompareProvider;

    #[async_trait::async_trait]
    impl db_client::DbProvider for SchemaCompareProvider {
        async fn ping(&self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn list_databases(&self) -> anyhow::Result<Vec<db_client::DatabaseInfo>> {
            Ok(Vec::new())
        }
        async fn list_tables(&self, _database: &str) -> anyhow::Result<Vec<db_client::TableInfo>> {
            Ok(Vec::new())
        }
        async fn describe_table(
            &self,
            _database: &str,
            table: &str,
        ) -> anyhow::Result<Vec<db_client::ColumnInfo>> {
            let id = db_client::ColumnInfo {
                name: "id".to_string(),
                data_type: "int".to_string(),
                is_nullable: false,
                column_key: Some("PRI".to_string()),
                default_value: None,
                extra: String::new(),
            };
            Ok(if table == "right" {
                vec![
                    id,
                    db_client::ColumnInfo {
                        name: "email".to_string(),
                        data_type: "varchar(255)".to_string(),
                        is_nullable: false,
                        column_key: None,
                        default_value: None,
                        extra: String::new(),
                    },
                ]
            } else {
                vec![
                    id,
                    db_client::ColumnInfo {
                        name: "legacy".to_string(),
                        data_type: "tinyint".to_string(),
                        is_nullable: true,
                        column_key: None,
                        default_value: None,
                        extra: String::new(),
                    },
                ]
            })
        }
        async fn execute_query(
            &self,
            _database: &str,
            _sql: &str,
        ) -> anyhow::Result<db_client::schema::QueryResult> {
            unreachable!("this fake only exercises schema introspection")
        }
        async fn get_table_ddl(&self, _database: &str, _table: &str) -> anyhow::Result<String> {
            Ok(String::new())
        }
    }

    #[gpui::test]
    async fn schema_compare_opens_a_diff_view_with_the_migration_script(cx: &mut TestAppContext) {
        let config = db_client::ConnectionConfig {
            label: "schema-compare".to_string(),
            auto_connect: false,
            driver: db_client::DatabaseDriver::PostgreSQL,
            ..Default::default()
        };
        let connection_id = config.id;
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs.clone(), [], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        let panel = workspace
            .update_in(&mut cx, |_, window, cx| {
                cx.spawn_in(
                    window,
                    async move |workspace_handle, cx: &mut AsyncWindowContext| {
                        DatabasePanel::load(workspace_handle, cx.clone()).await
                    },
                )
            })
            .await
            .expect("DatabasePanel::load must succeed");
        workspace.update_in(&mut cx, |workspace, window, cx| {
            workspace.add_panel(panel.clone(), window, cx);
        });
        panel.update(&mut cx, |panel, cx| {
            panel.store.update(cx, |store, cx| {
                store.add_connected_for_test(
                    config,
                    std::sync::Arc::new(SchemaCompareProvider),
                    cx,
                );
            });
        });
        cx.run_until_parked();

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.start_schema_compare(
                connection_id,
                "public".to_string(),
                "left".to_string(),
                "right".to_string(),
                window,
                cx,
            );
        });
        cx.run_until_parked();

        let script = workspace.read_with(&cx, |workspace, cx| {
            workspace
                .active_pane()
                .read(cx)
                .items_of_type::<crate::schema_diff::SchemaDiffView>()
                .next()
                .map(|view| view.read(cx).script_text())
        });
        let script = script.expect("Compare Schema must open a SchemaDiffView tab");
        assert!(
            script.contains("ADD COLUMN \"email\""),
            "the script must add the column only present on the right side: {script}"
        );
        assert!(
            script.contains("DROP COLUMN \"legacy\""),
            "the script must drop the column only present on the left side: {script}"
        );
    }

    #[gpui::test]
    async fn dump_dialog_opens_and_task_dismisses(cx: &mut TestAppContext) {
        let config = db_client::ConnectionConfig {
            label: "qa".to_string(),
            driver: db_client::DatabaseDriver::MySQL,
            auto_connect: false,
            ..Default::default()
        };
        let connection_id = config.id;

        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs.clone(), [], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        let (store, panel) = workspace.update_in(cx, |workspace, window, cx| {
            let store = cx.new(DatabaseStore::new);
            let focus_handle = cx.focus_handle();
            let workspace_handle = workspace.weak_handle();
            let table_filter_editor = cx.new(|cx| Editor::single_line(window, cx));
            let panel = cx.new(|_| DatabasePanel {
                focus_handle,
                store: store.clone(),
                workspace: workspace_handle,
                history_expanded: false,
                table_filter_editor,
                collapsed_folders: HashSet::default(),
                collapsed_connections: HashSet::default(),
                editing_folder: None,
                drag_target: None,
                views_expanded: HashSet::default(),
                procedures_expanded: HashSet::default(),
                sequences_expanded: HashSet::default(),
                events_expanded: HashSet::default(),
                table_indexes_expanded: HashSet::default(),
                table_fks_expanded: HashSet::default(),
                table_triggers_expanded: HashSet::default(),
                server_objects_expanded: HashSet::default(),
                server_users: HashMap::default(),
                table_filter_is_regex: false,
                selected_tree_node: None,
                selected_entity: None,
                initial_collapse_pending: false,
                pending_tree_state_serialization: Task::ready(None),
                dump: DumpUiState::default(),
                export: ExportUiState::default(),
                context_menu: None,
                tree_scroll_handle: ScrollHandle::new(),
                _subscriptions: Vec::new(),
            });
            workspace.add_panel(panel.clone(), window, cx);
            (store, panel)
        });

        store.update(cx, |store, cx| {
            store.add_connected_for_test(config, std::sync::Arc::new(MockProvider), cx);
        });
        cx.run_until_parked();

        panel.update_in(cx, |panel, window, cx| {
            panel.open_dump_dialog(connection_id, Vec::new(), Vec::new(), window, cx);
        });
        assert!(
            workspace.update(cx, |workspace, cx| workspace
                .active_modal::<NativeDumpDialog>(cx)
                .is_some()),
            "Export entry must open the native dump dialog as a workspace modal"
        );

        panel.update(cx, |panel, _cx| {
            panel.dump.tasks.push(DumpTask {
                id: 42,
                label: "qa".into(),
                status: DumpStatus::Done {
                    output_path: "/tmp/qa-dump.sql".to_string(),
                },
            });
        });
        assert_eq!(panel.read_with(cx, |panel, _| panel.dump.tasks.len()), 1);

        panel.update(cx, |panel, cx| panel.dismiss_dump_task(42, cx));
        assert!(
            panel.read_with(cx, |panel, _| panel.dump.tasks.is_empty()),
            "Dismiss must remove a finished dump task from the status strip"
        );
    }

    #[gpui::test]
    async fn new_folder_creates_and_renames_then_collapses(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs.clone(), [], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        let (store, panel) = workspace.update_in(cx, |workspace, window, cx| {
            let store = cx.new(DatabaseStore::new);
            let focus_handle = cx.focus_handle();
            let workspace_handle = workspace.weak_handle();
            let table_filter_editor = cx.new(|cx| Editor::single_line(window, cx));
            let panel = cx.new(|_| DatabasePanel {
                focus_handle,
                store: store.clone(),
                workspace: workspace_handle,
                history_expanded: false,
                table_filter_editor,
                collapsed_folders: HashSet::default(),
                collapsed_connections: HashSet::default(),
                editing_folder: None,
                drag_target: None,
                views_expanded: HashSet::default(),
                procedures_expanded: HashSet::default(),
                sequences_expanded: HashSet::default(),
                events_expanded: HashSet::default(),
                table_indexes_expanded: HashSet::default(),
                table_fks_expanded: HashSet::default(),
                table_triggers_expanded: HashSet::default(),
                server_objects_expanded: HashSet::default(),
                server_users: HashMap::default(),
                table_filter_is_regex: false,
                selected_tree_node: None,
                selected_entity: None,
                initial_collapse_pending: false,
                pending_tree_state_serialization: Task::ready(None),
                dump: DumpUiState::default(),
                export: ExportUiState::default(),
                context_menu: None,
                tree_scroll_handle: ScrollHandle::new(),
                _subscriptions: Vec::new(),
            });
            workspace.add_panel(panel.clone(), window, cx);
            (store, panel)
        });

        // New Folder creates a folder and opens its inline editor.
        panel.update_in(cx, |panel, window, cx| {
            panel.start_new_folder(None, window, cx);
        });
        let folder_id = store.read_with(cx, |store, _| {
            assert_eq!(store.folders().len(), 1, "a folder must be created");
            store.folders()[0].id
        });
        panel.read_with(cx, |panel, _| {
            assert!(
                panel.editing_folder.is_some(),
                "the new folder opens its rename editor"
            );
        });

        // Typing a name and committing renames the folder.
        panel.update_in(cx, |panel, window, cx| {
            if let Some(editing) = panel.editing_folder.as_ref() {
                editing
                    .editor
                    .update(cx, |editor, cx| editor.set_text("Production", window, cx));
            }
            panel.commit_folder_rename(cx);
        });
        store.read_with(cx, |store, _| {
            assert_eq!(store.folders()[0].name, "Production");
        });
        panel.read_with(cx, |panel, _| {
            assert!(panel.editing_folder.is_none(), "rename closes the editor");
        });

        // Collapsing and expanding toggles the folder's collapsed state.
        panel.update(cx, |panel, cx| panel.toggle_folder_collapsed(folder_id, cx));
        panel.read_with(cx, |panel, _| {
            assert!(panel.collapsed_folders.contains(&folder_id));
        });
        panel.update(cx, |panel, cx| panel.toggle_folder_collapsed(folder_id, cx));
        panel.read_with(cx, |panel, _| {
            assert!(!panel.collapsed_folders.contains(&folder_id));
        });

        // Deleting an empty folder removes it from the store.
        panel.update(cx, |panel, cx| panel.delete_folder(folder_id, cx));
        store.read_with(cx, |store, _| {
            assert!(
                store.folders().is_empty(),
                "an empty folder must be deletable"
            );
        });
    }

    #[gpui::test]
    async fn expand_all_collapse_all_and_connection_chevron_toggle(cx: &mut TestAppContext) {
        let config = db_client::ConnectionConfig {
            label: "tree".to_string(),
            auto_connect: false,
            ..Default::default()
        };
        let connection_id = config.id;

        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs.clone(), [], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        let (store, panel) = workspace.update_in(cx, |workspace, window, cx| {
            let store = cx.new(DatabaseStore::new);
            let focus_handle = cx.focus_handle();
            let workspace_handle = workspace.weak_handle();
            let table_filter_editor = cx.new(|cx| Editor::single_line(window, cx));
            let panel = cx.new(|_| DatabasePanel {
                focus_handle,
                store: store.clone(),
                workspace: workspace_handle,
                history_expanded: false,
                table_filter_editor,
                collapsed_folders: HashSet::default(),
                collapsed_connections: HashSet::default(),
                editing_folder: None,
                drag_target: None,
                views_expanded: HashSet::default(),
                procedures_expanded: HashSet::default(),
                sequences_expanded: HashSet::default(),
                events_expanded: HashSet::default(),
                table_indexes_expanded: HashSet::default(),
                table_fks_expanded: HashSet::default(),
                table_triggers_expanded: HashSet::default(),
                server_objects_expanded: HashSet::default(),
                server_users: HashMap::default(),
                table_filter_is_regex: false,
                selected_tree_node: None,
                selected_entity: None,
                initial_collapse_pending: false,
                pending_tree_state_serialization: Task::ready(None),
                dump: DumpUiState::default(),
                export: ExportUiState::default(),
                context_menu: None,
                tree_scroll_handle: ScrollHandle::new(),
                _subscriptions: Vec::new(),
            });
            workspace.add_panel(panel.clone(), window, cx);
            (store, panel)
        });

        store.update(cx, |store, cx| {
            store.add_connected_for_test(config, std::sync::Arc::new(MockProvider), cx);
        });
        cx.run_until_parked();

        let folder_id = store
            .update(cx, |store, cx| store.add_folder("Prod".into(), None, cx))
            .expect("folder must be created");

        // Collapse All folds every folder and connection and clears schema expansion.
        panel.update(cx, |panel, cx| panel.collapse_all(cx));
        panel.read_with(cx, |panel, _| {
            assert!(
                panel.collapsed_folders.contains(&folder_id),
                "collapse all folds folders"
            );
            assert!(
                panel.collapsed_connections.contains(&connection_id),
                "collapse all folds connections"
            );
        });
        store.read_with(cx, |store, _| {
            assert!(
                store
                    .connections()
                    .iter()
                    .all(|c| c.expanded_database_set.is_empty()),
                "collapse all clears schema expansion"
            );
        });

        // Expand All unfolds folders and connections.
        panel.update(cx, |panel, cx| panel.expand_all(cx));
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(
                panel.collapsed_folders.is_empty(),
                "expand all unfolds folders"
            );
            assert!(
                panel.collapsed_connections.is_empty(),
                "expand all unfolds connections"
            );
        });

        // The connection disclosure chevron folds a single connection in place.
        panel.update(cx, |panel, cx| {
            panel.toggle_connection_collapsed(connection_id, cx)
        });
        panel.read_with(cx, |panel, _| {
            assert!(panel.collapsed_connections.contains(&connection_id));
        });
        panel.update(cx, |panel, cx| {
            panel.toggle_connection_collapsed(connection_id, cx)
        });
        panel.read_with(cx, |panel, _| {
            assert!(!panel.collapsed_connections.contains(&connection_id));
        });
    }

    // Uses the real `DatabasePanel::load()` entry point (not a hand-built
    // struct literal) specifically because that's where the kvp read/restore
    // and the "nothing persisted yet -> collapse everything" fallback live.
    #[gpui::test]
    async fn tree_state_defaults_to_fully_collapsed_then_persists_across_a_simulated_restart(
        cx: &mut TestAppContext,
    ) {
        let config = db_client::ConnectionConfig {
            label: "persist-tree".to_string(),
            auto_connect: false,
            ..Default::default()
        };
        let connection_id = config.id;
        let folder = Folder::new("Prod".to_string(), None, 0);
        let folder_id = folder.id;

        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs.clone(), [], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let mut cx = VisualTestContext::from_window(window.into(), cx);

        let panel = workspace
            .update_in(&mut cx, |_, window, cx| {
                cx.spawn_in(
                    window,
                    async move |workspace_handle, cx: &mut AsyncWindowContext| {
                        DatabasePanel::load(workspace_handle, cx.clone()).await
                    },
                )
            })
            .await
            .expect("DatabasePanel::load must succeed");
        workspace.update_in(&mut cx, |workspace, window, cx| {
            workspace.add_panel(panel.clone(), window, cx);
        });
        panel.update(&mut cx, |panel, cx| {
            panel.store.update(cx, |store, cx| {
                store.add_connected_for_test(config.clone(), std::sync::Arc::new(MockProvider), cx);
                store.folders.push(folder.clone());
                cx.emit(DatabaseStoreEvent::ConnectionsChanged);
            });
        });
        cx.run_until_parked();

        // Nothing was ever persisted for this brand-new workspace, so the
        // panel must fall back to collapsing everything it just learned
        // about rather than leaving the empty-set default of "expanded".
        panel.read_with(&cx, |panel, _| {
            assert!(
                panel.collapsed_folders.contains(&folder_id),
                "first launch with nothing persisted must collapse every folder by default"
            );
            assert!(
                panel.collapsed_connections.contains(&connection_id),
                "first launch with nothing persisted must collapse every connection by default"
            );
        });

        // Expand both; each toggle also persists the new state in the
        // background (`serialize_tree_state`).
        panel.update(&mut cx, |panel, cx| {
            panel.toggle_folder_collapsed(folder_id, cx);
            panel.toggle_connection_collapsed(connection_id, cx);
        });
        cx.run_until_parked();

        // A fresh `DatabasePanel::load()` against the very same workspace
        // simulates a restart: it must restore the expanded state instead of
        // re-collapsing everything, because a real persisted state now exists.
        let restarted_panel = workspace
            .update_in(&mut cx, |_, window, cx| {
                cx.spawn_in(
                    window,
                    async move |workspace_handle, cx: &mut AsyncWindowContext| {
                        DatabasePanel::load(workspace_handle, cx.clone()).await
                    },
                )
            })
            .await
            .expect("DatabasePanel::load must succeed");
        restarted_panel.update(&mut cx, |panel, cx| {
            panel.store.update(cx, |store, cx| {
                store.add_connected_for_test(config, std::sync::Arc::new(MockProvider), cx);
                store.folders.push(folder);
                cx.emit(DatabaseStoreEvent::ConnectionsChanged);
            });
        });
        cx.run_until_parked();

        restarted_panel.read_with(&cx, |panel, _| {
            assert!(
                !panel.collapsed_folders.contains(&folder_id),
                "a restart must restore the persisted expanded folder, not re-collapse it"
            );
            assert!(
                !panel.collapsed_connections.contains(&connection_id),
                "a restart must restore the persisted expanded connection, not re-collapse it"
            );
        });
    }

    // `handle_drop` had zero test coverage of any kind -- not even a direct
    // function call -- despite being the whole mechanism behind drag-and-drop
    // reparenting in the tree. Per this session's own lesson (a test calling
    // a handler directly can pass while the real event-driven path is
    // broken), this drives the real GPUI drag gesture: mouse-down on the
    // connection row, mouse-move onto the folder row (which is what actually
    // arms `on_drag`/`on_drag_move` and flips `drag_target`), then mouse-up
    // to fire `on_drop`.
    #[gpui::test]
    async fn dragging_a_connection_row_onto_a_folder_row_reparents_it(cx: &mut TestAppContext) {
        let config = db_client::ConnectionConfig {
            label: "tree".to_string(),
            auto_connect: false,
            ..Default::default()
        };
        let connection_id = config.id;

        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs.clone(), [], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        let (store, _panel) = workspace.update_in(cx, |workspace, window, cx| {
            let store = cx.new(DatabaseStore::new);
            let focus_handle = cx.focus_handle();
            let workspace_handle = workspace.weak_handle();
            let table_filter_editor = cx.new(|cx| Editor::single_line(window, cx));
            let panel = cx.new(|_| DatabasePanel {
                focus_handle,
                store: store.clone(),
                workspace: workspace_handle,
                history_expanded: false,
                table_filter_editor,
                collapsed_folders: HashSet::default(),
                collapsed_connections: HashSet::default(),
                editing_folder: None,
                drag_target: None,
                views_expanded: HashSet::default(),
                procedures_expanded: HashSet::default(),
                sequences_expanded: HashSet::default(),
                events_expanded: HashSet::default(),
                table_indexes_expanded: HashSet::default(),
                table_fks_expanded: HashSet::default(),
                table_triggers_expanded: HashSet::default(),
                server_objects_expanded: HashSet::default(),
                server_users: HashMap::default(),
                table_filter_is_regex: false,
                selected_tree_node: None,
                selected_entity: None,
                initial_collapse_pending: false,
                pending_tree_state_serialization: Task::ready(None),
                dump: DumpUiState::default(),
                export: ExportUiState::default(),
                context_menu: None,
                tree_scroll_handle: ScrollHandle::new(),
                _subscriptions: Vec::new(),
            });
            workspace.add_panel(panel.clone(), window, cx);
            (store, panel)
        });

        store.update(cx, |store, cx| {
            store.add_connected_for_test(config, std::sync::Arc::new(MockProvider), cx);
        });
        let folder_id = store
            .update(cx, |store, cx| store.add_folder("Prod".into(), None, cx))
            .expect("folder must be created");
        cx.run_until_parked();

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.toggle_panel_focus::<DatabasePanel>(window, cx);
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        cx.run_until_parked();

        let connection_source = debug_center(cx, format!("conn-header-{connection_id}").leak());
        let folder_target = debug_center(cx, format!("folder-row-{folder_id}").leak());

        store.read_with(cx, |store, _| {
            assert_eq!(
                store
                    .connections()
                    .iter()
                    .find(|c| c.config.id == connection_id)
                    .and_then(|c| c.config.folder_id),
                None,
                "the connection must start outside any folder"
            );
        });

        cx.simulate_mouse_down(
            connection_source,
            MouseButton::Left,
            gpui::Modifiers::none(),
        );
        cx.simulate_mouse_move(
            folder_target,
            Some(MouseButton::Left),
            gpui::Modifiers::none(),
        );
        cx.simulate_mouse_move(
            folder_target,
            Some(MouseButton::Left),
            gpui::Modifiers::none(),
        );
        cx.simulate_mouse_up(folder_target, MouseButton::Left, gpui::Modifiers::none());
        cx.run_until_parked();

        store.read_with(cx, |store, _| {
            assert_eq!(
                store
                    .connections()
                    .iter()
                    .find(|c| c.config.id == connection_id)
                    .and_then(|c| c.config.folder_id),
                Some(folder_id),
                "dropping the connection row onto the folder row must reparent it via the real \
                 on_drag/on_drop path, not merely by calling handle_drop directly"
            );
        });
    }

    // Folders had no way to change their sibling order at all before this
    // test's fix -- only connections could be reordered via the selection
    // action bar's Move Up/Down buttons. Drives the real right-click gesture
    // (mouse-down, mouse-up to open the popover, then a click on the rendered
    // "Move Down" entry) rather than calling `reorder_folder` directly, per
    // this session's own lesson that a direct call can pass while the real
    // event-driven path is broken.
    #[gpui::test]
    async fn folder_context_menu_move_down_reorders_siblings(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs.clone(), [], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        let (store, _panel) = workspace.update_in(cx, |workspace, window, cx| {
            let store = cx.new(DatabaseStore::new);
            let focus_handle = cx.focus_handle();
            let workspace_handle = workspace.weak_handle();
            let table_filter_editor = cx.new(|cx| Editor::single_line(window, cx));
            let panel = cx.new(|_| DatabasePanel {
                focus_handle,
                store: store.clone(),
                workspace: workspace_handle,
                history_expanded: false,
                table_filter_editor,
                collapsed_folders: HashSet::default(),
                collapsed_connections: HashSet::default(),
                editing_folder: None,
                drag_target: None,
                views_expanded: HashSet::default(),
                procedures_expanded: HashSet::default(),
                sequences_expanded: HashSet::default(),
                events_expanded: HashSet::default(),
                table_indexes_expanded: HashSet::default(),
                table_fks_expanded: HashSet::default(),
                table_triggers_expanded: HashSet::default(),
                server_objects_expanded: HashSet::default(),
                server_users: HashMap::default(),
                table_filter_is_regex: false,
                selected_tree_node: None,
                selected_entity: None,
                initial_collapse_pending: false,
                pending_tree_state_serialization: Task::ready(None),
                dump: DumpUiState::default(),
                export: ExportUiState::default(),
                context_menu: None,
                tree_scroll_handle: ScrollHandle::new(),
                _subscriptions: Vec::new(),
            });
            workspace.add_panel(panel.clone(), window, cx);
            (store, panel)
        });

        let folder_a = store
            .update(cx, |store, cx| store.add_folder("A".into(), None, cx))
            .expect("folder a must be created");
        let folder_b = store
            .update(cx, |store, cx| store.add_folder("B".into(), None, cx))
            .expect("folder b must be created");
        cx.run_until_parked();

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.toggle_panel_focus::<DatabasePanel>(window, cx);
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        cx.run_until_parked();

        store.read_with(cx, |store, _| {
            let mut folders = store.folders().to_vec();
            folders.sort_by_key(|f| f.order);
            assert_eq!(
                folders.iter().map(|f| f.id).collect::<Vec<_>>(),
                vec![folder_a, folder_b],
                "A must sort before B before any reordering"
            );
        });

        let folder_a_row = debug_center(cx, format!("folder-row-{folder_a}").leak());
        cx.simulate_mouse_move(folder_a_row, None, gpui::Modifiers::none());
        cx.simulate_mouse_down(folder_a_row, MouseButton::Right, gpui::Modifiers::none());
        cx.simulate_mouse_up(folder_a_row, MouseButton::Right, gpui::Modifiers::none());
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        cx.run_until_parked();

        let move_down_item = debug_center(cx, "MENU_ITEM-Move Down".to_string().leak());
        cx.simulate_click(move_down_item, gpui::Modifiers::none());
        cx.run_until_parked();

        store.read_with(cx, |store, _| {
            let mut folders = store.folders().to_vec();
            folders.sort_by_key(|f| f.order);
            assert_eq!(
                folders.iter().map(|f| f.id).collect::<Vec<_>>(),
                vec![folder_b, folder_a],
                "clicking Move Down on A's real context menu must swap it below B"
            );
        });
    }

    // Drives a real keystroke through the production keymap binding
    // ("DatabasePanel" context), not a direct `move_selected` call, mirroring
    // `folder_context_menu_move_down_reorders_siblings`'s philosophy of
    // exercising the actual interaction path rather than an internal method.
    #[gpui::test]
    async fn shift_down_and_shift_up_reorder_the_selected_folder(cx: &mut TestAppContext) {
        use zed_actions::database_panel::{MoveSelectedDown, MoveSelectedUp};

        init_test(cx);
        cx.update(|cx| {
            cx.bind_keys([
                gpui::KeyBinding::new("shift-down", MoveSelectedDown, Some("DatabasePanel")),
                gpui::KeyBinding::new("shift-up", MoveSelectedUp, Some("DatabasePanel")),
            ]);
        });
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs.clone(), [], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        let (store, _panel) = workspace.update_in(cx, |workspace, window, cx| {
            let store = cx.new(DatabaseStore::new);
            let focus_handle = cx.focus_handle();
            let workspace_handle = workspace.weak_handle();
            let table_filter_editor = cx.new(|cx| Editor::single_line(window, cx));
            let panel = cx.new(|_| DatabasePanel {
                focus_handle,
                store: store.clone(),
                workspace: workspace_handle,
                history_expanded: false,
                table_filter_editor,
                collapsed_folders: HashSet::default(),
                collapsed_connections: HashSet::default(),
                editing_folder: None,
                drag_target: None,
                views_expanded: HashSet::default(),
                procedures_expanded: HashSet::default(),
                sequences_expanded: HashSet::default(),
                events_expanded: HashSet::default(),
                table_indexes_expanded: HashSet::default(),
                table_fks_expanded: HashSet::default(),
                table_triggers_expanded: HashSet::default(),
                server_objects_expanded: HashSet::default(),
                server_users: HashMap::default(),
                table_filter_is_regex: false,
                selected_tree_node: None,
                selected_entity: None,
                initial_collapse_pending: false,
                pending_tree_state_serialization: Task::ready(None),
                dump: DumpUiState::default(),
                export: ExportUiState::default(),
                context_menu: None,
                tree_scroll_handle: ScrollHandle::new(),
                _subscriptions: Vec::new(),
            });
            workspace.add_panel(panel.clone(), window, cx);
            (store, panel)
        });

        let folder_a = store
            .update(cx, |store, cx| store.add_folder("A".into(), None, cx))
            .expect("folder a must be created");
        let folder_b = store
            .update(cx, |store, cx| store.add_folder("B".into(), None, cx))
            .expect("folder b must be created");
        cx.run_until_parked();

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.toggle_panel_focus::<DatabasePanel>(window, cx);
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        cx.run_until_parked();

        // A real click selects folder A, same as a user would before reordering
        // it with the keyboard.
        let folder_a_row = debug_center(cx, format!("folder-row-{folder_a}").leak());
        cx.simulate_click(folder_a_row, gpui::Modifiers::none());
        cx.run_until_parked();

        cx.simulate_keystrokes("shift-down");
        cx.run_until_parked();

        store.read_with(cx, |store, _| {
            let mut folders = store.folders().to_vec();
            folders.sort_by_key(|f| f.order);
            assert_eq!(
                folders.iter().map(|f| f.id).collect::<Vec<_>>(),
                vec![folder_b, folder_a],
                "Shift+Down on selected folder A must swap it below B"
            );
        });

        cx.simulate_keystrokes("shift-up");
        cx.run_until_parked();

        store.read_with(cx, |store, _| {
            let mut folders = store.folders().to_vec();
            folders.sort_by_key(|f| f.order);
            assert_eq!(
                folders.iter().map(|f| f.id).collect::<Vec<_>>(),
                vec![folder_a, folder_b],
                "Shift+Up on selected folder A must swap it back above B"
            );
        });
    }

    #[gpui::test]
    async fn empty_panel_background_menu_creates_folder_and_connection(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs.clone(), [], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        let (store, panel) = workspace.update_in(cx, |workspace, window, cx| {
            let store = cx.new(DatabaseStore::new);
            let focus_handle = cx.focus_handle();
            let workspace_handle = workspace.weak_handle();
            let table_filter_editor = cx.new(|cx| Editor::single_line(window, cx));
            let panel = cx.new(|_| DatabasePanel {
                focus_handle,
                store: store.clone(),
                workspace: workspace_handle,
                history_expanded: false,
                table_filter_editor,
                collapsed_folders: HashSet::default(),
                collapsed_connections: HashSet::default(),
                editing_folder: None,
                drag_target: None,
                views_expanded: HashSet::default(),
                procedures_expanded: HashSet::default(),
                sequences_expanded: HashSet::default(),
                events_expanded: HashSet::default(),
                table_indexes_expanded: HashSet::default(),
                table_fks_expanded: HashSet::default(),
                table_triggers_expanded: HashSet::default(),
                server_objects_expanded: HashSet::default(),
                server_users: HashMap::default(),
                table_filter_is_regex: false,
                selected_tree_node: None,
                selected_entity: None,
                initial_collapse_pending: false,
                pending_tree_state_serialization: Task::ready(None),
                dump: DumpUiState::default(),
                export: ExportUiState::default(),
                context_menu: None,
                tree_scroll_handle: ScrollHandle::new(),
                _subscriptions: Vec::new(),
            });
            workspace.add_panel(panel.clone(), window, cx);
            (store, panel)
        });

        // An empty store means the tree renders the right-clickable empty state.
        store.read_with(cx, |store, _| {
            assert!(store.connections().is_empty());
            assert!(store.folders().is_empty());
        });

        // The background menu's "New Connection" opens a connection form.
        panel.update_in(cx, |panel, window, cx| {
            panel.new_connection_in_folder(None, window, cx);
        });
        let connection_forms = workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_pane()
                .read(cx)
                .items_of_type::<ConnectionView>()
                .count()
        });
        assert_eq!(
            connection_forms, 1,
            "New Connection opens a connection form"
        );

        // The background menu's "New Folder" creates a top-level folder.
        panel.update_in(cx, |panel, window, cx| {
            panel.start_new_folder(None, window, cx);
        });
        store.read_with(cx, |store, _| {
            assert_eq!(
                store.folders().len(),
                1,
                "New Folder creates a top-level folder"
            );
            assert!(store.folders()[0].parent_id.is_none());
        });
    }

    #[gpui::test]
    async fn pinned_result_tab_is_not_reused(cx: &mut TestAppContext) {
        use std::sync::Arc;
        use std::sync::atomic::AtomicUsize;
        use workspace::Item;

        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs.clone(), [], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        let pane = workspace.update_in(cx, |workspace, window, cx| {
            let project = workspace.project().clone();
            let handle = workspace.weak_handle();
            cx.new(|cx| {
                Pane::new(
                    handle,
                    project,
                    Arc::new(AtomicUsize::new(0)),
                    None,
                    Box::new(zed_actions::database_panel::NewQuery),
                    false,
                    window,
                    cx,
                )
            })
        });

        let conn = uuid::Uuid::new_v4();
        let view_1 = workspace.update_in(cx, |_, window, cx| {
            show_result_in_pane(&pane, conn, "Conn — Results".into(), None, window, cx)
        });
        // Pinning the tab means the next query for the same connection must not
        // reuse it.
        view_1.update(cx, |view, _| view.set_pinned_for_test(true));
        let view_2 = workspace.update_in(cx, |_, window, cx| {
            show_result_in_pane(&pane, conn, "Conn — Results".into(), None, window, cx)
        });

        assert_ne!(
            view_1.entity_id(),
            view_2.entity_id(),
            "a pinned tab must not be reused; a new tab opens instead"
        );
        let (count, second_title) = pane.read_with(cx, |pane, cx| {
            let views: Vec<_> = pane
                .items_of_type::<crate::result_view::ResultView>()
                .collect();
            (
                views.len(),
                views
                    .iter()
                    .map(|view| view.read(cx).tab_content_text(0, cx).to_string())
                    .find(|title| title.ends_with(" 2")),
            )
        });
        assert_eq!(count, 2, "pinning then re-running must yield two tabs");
        assert_eq!(
            second_title.as_deref(),
            Some("Conn — Results 2"),
            "the second tab must be numbered"
        );
    }

    // Loads the console keybinding the same way the real keymap does — by the
    // action's string name from JSON. If `database_panel::RunQuery` is not the
    // action's registered name, load_panic_on_failure panics and this fails,
    // catching a silent "binding dropped, inline assistant wins" regression.
    #[gpui::test]
    fn console_keybinding_resolves_from_json(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let json = r#"[
                {
                    "context": "Editor && mode == full",
                    "bindings": { "ctrl-enter": "database_panel::RunQuery" }
                }
            ]"#;
            let bindings = settings::KeymapFile::load_panic_on_failure(json, cx);
            assert_eq!(
                bindings.len(),
                1,
                "the console ctrl-enter binding must resolve from its JSON action name"
            );
        });
    }

    async fn resolve_definition_ddl(
        provider: &DbSemanticsProvider,
        buffer: &Entity<Buffer>,
        offset: usize,
        cx: &mut VisualTestContext,
    ) -> Option<String> {
        let anchor = buffer.read_with(cx, |buffer, _| buffer.snapshot().anchor_before(offset));
        let task = cx
            .update(|_, cx| provider.definitions(buffer, anchor, GotoDefinitionKind::Symbol, cx))?;
        let links = task.await.ok()??;
        let target = links.first()?.target.buffer.clone();
        Some(target.read_with(cx, |buffer, _| buffer.text()))
    }

    // Regression guard: Ctrl+click (go-to-definition) on a table token must open
    // the table DDL, and on the database token the database DDL. Exercises the
    // semantics provider end to end so "table click stopped working" is caught.
    #[gpui::test]
    async fn semantics_provider_routes_table_and_database_ddl(cx: &mut TestAppContext) {
        let config = db_client::ConnectionConfig {
            label: "ddl".to_string(),
            auto_connect: false,
            ..Default::default()
        };
        let connection_id = config.id;
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs.clone(), [], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        let store = cx.new(DatabaseStore::new);
        store.update(cx, |store, cx| {
            store.add_connected_for_test(config, std::sync::Arc::new(MockProvider), cx);
        });
        cx.run_until_parked();

        let provider = DbSemanticsProvider {
            connection_id,
            store: store.downgrade(),
            workspace: workspace.downgrade(),
        };

        let sql = "SELECT * FROM instruments.splits;";
        let buffer = cx.new(|cx| Buffer::local(sql, cx));

        let table_offset = sql.find("splits").expect("table token") + 1;
        let table_ddl = resolve_definition_ddl(&provider, &buffer, table_offset, cx).await;
        assert_eq!(
            table_ddl.as_deref(),
            Some("TABLE_DDL"),
            "Ctrl+click on a table token must resolve the table DDL"
        );

        let database_offset = sql.find("instruments").expect("database token") + 2;
        let database_ddl = resolve_definition_ddl(&provider, &buffer, database_offset, cx).await;
        assert_eq!(
            database_ddl.as_deref(),
            Some("DATABASE_DDL"),
            "Ctrl+click on a database token must resolve the database DDL"
        );
    }

    // Regression guard for Ctrl+click on table names: the SQL console editor must
    // actually carry the `DbSemanticsProvider` after `install_db_editor_features`
    // runs, otherwise the editor's go-to-definition path never calls
    // `definitions()` and the click does nothing. The earlier test only exercised
    // a hand-built provider; this one drives the real install entry point.
    #[gpui::test]
    async fn install_db_editor_features_sets_semantics_provider(cx: &mut TestAppContext) {
        let config = db_client::ConnectionConfig {
            label: "console".to_string(),
            auto_connect: false,
            ..Default::default()
        };
        let connection_id = config.id;
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs.clone(), [], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        let store = cx.new(DatabaseStore::new);
        store.update(cx, |store, cx| {
            store.add_connected_for_test(config, std::sync::Arc::new(MockProvider), cx);
        });
        cx.run_until_parked();

        let editor = workspace.update_in(cx, |_workspace, window, cx| {
            let buffer = cx.new(|cx| Buffer::local("SELECT * FROM instruments.splits", cx));
            let multi = cx.new(|cx| MultiBuffer::singleton(buffer, cx));
            cx.new(|cx| {
                let mut editor = Editor::for_multibuffer(multi, None, window, cx);
                editor.register_addon(DbQueryEditorAddon::new(connection_id));
                editor
            })
        });

        assert!(
            editor.read_with(cx, |editor, _| editor.semantics_provider().is_none()),
            "no DB semantics provider before install"
        );

        cx.update(|_window, cx| {
            install_db_editor_features(
                editor.clone(),
                store.downgrade(),
                workspace.downgrade(),
                cx,
            );
        });

        assert!(
            editor.read_with(cx, |editor, _| editor.semantics_provider().is_some()),
            "console editor must carry the DbSemanticsProvider so Ctrl+click resolves DDL"
        );
    }

    // The validator runs debounced off buffer edits, never on the edit itself,
    // and must actually reach the buffer's diagnostic set once it does run.
    #[gpui::test]
    async fn install_db_editor_features_debounces_sql_validation(cx: &mut TestAppContext) {
        let mut config = db_client::ConnectionConfig {
            label: "console".to_string(),
            auto_connect: false,
            database: Some("db".to_string()),
            ..Default::default()
        };
        config.driver = db_client::DatabaseDriver::MySQL;
        let connection_id = config.id;
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs.clone(), [], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        let store = cx.new(DatabaseStore::new);
        store.update(cx, |store, cx| {
            store.add_connected_for_test(config, std::sync::Arc::new(MockProvider), cx);
            let conn = store
                .connections
                .iter_mut()
                .find(|c| c.config.id == connection_id)
                .unwrap();
            conn.expanded_databases.insert(
                "db".to_string(),
                vec![db_client::TableInfo {
                    name: "orders".to_string(),
                    kind: db_client::TableKind::Table,
                }],
            );
        });
        cx.run_until_parked();

        let (buffer, editor) = workspace.update_in(cx, |_workspace, window, cx| {
            let buffer = cx.new(|cx| Buffer::local("SELECT * FROM db.missing_table", cx));
            let multi = cx.new(|cx| MultiBuffer::singleton(buffer.clone(), cx));
            let editor = cx.new(|cx| {
                let mut editor = Editor::for_multibuffer(multi, None, window, cx);
                editor.register_addon(DbQueryEditorAddon::new(connection_id));
                editor
            });
            (buffer, editor)
        });

        cx.update(|_window, cx| {
            install_db_editor_features(
                editor.clone(),
                store.downgrade(),
                workspace.downgrade(),
                cx,
            );
        });
        cx.run_until_parked();

        assert!(
            buffer.read_with(cx, |buffer, _| buffer
                .buffer_diagnostics(Some(SQL_VALIDATOR_SERVER_ID))
                .is_empty()),
            "the very first validation pass still has to wait out the debounce"
        );

        cx.executor().advance_clock(SQL_VALIDATION_DEBOUNCE);
        cx.run_until_parked();

        assert_eq!(
            buffer.read_with(cx, |buffer, _| buffer
                .buffer_diagnostics(Some(SQL_VALIDATOR_SERVER_ID))
                .len()),
            1,
            "an unknown table in a cached database must surface as a buffer diagnostic"
        );
    }

    // A console restored from a session has no addon, so the reinstall hook must
    // recognise it by its file path. This guards that path detection round-trips.
    #[test]
    fn console_path_round_trips_to_connection_id() {
        let id = db_client::ConnectionConfig::default().id;
        let path = super::connection_query_path(id, "My Conn!!", DatabaseDriver::MySQL);
        assert_eq!(
            super::connection_id_from_console_path(&path, &[id]),
            Some(id)
        );
        let unrelated = std::path::Path::new("/tmp/not-a-console.sql");
        assert_eq!(
            super::connection_id_from_console_path(unrelated, &[id]),
            None
        );
    }

    // The reinstall hook runs for every new editor; it must leave editors that are
    // not DB consoles untouched (never attach the console addon to a plain editor).
    #[gpui::test]
    async fn console_feature_hook_skips_non_console_editors(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs.clone(), [], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        let store = cx.new(DatabaseStore::new);
        store.update(cx, |store, cx| {
            store.add_connected_for_test(
                db_client::ConnectionConfig::default(),
                std::sync::Arc::new(MockProvider),
                cx,
            );
        });
        cx.update(|_window, cx| cx.set_global(crate::store::GlobalDatabaseStore(store.clone())));
        cx.run_until_parked();

        let editor = workspace.update_in(cx, |_workspace, window, cx| {
            let buffer = cx.new(|cx| Buffer::local("SELECT 1", cx));
            let multi = cx.new(|cx| MultiBuffer::singleton(buffer, cx));
            cx.new(|cx| Editor::for_multibuffer(multi, None, window, cx))
        });
        cx.run_until_parked();

        assert!(
            editor.read_with(cx, |editor, _| editor
                .addon::<DbQueryEditorAddon>()
                .is_none()),
            "the reinstall hook must not attach the console addon to a non-console editor"
        );
    }

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            super::init(cx);
        });
    }

    fn debug_center(cx: &mut VisualTestContext, selector: &'static str) -> gpui::Point<Pixels> {
        cx.debug_bounds(selector)
            .unwrap_or_else(|| panic!("expected debug bounds for {selector}"))
            .center()
    }

    // Autonomously verifies the Ctrl+Enter precedence without a live database,
    // mirroring production exactly: the inline-assistant binding sits on
    // `!AcpThread > Editor && mode == full`, the console binding on
    // `Editor && mode == full` added last. Both match at the same context depth
    // (the Editor node), so the later-loaded console binding wins by index. This
    // is the precedence guarantee that keeps ctrl-enter from opening the inline
    // assistant instead of running the query.
    #[gpui::test]
    async fn ctrl_enter_dispatches_run_query_in_db_console(cx: &mut TestAppContext) {
        use zed_actions::database_panel::RunQuery;

        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            // Same contexts and load order as production: the inline-assistant
            // binding first (default keymap), the console binding last (base
            // keymap). The console binding must win.
            cx.bind_keys([
                gpui::KeyBinding::new(
                    "ctrl-enter",
                    CompetingAssistProbe,
                    Some("!AcpThread > Editor && mode == full"),
                ),
                gpui::KeyBinding::new("ctrl-enter", RunQuery, Some("Editor && mode == full")),
            ]);
        });

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs.clone(), [], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        let ran = std::rc::Rc::new(std::cell::Cell::new(false));
        let competed = std::rc::Rc::new(std::cell::Cell::new(false));
        workspace.update_in(cx, {
            let ran = ran.clone();
            let competed = competed.clone();
            move |workspace, _window, _cx| {
                workspace.register_action(move |_, _: &RunQuery, _, _| {
                    ran.set(true);
                });
                workspace.register_action(move |_, _: &CompetingAssistProbe, _, _| {
                    competed.set(true);
                });
            }
        });

        let editor = workspace.update_in(cx, |workspace, window, cx| {
            let buffer = cx.new(|cx| language::Buffer::local("SELECT 1", cx));
            let multi = cx.new(|cx| MultiBuffer::singleton(buffer, cx));
            let editor = cx.new(|cx| {
                let mut editor = Editor::for_multibuffer(multi, None, window, cx);
                editor.register_addon(DbQueryEditorAddon::new(uuid::Uuid::new_v4()));
                editor.set_show_runnables(true, cx);
                editor
            });
            workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
            editor
        });

        editor.update_in(cx, |editor, window, cx| {
            let handle = editor.focus_handle(cx);
            window.focus(&handle, cx);
        });
        cx.run_until_parked();

        cx.simulate_keystrokes("ctrl-enter");
        cx.run_until_parked();

        assert!(
            ran.get(),
            "ctrl-enter in a DbQueryEditor must dispatch RunQuery"
        );
        assert!(
            !competed.get(),
            "ctrl-enter must not fall through to the inline-assistant binding"
        );
    }

    // Guards the robust design: in a normal editor (no DbQueryEditor addon), the
    // RunQuery handler must propagate so the editor's own ctrl-enter binding
    // still fires. This is what keeps the global binding from breaking normal
    // editors and is the regression guard for "ctrl-enter broke again".
    #[gpui::test]
    async fn ctrl_enter_propagates_in_non_console_editor(cx: &mut TestAppContext) {
        use zed_actions::database_panel::RunQuery;

        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            // The console binding wins first (added last); a stand-in for the
            // editor's default ctrl-enter binding is added before it.
            cx.bind_keys([
                gpui::KeyBinding::new(
                    "ctrl-enter",
                    CompetingAssistProbe,
                    Some("Editor && mode == full"),
                ),
                gpui::KeyBinding::new("ctrl-enter", RunQuery, Some("Editor && mode == full")),
            ]);
        });

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs.clone(), [], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        let fell_through = std::rc::Rc::new(std::cell::Cell::new(false));
        workspace.update_in(cx, {
            let fell_through = fell_through.clone();
            move |workspace, _window, _cx| {
                // The real RunQuery handler — with no panel/addon it must propagate.
                workspace.register_action(|workspace, _: &RunQuery, window, cx| {
                    run_current_sql_query(workspace, window, cx);
                });
                workspace.register_action(move |_, _: &CompetingAssistProbe, _, _| {
                    fell_through.set(true);
                });
            }
        });

        // A plain editor WITHOUT the DbQueryEditor addon.
        let editor = workspace.update_in(cx, |workspace, window, cx| {
            let buffer = cx.new(|cx| language::Buffer::local("not sql", cx));
            let multi = cx.new(|cx| MultiBuffer::singleton(buffer, cx));
            let editor = cx.new(|cx| Editor::for_multibuffer(multi, None, window, cx));
            workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
            editor
        });
        editor.update_in(cx, |editor, window, cx| {
            let handle = editor.focus_handle(cx);
            window.focus(&handle, cx);
        });
        cx.run_until_parked();

        cx.simulate_keystrokes("ctrl-enter");
        cx.run_until_parked();

        assert!(
            fell_through.get(),
            "ctrl-enter in a non-console editor must propagate past RunQuery to the default binding"
        );
    }

    // Verifies the panel opens when toggle_panel_focus is called directly
    // (existing test, covers the Panel trait plumbing)
    #[gpui::test]
    async fn test_database_panel_opens_on_toggle_focus(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs.clone(), [], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        workspace.update_in(cx, |workspace, window, cx| {
            let store = cx.new(|cx| DatabaseStore::new(cx));
            let focus_handle = cx.focus_handle();
            let workspace_handle = workspace.weak_handle();
            let table_filter_editor = cx.new(|cx| Editor::single_line(window, cx));
            let panel = cx.new(|cx| {
                let sub = cx.subscribe(
                    &store,
                    |_: &mut DatabasePanel,
                     _: Entity<DatabaseStore>,
                     _: &DatabaseStoreEvent,
                     cx: &mut Context<DatabasePanel>| {
                        cx.notify();
                    },
                );
                DatabasePanel {
                    focus_handle,
                    store,
                    workspace: workspace_handle,
                    history_expanded: false,
                    table_filter_editor,
                    collapsed_folders: HashSet::default(),
                    collapsed_connections: HashSet::default(),
                    editing_folder: None,
                    drag_target: None,
                    views_expanded: HashSet::default(),
                    procedures_expanded: HashSet::default(),
                    sequences_expanded: HashSet::default(),
                    events_expanded: HashSet::default(),
                    table_indexes_expanded: HashSet::default(),
                    table_fks_expanded: HashSet::default(),
                    table_triggers_expanded: HashSet::default(),
                    server_objects_expanded: HashSet::default(),
                    server_users: HashMap::default(),
                    table_filter_is_regex: false,
                    selected_tree_node: None,
                    selected_entity: None,
                    initial_collapse_pending: false,
                    pending_tree_state_serialization: Task::ready(None),
                    dump: DumpUiState::default(),
                    export: ExportUiState::default(),
                    context_menu: None,
                    tree_scroll_handle: ScrollHandle::new(),
                    _subscriptions: vec![sub],
                }
            });
            workspace.add_panel(panel, window, cx);
        });

        cx.run_until_parked();

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.toggle_panel_focus::<DatabasePanel>(window, cx);
        });

        cx.run_until_parked();

        workspace.read_with(cx, |workspace, cx| {
            let dock = workspace.left_dock().read(cx);
            assert!(
                dock.is_open(),
                "left dock must be open after toggle_panel_focus"
            );
            assert!(
                dock.panel::<DatabasePanel>().is_some(),
                "DatabasePanel must be in left dock"
            );
        });
    }

    // Proves the tree can actually scroll: with far more connection rows than
    // fit in the test window, a real layout+paint pass must leave the scroll
    // handle with a nonzero max offset. `Scrollbars::tracked_scroll_handle`
    // marks the handle as manually-added, which makes `custom_scrollbars`
    // SKIP its own internal `.track_scroll()`/`.overflow_scroll()` wiring
    // (see `ScrollbarState::handle_to_track`) -- the caller supplying its own
    // handle is responsible for wiring the div itself. Without an explicit
    // `.overflow_scroll().track_scroll(&handle)` call, the scroll handle's
    // `scroll_offset` is never set, `clamp_scroll_position` never runs, and
    // `max_offset`/`bounds` never update from their zero-initialized state --
    // regardless of how much taller the actual content is.
    #[gpui::test]
    async fn database_explorer_tree_scrolls_when_content_overflows_the_viewport(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs.clone(), [], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        let store = workspace.update_in(cx, |workspace, window, cx| {
            let store = cx.new(|cx| DatabaseStore::new(cx));
            let focus_handle = cx.focus_handle();
            let workspace_handle = workspace.weak_handle();
            let table_filter_editor = cx.new(|cx| Editor::single_line(window, cx));
            let panel = cx.new(|cx| {
                let sub = cx.subscribe(
                    &store,
                    |_: &mut DatabasePanel,
                     _: Entity<DatabaseStore>,
                     _: &DatabaseStoreEvent,
                     cx: &mut Context<DatabasePanel>| {
                        cx.notify();
                    },
                );
                DatabasePanel {
                    focus_handle,
                    store: store.clone(),
                    workspace: workspace_handle,
                    history_expanded: false,
                    table_filter_editor,
                    collapsed_folders: HashSet::default(),
                    collapsed_connections: HashSet::default(),
                    editing_folder: None,
                    drag_target: None,
                    views_expanded: HashSet::default(),
                    procedures_expanded: HashSet::default(),
                    sequences_expanded: HashSet::default(),
                    events_expanded: HashSet::default(),
                    table_indexes_expanded: HashSet::default(),
                    table_fks_expanded: HashSet::default(),
                    table_triggers_expanded: HashSet::default(),
                    server_objects_expanded: HashSet::default(),
                    server_users: HashMap::default(),
                    table_filter_is_regex: false,
                    selected_tree_node: None,
                    selected_entity: None,
                    initial_collapse_pending: false,
                    pending_tree_state_serialization: Task::ready(None),
                    dump: DumpUiState::default(),
                    export: ExportUiState::default(),
                    context_menu: None,
                    tree_scroll_handle: ScrollHandle::new(),
                    _subscriptions: vec![sub],
                }
            });
            workspace.add_panel(panel, window, cx);
            store
        });

        store.update(cx, |store, cx| {
            for i in 0..80 {
                store.add_connection(
                    db_client::ConnectionConfig {
                        label: format!("connection-{i}"),
                        auto_connect: false,
                        ..Default::default()
                    },
                    cx,
                );
            }
        });
        cx.run_until_parked();

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.toggle_panel_focus::<DatabasePanel>(window, cx);
        });
        cx.run_until_parked();

        // Force a real layout+paint pass so the scroll handle's bounds and
        // content size reflect what was actually measured, not just default
        // zero-initialized state.
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        cx.run_until_parked();

        let panel = workspace
            .read_with(cx, |workspace, cx| {
                workspace.left_dock().read(cx).panel::<DatabasePanel>()
            })
            .expect("DatabasePanel must be in the left dock");

        let max_offset_y = panel.read_with(cx, |panel, _| panel.tree_scroll_handle.max_offset().y);
        assert!(
            max_offset_y > px(0.),
            "tree has 80 connection rows, far more than fit in the test window, so the scroll \
             handle must report a nonzero max scroll offset; got {max_offset_y:?}"
        );

        // A row's own chrome (indent, chevron, driver icon + status dot, label
        // column, and the New Query / Connect buttons) genuinely needs a little
        // more than this test's narrow viewport even for a short label like
        // "connection-79" -- that's real content width, not a bug, so this
        // asserts "no row is dramatically wider than the viewport" rather than
        // "zero scroll capacity", distinguishing it from the >100px overflow a
        // genuinely long label produces in the sibling test above.
        let max_offset_x = panel.read_with(cx, |panel, _| panel.tree_scroll_handle.max_offset().x);
        assert!(
            max_offset_x < px(100.),
            "short connection labels should need at most a small amount of horizontal scroll \
             for the row's own chrome, not the hundreds of pixels a genuinely long label would \
             produce; got {max_offset_x:?}"
        );
    }

    // A connection label far wider than the test window's panel must produce a
    // nonzero horizontal scroll range -- this is the counterpart to the
    // vertical-overflow test above, checking the `x` axis instead of `y`.
    //
    // Root cause (previously an unfixed bug, now fixed): `db-panel-scroll` had
    // no `align-items` override, so as either a block container (no `.flex()`
    // at all) or a flex container it defaulted to stretching its single child
    // to the viewport's exact width; `db-tree-background` (a `v_flex()`) had
    // the same default-stretch problem for each row. Stretch bypasses
    // content-based measurement entirely, so a row's true (potentially wider)
    // content size never had a chance to propagate up regardless of what any
    // individual child's width was set to -- explaining why even a
    // `flex_none` child with an explicit, definite `.w(px(2000.))` still
    // measured at exactly the viewport width. The fix: give `db-panel-scroll`
    // an explicit `.flex().flex_col().items_start()` and `db-tree-background`
    // an `.items_start()` too, so each level measures its child by content
    // instead of stretching it. Their pre-existing `.min_w_full()` (and
    // `.min_h_full()`) still act as a floor, so short rows keep filling the
    // panel's width for a full-width hover/selection highlight; only rows
    // that genuinely need more get to overflow and become scrollable.
    // `db-tree-background` also needs `.flex_shrink_0()` -- without it,
    // `db-panel-scroll` becoming a flex column made its single child a
    // main-axis flex item, and the default `flex-shrink: 1` silently crushed
    // it back down to the viewport *height*, breaking the vertical-overflow
    // test above; `flex_shrink_0()` stops that regression.
    #[gpui::test]
    async fn database_explorer_tree_scrolls_horizontally_when_a_label_overflows_the_viewport(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs.clone(), [], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        let store = workspace.update_in(cx, |workspace, window, cx| {
            let store = cx.new(|cx| DatabaseStore::new(cx));
            let focus_handle = cx.focus_handle();
            let workspace_handle = workspace.weak_handle();
            let table_filter_editor = cx.new(|cx| Editor::single_line(window, cx));
            let panel = cx.new(|cx| {
                let sub = cx.subscribe(
                    &store,
                    |_: &mut DatabasePanel,
                     _: Entity<DatabaseStore>,
                     _: &DatabaseStoreEvent,
                     cx: &mut Context<DatabasePanel>| {
                        cx.notify();
                    },
                );
                DatabasePanel {
                    focus_handle,
                    store: store.clone(),
                    workspace: workspace_handle,
                    history_expanded: false,
                    table_filter_editor,
                    collapsed_folders: HashSet::default(),
                    collapsed_connections: HashSet::default(),
                    editing_folder: None,
                    drag_target: None,
                    views_expanded: HashSet::default(),
                    procedures_expanded: HashSet::default(),
                    sequences_expanded: HashSet::default(),
                    events_expanded: HashSet::default(),
                    table_indexes_expanded: HashSet::default(),
                    table_fks_expanded: HashSet::default(),
                    table_triggers_expanded: HashSet::default(),
                    server_objects_expanded: HashSet::default(),
                    server_users: HashMap::default(),
                    table_filter_is_regex: false,
                    selected_tree_node: None,
                    selected_entity: None,
                    initial_collapse_pending: false,
                    pending_tree_state_serialization: Task::ready(None),
                    dump: DumpUiState::default(),
                    export: ExportUiState::default(),
                    context_menu: None,
                    tree_scroll_handle: ScrollHandle::new(),
                    _subscriptions: vec![sub],
                }
            });
            workspace.add_panel(panel, window, cx);
            store
        });

        store.update(cx, |store, cx| {
            store.add_connection(
                db_client::ConnectionConfig {
                    label: "a-connection-name-so-long-it-must-overflow-any-reasonable-panel-width-and-then-some-more"
                        .to_string(),
                    auto_connect: false,
                    ..Default::default()
                },
                cx,
            );
        });
        cx.run_until_parked();

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.toggle_panel_focus::<DatabasePanel>(window, cx);
        });
        cx.run_until_parked();

        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        cx.run_until_parked();

        let panel = workspace
            .read_with(cx, |workspace, cx| {
                workspace.left_dock().read(cx).panel::<DatabasePanel>()
            })
            .expect("DatabasePanel must be in the left dock");

        let max_offset_x = panel.read_with(cx, |panel, _| panel.tree_scroll_handle.max_offset().x);
        assert!(
            max_offset_x > px(0.),
            "the connection label is far wider than the panel, so the scroll handle must report \
             a nonzero horizontal max offset; got {max_offset_x:?}"
        );
    }

    // Replicates what the real app does:
    // 1. Uses DatabasePanel::load (as initialize_panels in zed.rs does)
    // 2. Registers ToggleFocus on the workspace (as zed::register_actions does)
    // 3. Dispatches ToggleFocus action (as the View menu click does)
    // 4. Asserts the dock opened and the panel is visible
    #[gpui::test]
    async fn test_panel_load_and_action_dispatch(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs.clone(), [], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        // Load the panel exactly as initialize_panels does in zed.rs.
        // spawn_in requires &mut AsyncWindowContext; load takes owned, so we clone.
        let panel = workspace
            .update_in(cx, |_, window, cx| {
                cx.spawn_in(
                    window,
                    async move |workspace_handle, cx: &mut AsyncWindowContext| {
                        DatabasePanel::load(workspace_handle, cx.clone()).await
                    },
                )
            })
            .await
            .expect("DatabasePanel::load must succeed");

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
                workspace.toggle_panel_focus::<DatabasePanel>(window, cx);
            });
            workspace.add_panel(panel, window, cx);
        });

        cx.run_until_parked();

        // Dispatch ToggleFocus exactly as the View menu click does
        cx.dispatch_action(ToggleFocus);

        cx.run_until_parked();

        workspace.read_with(cx, |workspace, cx| {
            let dock = workspace.left_dock().read(cx);
            assert!(
                dock.is_open(),
                "left dock must be open after ToggleFocus action dispatch"
            );
            assert!(
                dock.panel::<DatabasePanel>().is_some(),
                "DatabasePanel must be in left dock"
            );
        });
    }

    // Mirrors the zed.rs register_actions path: registers ToggleFocus directly on
    // the workspace (as zed::register_actions does for ProjectPanel, TerminalPanel, etc.)
    // and verifies the menu dispatch path works.
    #[gpui::test]
    async fn test_panel_toggle_via_register_actions_path(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs.clone(), [], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        // Register ToggleFocus directly on the workspace, exactly as zed.rs register_actions does.
        workspace.update_in(cx, |workspace, _, _| {
            workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
                workspace.toggle_panel_focus::<DatabasePanel>(window, cx);
            });
        });

        let panel = workspace
            .update_in(cx, |_, window, cx| {
                cx.spawn_in(
                    window,
                    async move |workspace_handle, cx: &mut AsyncWindowContext| {
                        DatabasePanel::load(workspace_handle, cx.clone()).await
                    },
                )
            })
            .await
            .expect("DatabasePanel::load must succeed");

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_panel(panel, window, cx);
        });

        cx.run_until_parked();

        cx.dispatch_action(ToggleFocus);

        cx.run_until_parked();

        workspace.read_with(cx, |workspace, cx| {
            let dock = workspace.left_dock().read(cx);
            assert!(
                dock.is_open(),
                "left dock must be open after ToggleFocus via register_actions path"
            );
            assert!(
                dock.panel::<DatabasePanel>().is_some(),
                "DatabasePanel must be in left dock"
            );
        });
    }

    async fn load_connected_panel(
        cx: &mut TestAppContext,
        config: db_client::ConnectionConfig,
    ) -> (Entity<DatabasePanel>, VisualTestContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs.clone(), [], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let mut visual_cx = VisualTestContext::from_window(window.into(), cx);
        let panel = workspace
            .update_in(&mut visual_cx, |_, window, cx| {
                cx.spawn_in(
                    window,
                    async move |workspace_handle, cx: &mut AsyncWindowContext| {
                        DatabasePanel::load(workspace_handle, cx.clone()).await
                    },
                )
            })
            .await
            .expect("DatabasePanel::load must succeed");
        workspace.update_in(&mut visual_cx, |workspace, window, cx| {
            workspace.add_panel(panel.clone(), window, cx);
        });
        panel.update(&mut visual_cx, |panel, cx| {
            panel.store.update(cx, |store, cx| {
                store.add_connected_for_test(config, std::sync::Arc::new(MockProvider), cx);
            });
        });
        visual_cx.run_until_parked();
        (panel, visual_cx)
    }

    #[gpui::test]
    async fn quick_doc_populates_title_from_table(cx: &mut TestAppContext) {
        let config = db_client::ConnectionConfig {
            label: "quick-doc".to_string(),
            auto_connect: false,
            ..Default::default()
        };
        let connection_id = config.id;
        let (panel, mut cx) = load_connected_panel(cx, config).await;
        let workspace = panel
            .read_with(&cx, |panel, _| panel.workspace.upgrade())
            .expect("workspace handle must be live");

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.open_quick_doc(
                connection_id,
                "public".to_string(),
                "users".to_string(),
                window,
                cx,
            );
        });
        cx.run_until_parked();

        let has_modal = workspace.read_with(&cx, |workspace, cx| {
            workspace.active_modal::<QuickDocView>(cx).is_some()
        });
        assert!(
            has_modal,
            "Quick Documentation must open as a workspace modal"
        );
    }

    #[gpui::test]
    async fn server_objects_expand_loads_users(cx: &mut TestAppContext) {
        let config = db_client::ConnectionConfig {
            label: "server-objects".to_string(),
            auto_connect: false,
            ..Default::default()
        };
        let connection_id = config.id;
        let (panel, mut cx) = load_connected_panel(cx, config).await;

        panel.update(&mut cx, |panel, cx| {
            panel.toggle_server_objects(connection_id, cx);
        });
        cx.run_until_parked();

        panel.read_with(&cx, |panel, _| {
            assert!(
                panel.server_objects_expanded.contains(&connection_id),
                "Server Objects node must be expanded after toggle"
            );
            assert!(
                panel.server_users.contains_key(&connection_id),
                "users must be fetched and cached when Server Objects expands"
            );
        });

        panel.update(&mut cx, |panel, cx| {
            panel.toggle_server_objects(connection_id, cx);
        });
        panel.read_with(&cx, |panel, _| {
            assert!(
                !panel.server_objects_expanded.contains(&connection_id),
                "toggling again must collapse the Server Objects node"
            );
        });
    }

    struct RoutineTreeProvider;

    #[async_trait::async_trait]
    impl db_client::DbProvider for RoutineTreeProvider {
        async fn ping(&self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn list_databases(&self) -> anyhow::Result<Vec<db_client::DatabaseInfo>> {
            Ok(vec![db_client::DatabaseInfo {
                name: "shop".into(),
            }])
        }
        async fn list_tables(&self, _database: &str) -> anyhow::Result<Vec<db_client::TableInfo>> {
            Ok(Vec::new())
        }
        async fn describe_table(
            &self,
            _database: &str,
            _table: &str,
        ) -> anyhow::Result<Vec<db_client::ColumnInfo>> {
            Ok(Vec::new())
        }
        async fn execute_query(
            &self,
            _database: &str,
            _sql: &str,
        ) -> anyhow::Result<db_client::schema::QueryResult> {
            Ok(db_client::schema::QueryResult {
                columns: Vec::new(),
                rows: Vec::new(),
                rows_affected: 0,
                execution_time_ms: 0,
            })
        }
        async fn get_table_ddl(&self, _database: &str, _table: &str) -> anyhow::Result<String> {
            Ok(String::new())
        }
        async fn list_procedures(
            &self,
            _database: &str,
        ) -> anyhow::Result<Vec<db_client::schema::ProcedureInfo>> {
            Ok(vec![db_client::schema::ProcedureInfo {
                name: "recalc_totals".into(),
                kind: db_client::schema::ProcedureKind::Procedure,
                definition: Some("BEGIN UPDATE orders SET total = 0; END".into()),
            }])
        }
    }

    // Real end-to-end proof that the "Routines" tree section (feature-gap item
    // 11) is reachable and functional purely through simulated clicks: expand
    // the database row, expand the Routines group it reveals, click a
    // procedure leaf, and confirm its cached source opens as a console tab --
    // not a call into the render/toggle methods directly.
    #[gpui::test]
    async fn clicking_a_procedure_row_opens_its_source_as_a_console_tab(cx: &mut TestAppContext) {
        let config = db_client::ConnectionConfig {
            label: "routine-tree".to_string(),
            auto_connect: false,
            ..Default::default()
        };
        let connection_id = config.id;

        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs.clone(), [], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        let panel = workspace
            .update_in(&mut cx, |_, window, cx| {
                cx.spawn_in(
                    window,
                    async move |workspace_handle, cx: &mut AsyncWindowContext| {
                        DatabasePanel::load(workspace_handle, cx.clone()).await
                    },
                )
            })
            .await
            .expect("DatabasePanel::load must succeed");
        workspace.update_in(&mut cx, |workspace, window, cx| {
            workspace.add_panel(panel.clone(), window, cx);
        });
        panel.update(&mut cx, |panel, cx| {
            panel.store.update(cx, |store, cx| {
                store.add_connected_for_test(config, std::sync::Arc::new(RoutineTreeProvider), cx);
                // Populates only `conn.databases` (so the tree can render a
                // db-row to click) without touching `expanded_databases`,
                // which would otherwise make the click below take the
                // already-cached shortcut path and skip the real
                // list_procedures/list_sequences/list_events fetch this test
                // exists to prove.
                store
                    .ensure_schema_for_completion(connection_id, String::new(), cx)
                    .detach();
            });
        });
        cx.run_until_parked();
        // Connections start collapsed by default; this test clicks into the
        // connection's tree, so it must be expanded first.
        panel.update(&mut cx, |panel, cx| {
            panel.toggle_connection_collapsed(connection_id, cx);
        });

        workspace.update_in(&mut cx, |workspace, window, cx| {
            workspace.toggle_panel_focus::<DatabasePanel>(window, cx);
        });
        cx.run_until_parked();

        let db_row = debug_center(&mut cx, format!("db-row-{}-shop", connection_id).leak());
        cx.simulate_click(db_row, gpui::Modifiers::none());
        cx.run_until_parked();

        let routines_header = debug_center(
            &mut cx,
            format!("procedures-group-{}-shop", connection_id).leak(),
        );
        cx.simulate_click(routines_header, gpui::Modifiers::none());
        cx.run_until_parked();

        let procedure_row = debug_center(
            &mut cx,
            format!("procedure-{}-shop-recalc_totals", connection_id).leak(),
        );
        cx.simulate_click(procedure_row, gpui::Modifiers::none());
        cx.run_until_parked();

        let editor_text = workspace.read_with(&cx, |workspace, cx| {
            workspace
                .active_item(cx)
                .and_then(|item| item.act_as::<Editor>(cx))
                .map(|editor| editor.read(cx).text(cx))
        });
        assert_eq!(
            editor_text.as_deref(),
            Some("BEGIN UPDATE orders SET total = 0; END"),
            "clicking a Routines leaf must open its cached source as a real console tab"
        );
    }

    #[gpui::test]
    async fn clicking_a_folder_or_a_disconnected_connection_row_selects_and_highlights_it(
        cx: &mut TestAppContext,
    ) {
        let config = db_client::ConnectionConfig {
            label: "select-me".to_string(),
            auto_connect: false,
            ..Default::default()
        };
        let connection_id = config.id;

        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs.clone(), [], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        let panel = workspace
            .update_in(&mut cx, |_, window, cx| {
                cx.spawn_in(
                    window,
                    async move |workspace_handle, cx: &mut AsyncWindowContext| {
                        DatabasePanel::load(workspace_handle, cx.clone()).await
                    },
                )
            })
            .await
            .expect("DatabasePanel::load must succeed");
        workspace.update_in(&mut cx, |workspace, window, cx| {
            workspace.add_panel(panel.clone(), window, cx);
        });
        let folder_id = panel
            .update(&mut cx, |panel, cx| {
                panel
                    .store
                    .update(cx, |store, cx| store.add_folder("Prod".into(), None, cx))
            })
            .expect("folder must be created");
        panel.update(&mut cx, |panel, cx| {
            panel.store.update(cx, |store, cx| {
                store.add_connection(config, cx);
            });
            // Everything starts collapsed by default; expand the top level so
            // both rows are actually laid out and clickable.
            panel.collapsed_folders.clear();
        });
        cx.run_until_parked();

        workspace.update_in(&mut cx, |workspace, window, cx| {
            workspace.toggle_panel_focus::<DatabasePanel>(window, cx);
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        cx.run_until_parked();

        panel.read_with(&cx, |panel, _| {
            assert_eq!(panel.selected_entity, None, "nothing is selected yet");
        });

        // Click the connection row first: unlike the folder row, selecting it
        // must not itself change the tree layout (no toggling, no auto-expand),
        // so this click's bounds stay valid for the follow-up assertions.
        let conn_bounds = cx
            .debug_bounds(format!("conn-header-{connection_id}").leak())
            .expect("expected debug bounds for conn-header");
        let conn_row = gpui::Point {
            x: conn_bounds.origin.x + px(20.),
            y: conn_bounds.center().y,
        };
        cx.simulate_click(conn_row, gpui::Modifiers::none());
        cx.run_until_parked();
        panel.read_with(&cx, |panel, _| {
            assert_eq!(
                panel.selected_entity,
                Some(SelectedEntity::Connection(connection_id)),
                "a real click on a disconnected connection row must select it"
            );
        });
        panel.read_with(&cx, |panel, cx| {
            let status = panel
                .store
                .read(cx)
                .connections()
                .iter()
                .find(|c| c.config.id == connection_id)
                .map(|c| c.status.clone());
            assert!(
                matches!(status, Some(ConnectionStatus::Disconnected)),
                "selecting a disconnected connection row must not connect it, got {status:?}"
            );
        });

        let folder_row = debug_center(&mut cx, format!("folder-row-{folder_id}").leak());
        cx.simulate_click(folder_row, gpui::Modifiers::none());
        cx.run_until_parked();
        panel.read_with(&cx, |panel, _| {
            assert_eq!(
                panel.selected_entity,
                Some(SelectedEntity::Folder(folder_id)),
                "clicking a different row moves the selection to it"
            );
        });
    }

    #[gpui::test]
    async fn connection_row_no_longer_renders_its_own_action_icons(cx: &mut TestAppContext) {
        let config = db_client::ConnectionConfig {
            label: "no-row-icons".to_string(),
            auto_connect: false,
            ..Default::default()
        };
        let connection_id = config.id;

        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs.clone(), [], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        let panel = workspace
            .update_in(&mut cx, |_, window, cx| {
                cx.spawn_in(
                    window,
                    async move |workspace_handle, cx: &mut AsyncWindowContext| {
                        DatabasePanel::load(workspace_handle, cx.clone()).await
                    },
                )
            })
            .await
            .expect("DatabasePanel::load must succeed");
        workspace.update_in(&mut cx, |workspace, window, cx| {
            workspace.add_panel(panel.clone(), window, cx);
        });
        panel.update(&mut cx, |panel, cx| {
            panel.store.update(cx, |store, cx| {
                store.add_connection(config, cx);
            });
            panel.collapsed_folders.clear();
            panel.collapsed_connections.clear();
        });
        cx.run_until_parked();
        workspace.update_in(&mut cx, |workspace, window, cx| {
            workspace.toggle_panel_focus::<DatabasePanel>(window, cx);
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        cx.run_until_parked();

        for selector in [
            format!("new-query-{connection_id}"),
            format!("connect-{connection_id}"),
            format!("move-up-{connection_id}"),
            format!("move-down-{connection_id}"),
            format!("edit-conn-{connection_id}"),
            format!("dup-conn-{connection_id}"),
            format!("delete-conn-{connection_id}"),
        ] {
            assert!(
                cx.debug_bounds(selector.clone().leak()).is_none(),
                "the per-row action icon {selector:?} must no longer be rendered on a connection row"
            );
        }
        assert!(
            cx.debug_bounds("selection-connect").is_some(),
            "the unified action bar's Connect button must be rendered instead"
        );
    }

    #[gpui::test]
    async fn selection_action_bar_is_disabled_until_a_connection_is_selected(
        cx: &mut TestAppContext,
    ) {
        let config = db_client::ConnectionConfig {
            label: "bar-enable".to_string(),
            auto_connect: false,
            ..Default::default()
        };
        let connection_id = config.id;

        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs.clone(), [], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        let panel = workspace
            .update_in(&mut cx, |_, window, cx| {
                cx.spawn_in(
                    window,
                    async move |workspace_handle, cx: &mut AsyncWindowContext| {
                        DatabasePanel::load(workspace_handle, cx.clone()).await
                    },
                )
            })
            .await
            .expect("DatabasePanel::load must succeed");
        workspace.update_in(&mut cx, |workspace, window, cx| {
            workspace.add_panel(panel.clone(), window, cx);
        });
        panel.update(&mut cx, |panel, cx| {
            panel.store.update(cx, |store, cx| {
                store.add_connection(config, cx);
            });
            panel.collapsed_folders.clear();
            panel.collapsed_connections.clear();
        });
        cx.run_until_parked();
        workspace.update_in(&mut cx, |workspace, window, cx| {
            workspace.toggle_panel_focus::<DatabasePanel>(window, cx);
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        cx.run_until_parked();

        // Uses Duplicate rather than Connect: Connect spawns a real network
        // task on the shared Tokio runtime (see `runtime.rs`) that outlives
        // this test's teardown against an unreachable default host/port,
        // which aborts the process. Duplicate exercises the same
        // disabled-until-selected wiring synchronously.
        let duplicate_button = debug_center(&mut cx, "selection-duplicate");
        cx.simulate_click(duplicate_button, gpui::Modifiers::none());
        cx.run_until_parked();
        panel.read_with(&cx, |panel, cx| {
            let count = panel.store.read(cx).connections().len();
            assert_eq!(
                count, 1,
                "the Duplicate button must do nothing while no connection is selected"
            );
        });

        let conn_bounds = cx
            .debug_bounds(format!("conn-header-{connection_id}").leak())
            .expect("expected debug bounds for conn-header");
        let conn_row = gpui::Point {
            x: conn_bounds.origin.x + px(20.),
            y: conn_bounds.center().y,
        };
        cx.simulate_click(conn_row, gpui::Modifiers::none());
        cx.run_until_parked();

        let duplicate_button = debug_center(&mut cx, "selection-duplicate");
        cx.simulate_click(duplicate_button, gpui::Modifiers::none());
        cx.run_until_parked();
        panel.read_with(&cx, |panel, cx| {
            let labels: Vec<String> = panel
                .store
                .read(cx)
                .connections()
                .iter()
                .map(|c| c.config.label.clone())
                .collect();
            assert!(
                labels.iter().any(|label| label.ends_with("(copy)")),
                "the Duplicate button must duplicate the selected connection once one is selected, got {labels:?}"
            );
        });
    }

    #[gpui::test]
    async fn selection_action_bar_targets_the_selected_connection_not_another_one(
        cx: &mut TestAppContext,
    ) {
        let config_a = db_client::ConnectionConfig {
            label: "bar-target-a".to_string(),
            auto_connect: false,
            ..Default::default()
        };
        let config_b = db_client::ConnectionConfig {
            label: "bar-target-b".to_string(),
            auto_connect: false,
            ..Default::default()
        };
        let id_b = config_b.id;

        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs.clone(), [], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        let panel = workspace
            .update_in(&mut cx, |_, window, cx| {
                cx.spawn_in(
                    window,
                    async move |workspace_handle, cx: &mut AsyncWindowContext| {
                        DatabasePanel::load(workspace_handle, cx.clone()).await
                    },
                )
            })
            .await
            .expect("DatabasePanel::load must succeed");
        workspace.update_in(&mut cx, |workspace, window, cx| {
            workspace.add_panel(panel.clone(), window, cx);
        });
        panel.update(&mut cx, |panel, cx| {
            panel.store.update(cx, |store, cx| {
                store.add_connection(config_a, cx);
                store.add_connection(config_b, cx);
            });
            panel.collapsed_folders.clear();
            panel.collapsed_connections.clear();
        });
        cx.run_until_parked();
        workspace.update_in(&mut cx, |workspace, window, cx| {
            workspace.toggle_panel_focus::<DatabasePanel>(window, cx);
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        cx.run_until_parked();

        let conn_b_bounds = cx
            .debug_bounds(format!("conn-header-{id_b}").leak())
            .expect("expected debug bounds for conn-header of B");
        let conn_b_row = gpui::Point {
            x: conn_b_bounds.origin.x + px(20.),
            y: conn_b_bounds.center().y,
        };
        cx.simulate_click(conn_b_row, gpui::Modifiers::none());
        cx.run_until_parked();
        panel.read_with(&cx, |panel, _| {
            assert_eq!(
                panel.selected_entity,
                Some(SelectedEntity::Connection(id_b))
            );
        });

        // Uses Duplicate rather than Connect: Connect spawns a real network
        // task on the shared Tokio runtime (see `runtime.rs`) that outlives
        // this test's teardown against an unreachable default host/port,
        // which aborts the process. Duplicate exercises the same
        // selected-connection targeting synchronously.
        let duplicate_button = debug_center(&mut cx, "selection-duplicate");
        cx.simulate_click(duplicate_button, gpui::Modifiers::none());
        cx.run_until_parked();

        panel.read_with(&cx, |panel, cx| {
            let labels: Vec<String> = panel
                .store
                .read(cx)
                .connections()
                .iter()
                .map(|c| c.config.label.clone())
                .collect();
            assert!(
                labels.contains(&"bar-target-b (copy)".to_string()),
                "duplicating must target the selected connection B, got {labels:?}"
            );
            assert!(
                !labels.contains(&"bar-target-a (copy)".to_string()),
                "the unselected connection A must be left untouched, got {labels:?}"
            );
        });
    }

    #[gpui::test]
    async fn the_connection_row_is_a_single_compact_line_not_a_stacked_label_and_caption(
        cx: &mut TestAppContext,
    ) {
        let config = db_client::ConnectionConfig {
            label: "compact-row-test".to_string(),
            auto_connect: false,
            ..Default::default()
        };
        let connection_id = config.id;

        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs.clone(), [], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        let panel = workspace
            .update_in(&mut cx, |_, window, cx| {
                cx.spawn_in(
                    window,
                    async move |workspace_handle, cx: &mut AsyncWindowContext| {
                        DatabasePanel::load(workspace_handle, cx.clone()).await
                    },
                )
            })
            .await
            .expect("DatabasePanel::load must succeed");
        workspace.update_in(&mut cx, |workspace, window, cx| {
            workspace.add_panel(panel.clone(), window, cx);
        });
        let folder_id = panel.update(&mut cx, |panel, cx| {
            let folder_id = panel
                .store
                .update(cx, |store, cx| {
                    store.add_folder("reference-folder".into(), None, cx)
                })
                .expect("add_folder must succeed for a top-level folder");
            panel.store.update(cx, |store, cx| {
                store.add_connection(config, cx);
            });
            panel.collapsed_folders.clear();
            panel.collapsed_connections.clear();
            folder_id
        });
        cx.run_until_parked();
        workspace.update_in(&mut cx, |workspace, window, cx| {
            workspace.toggle_panel_focus::<DatabasePanel>(window, cx);
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        cx.run_until_parked();

        // The folder row is an established single line (chevron + folder icon +
        // label). A connection row that still stacks its driver caption under
        // the label would be noticeably taller than this baseline; a genuinely
        // single-line row (icon + label + inline driver caption) stays close to
        // it, regardless of the exact pixel values the current theme/font
        // resolves to.
        let folder_bounds = cx
            .debug_bounds(format!("folder-row-{folder_id}").leak())
            .expect("expected debug bounds for the reference folder row");
        let conn_bounds = cx
            .debug_bounds(format!("conn-header-{connection_id}").leak())
            .expect("expected debug bounds for conn-header");

        assert!(
            f32::from(conn_bounds.size.height) <= f32::from(folder_bounds.size.height) * 1.5,
            "connection row height {:?} must stay close to the single-line folder row height {:?} \
             (a two-line stacked label+caption row would be roughly 2x taller)",
            conn_bounds.size.height,
            folder_bounds.size.height,
        );
    }

    #[gpui::test]
    async fn clicking_the_exec_button_opens_the_exec_dialog_for_the_selected_connection(
        cx: &mut TestAppContext,
    ) {
        let config = db_client::ConnectionConfig {
            label: "exec-ui-test".to_string(),
            auto_connect: false,
            ..Default::default()
        };
        let connection_id = config.id;
        let (panel, mut cx) = load_connected_panel(cx, config).await;
        let workspace = panel
            .read_with(&cx, |panel, _| panel.workspace.upgrade())
            .expect("workspace handle must be live");

        workspace.update_in(&mut cx, |workspace, window, cx| {
            workspace.toggle_panel_focus::<DatabasePanel>(window, cx);
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        cx.run_until_parked();

        let conn_bounds = cx
            .debug_bounds(format!("conn-header-{connection_id}").leak())
            .expect("expected debug bounds for conn-header");
        let conn_row = gpui::Point {
            x: conn_bounds.origin.x + px(20.),
            y: conn_bounds.center().y,
        };
        cx.simulate_click(conn_row, gpui::Modifiers::none());
        cx.run_until_parked();
        panel.read_with(&cx, |panel, _| {
            assert_eq!(
                panel.selected_entity,
                Some(SelectedEntity::Connection(connection_id)),
                "the connection row must be selected before the Exec button is enabled"
            );
        });

        let exec_button = debug_center(&mut cx, "selection-exec");
        cx.simulate_click(exec_button, gpui::Modifiers::none());
        cx.run_until_parked();

        workspace.update(&mut cx, |workspace, cx| {
            assert!(
                workspace
                    .active_modal::<crate::sql_exec::ExecDialog>(cx)
                    .is_some(),
                "clicking the Exec button must open the ExecDialog modal"
            );
        });
    }

    #[gpui::test]
    async fn connection_right_click_opens_connection_menu(cx: &mut TestAppContext) {
        let config = db_client::ConnectionConfig {
            label: "ctx-conn".to_string(),
            auto_connect: false,
            ..Default::default()
        };
        let connection_id = config.id;
        let (panel, mut cx) = load_connected_panel(cx, config).await;

        panel.read_with(&cx, |panel, _| {
            assert!(
                panel.context_menu.is_none(),
                "no context menu is open before right-clicking"
            );
        });

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.deploy_connection_context_menu(connection_id, gpui::Point::default(), window, cx);
        });

        panel.read_with(&cx, |panel, _| {
            assert!(
                panel.context_menu.is_some(),
                "right-clicking a connection opens its actions menu"
            );
        });
    }

    #[test]
    fn extract_parameters_skips_string_literals_and_casts() {
        let sql = "SELECT * FROM t WHERE id = :id AND name = ':notparam' AND x = value::int AND y = ?limit";
        let params = extract_query_parameters(sql);
        assert_eq!(params, vec!["id".to_string(), "limit".to_string()]);
    }

    #[test]
    fn extract_parameters_dedupes_in_first_seen_order() {
        let sql = "SELECT :b, :a, :b, :a";
        assert_eq!(
            extract_query_parameters(sql),
            vec!["b".to_string(), "a".to_string()]
        );
    }

    #[test]
    fn substitute_parameters_quotes_strings_and_keeps_numbers() {
        let mut values = HashMap::default();
        values.insert("name".to_string(), "O'Brien".to_string());
        values.insert("age".to_string(), "42".to_string());
        let sql = "SELECT * FROM t WHERE name = :name AND age > :age";
        assert_eq!(
            substitute_query_parameters(sql, &values),
            "SELECT * FROM t WHERE name = 'O''Brien' AND age > 42"
        );
    }

    #[test]
    fn substitute_parameters_preserves_casts_and_missing_values() {
        let values = HashMap::default();
        let sql = "SELECT x::int, :missing FROM t";
        assert_eq!(
            substitute_query_parameters(sql, &values),
            "SELECT x::int, :missing FROM t"
        );
    }

    #[test]
    fn substitute_parameter_null_is_inserted_verbatim() {
        let mut values = HashMap::default();
        values.insert("v".to_string(), "null".to_string());
        assert_eq!(
            substitute_query_parameters("SELECT :v", &values),
            "SELECT NULL"
        );
    }

    #[gpui::test]
    async fn open_modify_table_opens_workspace_modal(cx: &mut TestAppContext) {
        let config = db_client::ConnectionConfig {
            label: "modify".to_string(),
            auto_connect: false,
            ..Default::default()
        };
        let connection_id = config.id;
        let (panel, mut cx) = load_connected_panel(cx, config).await;
        let workspace = panel
            .read_with(&cx, |panel, _| panel.workspace.upgrade())
            .expect("workspace handle must be live");

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.open_modify_table(
                connection_id,
                "public".to_string(),
                "users".to_string(),
                window,
                cx,
            );
        });
        cx.run_until_parked();

        let has_modal = workspace.read_with(&cx, |workspace, cx| {
            workspace.active_modal::<ModifyTableView>(cx).is_some()
        });
        assert!(
            has_modal,
            "Modify Table must open as a workspace modal, not an in-panel overlay"
        );
    }

    #[gpui::test]
    async fn open_data_import_opens_workspace_modal(cx: &mut TestAppContext) {
        let config = db_client::ConnectionConfig {
            label: "import".to_string(),
            auto_connect: false,
            ..Default::default()
        };
        let connection_id = config.id;
        let (panel, mut cx) = load_connected_panel(cx, config).await;
        let workspace = panel
            .read_with(&cx, |panel, _| panel.workspace.upgrade())
            .expect("workspace handle must be live");

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.open_data_import(
                connection_id,
                "public".to_string(),
                "users".to_string(),
                window,
                cx,
            );
        });
        cx.run_until_parked();

        let has_modal = workspace.read_with(&cx, |workspace, cx| {
            workspace.active_modal::<ImportDataView>(cx).is_some()
        });
        assert!(
            has_modal,
            "Import Data must open as a workspace modal, not an in-panel overlay"
        );
    }

    #[gpui::test]
    async fn open_ddl_source_opens_workspace_tab(cx: &mut TestAppContext) {
        let config = db_client::ConnectionConfig {
            label: "ddl".to_string(),
            auto_connect: false,
            ..Default::default()
        };
        let (panel, mut cx) = load_connected_panel(cx, config).await;
        let workspace = panel
            .read_with(&cx, |panel, _| panel.workspace.upgrade())
            .expect("workspace handle must be live");

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.open_ddl_source(window, cx);
        });
        cx.run_until_parked();

        let tabs = workspace.read_with(&cx, |workspace, cx| {
            workspace
                .active_pane()
                .read(cx)
                .items_of_type::<DdlSourceView>()
                .count()
        });
        assert_eq!(tabs, 1, "DDL Source must open as a tab in the active pane");
    }

    struct GoToObjectSchemaProvider;

    #[async_trait::async_trait]
    impl db_client::DbProvider for GoToObjectSchemaProvider {
        async fn ping(&self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn list_databases(&self) -> anyhow::Result<Vec<db_client::DatabaseInfo>> {
            Ok(vec![db_client::DatabaseInfo {
                name: "shop".into(),
            }])
        }
        async fn list_tables(&self, _database: &str) -> anyhow::Result<Vec<db_client::TableInfo>> {
            Ok(vec![db_client::TableInfo {
                name: "users".into(),
                kind: db_client::schema::TableKind::Table,
            }])
        }
        async fn describe_table(
            &self,
            _database: &str,
            _table: &str,
        ) -> anyhow::Result<Vec<db_client::ColumnInfo>> {
            Ok(vec![db_client::ColumnInfo {
                name: "id".into(),
                data_type: "int".into(),
                is_nullable: false,
                column_key: Some("PRI".into()),
                default_value: None,
                extra: String::new(),
            }])
        }
        async fn execute_query(
            &self,
            _database: &str,
            _sql: &str,
        ) -> anyhow::Result<db_client::schema::QueryResult> {
            Ok(db_client::schema::QueryResult {
                columns: vec!["id".to_string()],
                rows: vec![vec![Some("1".to_string())]],
                rows_affected: 1,
                execution_time_ms: 0,
            })
        }
        async fn get_table_ddl(&self, _database: &str, _table: &str) -> anyhow::Result<String> {
            Ok("TABLE_DDL".to_string())
        }
        async fn get_database_ddl(&self, _database: &str) -> anyhow::Result<String> {
            Ok("DATABASE_DDL".to_string())
        }
    }

    // Real end-to-end proof of the go-to-object palette (Ctrl+N ->
    // database_panel::GoToObject): dispatches the real action exactly as the
    // real keymap resolves it, types a real fuzzy query into the picker,
    // confirms with a real Enter keystroke, and asserts a ResultView tab for
    // the matched table opened in the active pane -- not an internal method
    // call standing in for the interaction.
    #[gpui::test]
    async fn go_to_object_action_opens_the_matched_table_as_a_tab(cx: &mut TestAppContext) {
        init_test(cx);
        // The picker crate's query editor goes through ui_input's type-erased
        // editor factory, which only `editor::init` registers -- db_client_ui's
        // own cell editors construct `editor::Editor` directly and never hit it.
        cx.update(|cx| editor::init(cx));
        // This test harness loads no production keymap, so mirror the real
        // global (unscoped) "enter" -> menu::Confirm binding the picker relies
        // on (assets/keymaps/default-linux.json has no "context" on that block).
        cx.update(|cx| {
            cx.bind_keys([gpui::KeyBinding::new("enter", menu::Confirm, None)]);
        });
        let config = db_client::ConnectionConfig {
            label: "go-to-object".to_string(),
            auto_connect: false,
            ..Default::default()
        };
        let connection_id = config.id;
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs.clone(), [], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        let panel = workspace
            .update_in(&mut cx, |_, window, cx| {
                cx.spawn_in(
                    window,
                    async move |workspace_handle, cx: &mut AsyncWindowContext| {
                        DatabasePanel::load(workspace_handle, cx.clone()).await
                    },
                )
            })
            .await
            .expect("DatabasePanel::load must succeed");
        workspace.update_in(&mut cx, |workspace, window, cx| {
            workspace.add_panel(panel.clone(), window, cx);
        });
        panel.update(&mut cx, |panel, cx| {
            panel.store.update(cx, |store, cx| {
                store.add_connected_for_test(
                    config,
                    std::sync::Arc::new(GoToObjectSchemaProvider),
                    cx,
                );
                store.prefetch_full_schema(connection_id, cx).detach();
            });
        });
        cx.run_until_parked();

        workspace.update_in(&mut cx, |workspace, window, cx| {
            workspace.toggle_panel_focus::<DatabasePanel>(window, cx);
        });
        cx.run_until_parked();

        cx.dispatch_action(GoToObject);
        cx.run_until_parked();

        let has_modal = workspace.read_with(&cx, |workspace, cx| {
            workspace.active_modal::<GoToObjectPalette>(cx).is_some()
        });
        assert!(
            has_modal,
            "Ctrl+N (database_panel::GoToObject) must open the go-to-object palette as a modal"
        );

        cx.simulate_keystrokes("u s e r s");
        cx.run_until_parked();
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();

        let tabs = workspace.read_with(&cx, |workspace, cx| {
            workspace
                .active_pane()
                .read(cx)
                .items_of_type::<ResultView>()
                .count()
        });
        assert_eq!(
            tabs, 1,
            "confirming the 'users' match must open its data grid as a tab"
        );
        let tab_title = workspace.read_with(&cx, |workspace, cx| {
            use workspace::Item;
            workspace
                .active_pane()
                .read(cx)
                .items_of_type::<ResultView>()
                .next()
                .map(|view| view.read(cx).tab_content_text(0, cx))
        });
        assert_eq!(tab_title, Some(SharedString::from("users")));

        let has_modal_after_confirm = workspace.read_with(&cx, |workspace, cx| {
            workspace.active_modal::<GoToObjectPalette>(cx).is_some()
        });
        assert!(
            !has_modal_after_confirm,
            "confirming a match must dismiss the palette"
        );
    }

    #[gpui::test]
    async fn open_query_params_prompt_lists_detected_parameters(cx: &mut TestAppContext) {
        let config = db_client::ConnectionConfig {
            label: "params".to_string(),
            auto_connect: false,
            ..Default::default()
        };
        let connection_id = config.id;
        let (panel, mut cx) = load_connected_panel(cx, config).await;

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.open_query_params_prompt(
                connection_id,
                "SELECT * FROM t WHERE id = :id AND name = :name".to_string(),
                window,
                cx,
            );
        });

        let workspace = panel.read_with(&cx, |panel, _| panel.workspace.clone());
        workspace
            .update(&mut cx, |workspace, cx| {
                let view = workspace
                    .active_modal::<QueryParamsView>(cx)
                    .expect("params prompt must open as a workspace modal");
                let names: Vec<_> = view
                    .read(cx)
                    .inputs
                    .iter()
                    .map(|(name, _)| name.clone())
                    .collect();
                assert_eq!(names, vec!["id".to_string(), "name".to_string()]);
            })
            .unwrap();
    }

    // Real end-to-end proof of table rename with find-usages: two real console
    // editors are open, one referencing the table being renamed and one not.
    // Confirming the real "Rename" button must (a) send the correct RENAME
    // statement to the connection, (b) rewrite the referencing console's
    // buffer in place, and (c) leave the unrelated console untouched.
    #[gpui::test]
    async fn rename_table_dialog_rewrites_referencing_consoles_only(cx: &mut TestAppContext) {
        let config = db_client::ConnectionConfig {
            label: "rename".to_string(),
            auto_connect: false,
            ..Default::default()
        };
        let connection_id = config.id;
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs.clone(), [], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        let store = workspace.update_in(cx, |workspace, window, cx| {
            let store = cx.new(DatabaseStore::new);
            let focus_handle = cx.focus_handle();
            let workspace_handle = workspace.weak_handle();
            let table_filter_editor = cx.new(|cx| Editor::single_line(window, cx));
            let panel = cx.new(|cx| {
                let sub = cx.subscribe(
                    &store,
                    |_: &mut DatabasePanel,
                     _: Entity<DatabaseStore>,
                     _: &DatabaseStoreEvent,
                     cx: &mut Context<DatabasePanel>| {
                        cx.notify();
                    },
                );
                DatabasePanel {
                    focus_handle,
                    store: store.clone(),
                    workspace: workspace_handle,
                    history_expanded: false,
                    table_filter_editor,
                    collapsed_folders: HashSet::default(),
                    collapsed_connections: HashSet::default(),
                    editing_folder: None,
                    drag_target: None,
                    views_expanded: HashSet::default(),
                    procedures_expanded: HashSet::default(),
                    sequences_expanded: HashSet::default(),
                    events_expanded: HashSet::default(),
                    table_indexes_expanded: HashSet::default(),
                    table_fks_expanded: HashSet::default(),
                    table_triggers_expanded: HashSet::default(),
                    server_objects_expanded: HashSet::default(),
                    server_users: HashMap::default(),
                    table_filter_is_regex: false,
                    selected_tree_node: None,
                    selected_entity: None,
                    initial_collapse_pending: false,
                    pending_tree_state_serialization: Task::ready(None),
                    dump: DumpUiState::default(),
                    export: ExportUiState::default(),
                    context_menu: None,
                    tree_scroll_handle: ScrollHandle::new(),
                    _subscriptions: vec![sub],
                }
            });
            workspace.add_panel(panel, window, cx);
            store
        });
        store.update(cx, |store, cx| {
            store.add_connected_for_test(
                config,
                std::sync::Arc::new(RecordingMockProvider {
                    calls: calls.clone(),
                }),
                cx,
            );
        });
        cx.run_until_parked();

        let referencing_sql = "SELECT * FROM orders WHERE id = 1;";
        let referencing_editor = workspace.update_in(cx, |workspace, window, cx| {
            let buffer = cx.new(|cx| language::Buffer::local(referencing_sql, cx));
            let multi = cx.new(|cx| MultiBuffer::singleton(buffer, cx));
            let editor = cx.new(|cx| {
                let mut editor = Editor::for_multibuffer(multi, None, window, cx);
                editor.register_addon(DbQueryEditorAddon::new(connection_id));
                editor
            });
            workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
            editor
        });
        let unrelated_sql = "SELECT * FROM customers;";
        let unrelated_editor = workspace.update_in(cx, |workspace, window, cx| {
            let buffer = cx.new(|cx| language::Buffer::local(unrelated_sql, cx));
            let multi = cx.new(|cx| MultiBuffer::singleton(buffer, cx));
            let editor = cx.new(|cx| {
                let mut editor = Editor::for_multibuffer(multi, None, window, cx);
                editor.register_addon(DbQueryEditorAddon::new(connection_id));
                editor
            });
            workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
            editor
        });
        cx.run_until_parked();

        let panel = workspace
            .read_with(cx, |workspace, cx| workspace.panel::<DatabasePanel>(cx))
            .expect("panel must be registered");
        panel.update_in(cx, |panel, window, cx| {
            panel.open_rename_table_dialog(
                connection_id,
                "shop".to_string(),
                "orders".to_string(),
                window,
                cx,
            );
        });
        cx.run_until_parked();

        let usage_count = workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_modal::<RenameTableView>(cx)
                .expect("rename dialog must open as a workspace modal")
                .read(cx)
                .usages
                .len()
        });
        assert_eq!(
            usage_count, 1,
            "only the referencing console must show up in the usage preview"
        );

        let new_name_editor = workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_modal::<RenameTableView>(cx)
                .expect("rename dialog must still be open")
                .read(cx)
                .new_name_editor
                .clone()
        });
        new_name_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("purchases", window, cx);
        });

        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        cx.run_until_parked();
        let confirm_point = debug_center(cx, "rename-table-confirm");
        cx.simulate_event(gpui::MouseDownEvent {
            position: confirm_point,
            button: gpui::MouseButton::Left,
            modifiers: gpui::Modifiers::none(),
            click_count: 1,
            first_mouse: false,
        });
        cx.simulate_event(gpui::MouseUpEvent {
            position: confirm_point,
            button: gpui::MouseButton::Left,
            modifiers: gpui::Modifiers::none(),
            click_count: 1,
        });
        cx.run_until_parked();

        assert_eq!(
            calls
                .lock()
                .expect("mock call log should not be poisoned")
                .as_slice(),
            &["ALTER TABLE orders RENAME TO purchases".to_string()],
            "a real confirm click must send the rename statement to the connection"
        );
        let referencing_text = referencing_editor.read_with(cx, |editor, cx| editor.text(cx));
        assert_eq!(
            referencing_text, "SELECT * FROM purchases WHERE id = 1;",
            "the referencing console's buffer must be rewritten in place"
        );
        let unrelated_text = unrelated_editor.read_with(cx, |editor, cx| editor.text(cx));
        assert_eq!(
            unrelated_text, unrelated_sql,
            "a console that never referenced the table must be left untouched"
        );
        let has_modal_after_confirm = workspace.read_with(cx, |workspace, cx| {
            workspace.active_modal::<RenameTableView>(cx).is_some()
        });
        assert!(
            !has_modal_after_confirm,
            "confirming must dismiss the dialog"
        );
    }
}
