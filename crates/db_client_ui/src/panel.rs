use crate::compare_data::CompareDataView;
use crate::connection_view::ConnectionView;
use crate::data_import::ImportDataView;
use crate::ddl_source::DdlSourceView;
use crate::driver_icon::brand_icon;
use crate::erd_diagram::{ErdColumn, ErdRelationship, ErdTable, ErdView};
use crate::explain_plan::{
    ExplainPlanView, PlanNode, explain_sql_for_driver, parse_plan_tree, plan_text_from_result,
};
use crate::modify_table::ModifyTableView;
use crate::native_dump::{
    DumpRequest, DumpRunCallback, DumpStatus, DumpTask, NativeDumpDialog, apply_substitutions,
    render_dump_status_row, spawn_dump,
};
use crate::result_view::ResultView;
use crate::sql_completion_provider::install_on_editor;
use crate::store::{ActiveConnection, ConnectionStatus, DatabaseStore, DatabaseStoreEvent};
use collections::{HashMap, HashSet};
use db_client::{
    ConnectionConfig, ConnectionId, DatabaseDriver, Folder, FolderId, ProcedureKind, QueryResult,
    schema::ColumnInfo,
};
use editor::{Editor, EditorEvent, GotoDefinitionKind, SemanticsProvider, ToOffset};
use futures::future::Shared;
use gpui::{
    AnyElement, App, AsyncWindowContext, ClickEvent, Context, DismissEvent, DragMoveEvent,
    ElementId, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement, IntoElement,
    MouseButton, MouseDownEvent, ParentElement, Pixels, Point, PromptLevel, Render, ScrollHandle,
    SharedString, StatefulInteractiveElement, Styled, Subscription, Task, WeakEntity, Window,
    anchored, deferred, div, px,
};
use language::{Anchor, Buffer, BufferId, BufferRow};
use multi_buffer::MultiBuffer;
use project::{
    DocumentHighlight, InlayHint, InvalidationStrategy, Location, LocationLink, ProjectTransaction,
    lsp_store::{BufferSemanticTokens, CacheInlayHints, RefreshForServer},
};
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use terminal_view::terminal_panel::TerminalPanel;
use time::OffsetDateTime;
use time::macros::format_description;
use ui::{
    CommonAnimationExt, ContextMenu, HighlightedLabel, Icon, IconButton, IconName,
    IconSize, Indicator, Label, LabelSize, ScrollAxes, Scrollbars, Tooltip, WithScrollbar,
    prelude::*, right_click_menu,
};
use util::ResultExt as _;
use workspace::{
    Event as WorkspaceEvent, ItemHandle, ModalView, OpenOptions, OpenVisible, Pane, Toast,
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    notifications::NotificationId,
};
use zed_actions::database_panel::{GoToDdl, QuickDocumentation, ShowDiagram, ToggleFocus};

const DATABASE_PANEL_KEY: &str = "DatabasePanel";
const ERD_TABLE_LIMIT: usize = 50;

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

pub(crate) fn init(_cx: &mut App) {
    // Workspace action handlers (ToggleFocus, NewQuery, RunQuery) are registered
    // in zed::register_actions, which runs reliably for every workspace. An
    // observe_new here did not fire for the app's workspaces, so RunQuery had no
    // reachable handler and Ctrl+Enter fell through to the inline assistant.
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
    let statement_start = text[..cursor]
        .rfind(';')
        .map(|index| index + 1)
        .unwrap_or(0);
    let statement_end = text[cursor..]
        .find(';')
        .map(|index| cursor + index)
        .unwrap_or(text.len());
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
    let statement_start = text[..cursor]
        .rfind(';')
        .map(|index| index + 1)
        .unwrap_or(0);
    let statement_end = text[cursor..]
        .find(';')
        .map(|index| cursor + index)
        .unwrap_or(text.len());
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

        if let Some(table_reference) = statement_table_reference_at_offset(&text, offset)
            .or_else(|| table_reference_at_offset(&text, offset))
        {
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
                cx,
                move |store, cx| store.get_database_ddl(connection_id, database, cx),
            ));
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

impl DbSemanticsProvider {
    fn spawn_ddl_navigation(
        &self,
        source_buffer: Entity<Buffer>,
        origin_range: std::ops::Range<Anchor>,
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

            let ddl_buffer = workspace.update(cx, |ws, cx| {
                ws.project().update(cx, |project, cx| {
                    project.create_local_buffer(&ddl, language, false, cx)
                })
            })?;

            let target_anchor = ddl_buffer.read_with(cx, |buf, _| buf.anchor_before(0));

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

// Returns the `;`-delimited SQL statement that contains the byte offset
// `cursor`, trimmed. `;` is ASCII so byte scanning stays on char boundaries.
#[cfg(test)]
fn statement_at_cursor(text: &str, cursor: usize) -> String {
    let cursor = cursor.min(text.len());
    let start = text[..cursor].rfind(';').map(|i| i + 1).unwrap_or(0);
    let end = text[cursor..]
        .find(';')
        .map(|i| cursor + i)
        .unwrap_or(text.len());
    text[start..end].trim().to_string()
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SqlStatementRun {
    sql: String,
    start_row: u32,
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
    let cursor = cursor.min(text.len());
    let start = text[..cursor].rfind(';').map(|i| i + 1).unwrap_or(0);
    let end = text[cursor..]
        .find(';')
        .map(|i| cursor + i)
        .unwrap_or(text.len());
    trim_sql_range(text, start..end)
}

fn statement_runs_in_range(text: &str, range: Range<usize>) -> Vec<SqlStatementRun> {
    let Some(range) = trim_sql_range(text, range) else {
        return Vec::new();
    };
    let mut statements = Vec::new();
    let mut start = range.start;
    let Some(selected_text) = text.get(range.clone()) else {
        return Vec::new();
    };
    for relative_semicolon in selected_text.match_indices(';').map(|(ix, _)| ix) {
        let end = range.start + relative_semicolon;
        if let Some(trimmed) = trim_sql_range(text, start..end)
            && let Some(sql) = text.get(trimmed.clone())
        {
            statements.push(SqlStatementRun {
                sql: sql.to_string(),
                start_row: row_for_byte_offset(text, trimmed.start),
            });
        }
        start = end + 1;
    }
    if let Some(trimmed) = trim_sql_range(text, start..range.end)
        && let Some(sql) = text.get(trimmed.clone())
    {
        statements.push(SqlStatementRun {
            sql: sql.to_string(),
            start_row: row_for_byte_offset(text, trimmed.start),
        });
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

// Builds the nested tree under `parent_id`: child folders first (sorted by
// order, then name), then connections (sorted by order, then label). Pure so
// the grouping/sorting is unit-tested without the GPUI render path. `depth`
// bounds recursion so cyclic stored data cannot loop forever.
fn build_folder_tree(
    folders: &[Folder],
    connections: &[ActiveConnection],
    parent_id: Option<FolderId>,
    depth: usize,
) -> Vec<TreeNode> {
    if depth > db_client::MAX_FOLDER_DEPTH {
        return Vec::new();
    }
    let mut nodes: Vec<TreeNode> = Vec::new();

    let mut child_folders: Vec<&Folder> = folders
        .iter()
        .filter(|f| f.parent_id == parent_id)
        .collect();
    child_folders.sort_by(|a, b| {
        a.order
            .cmp(&b.order)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    for folder in child_folders {
        nodes.push(TreeNode::Folder {
            folder: folder.clone(),
            children: build_folder_tree(folders, connections, Some(folder.id), depth + 1),
        });
    }

    let mut child_connections: Vec<usize> = connections
        .iter()
        .enumerate()
        .filter(|(_, c)| c.config.folder_id == parent_id)
        .map(|(index, _)| index)
        .collect();
    child_connections.sort_by(|a, b| {
        let left = &connections[*a].config;
        let right = &connections[*b].config;
        left.order
            .cmp(&right.order)
            .then_with(|| left.label.to_lowercase().cmp(&right.label.to_lowercase()))
            .then_with(|| left.id.cmp(&right.id))
    });
    for index in child_connections {
        nodes.push(TreeNode::Connection { index });
    }

    nodes
}

// A persistent .sql scratch file per connection, kept in the config dir so it
// survives restarts and never needs an explicit save.
fn connection_query_path(connection_id: ConnectionId, label: &str) -> std::path::PathBuf {
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
        .join(format!("{sanitized}-{short}.sql"))
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

    editor.update(cx, |editor, cx| {
        if editor.addon::<DbQueryEditorAddon>().is_none() {
            editor.register_addon(DbQueryEditorAddon::new(connection_id));
        }
        editor.set_show_runnables(true, cx);
        editor.set_semantics_provider(Some(Rc::new(DbSemanticsProvider {
            connection_id,
            store,
            workspace,
        })));
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
    let store = DatabaseStore::global(cx).map(|store| store.downgrade());
    let workspace_handle = workspace.weak_handle();
    let path = connection_query_path(connection_id, &connection_label);
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
            .update_in(cx, |_workspace, _window, cx| {
                let Some(editor) = item.act_as::<Editor>(cx) else {
                    return;
                };
                editor.update(cx, |editor, cx| {
                    editor.register_addon(DbQueryEditorAddon::new(connection_id));
                    editor.set_show_runnables(true, cx);
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
            .map(|c| (c.config.id, c.config.label.clone()))
    };
    if let Some((id, label)) = connection {
        open_new_sql_query(workspace, id, label, window, cx);
    }
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
            editor.update(cx, |editor, cx| {
                if let Some(addon) = editor.addon_mut::<DbQueryEditorAddon>() {
                    addon.mark_query(statement.start_row, QueryExecutionStatus::Running);
                    cx.notify();
                }
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
                    result_view.update(cx, |view, cx| {
                        view.set_error(format!("Could not connect to '{conn_label}': {err}"), cx);
                    });
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
                    result_view.update(cx, |view, cx| view.set_error(err.to_string(), cx));
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
    table_indexes_expanded: HashSet<(ConnectionId, String, String)>,
    table_fks_expanded: HashSet<(ConnectionId, String, String)>,
    table_triggers_expanded: HashSet<(ConnectionId, String, String)>,
    server_objects_expanded: HashSet<ConnectionId>,
    server_users: HashMap<ConnectionId, Vec<(String, String)>>,
    table_filter_is_regex: bool,
    selected_tree_node: Option<SelectedTreeNode>,
    dump: DumpUiState,
    context_menu: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
    tree_scroll_handle: ScrollHandle,
    _subscriptions: Vec<Subscription>,
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
/// pointer is over empty panel space.
#[derive(Clone, Copy, PartialEq)]
enum DropTarget {
    Folder(FolderId),
    TopLevel,
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
        let rows: Vec<_> = self
            .columns
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
                    .children(
                        overlays
                            .into_iter()
                            .map(|(icon, color)| Icon::new(icon).size(IconSize::XSmall).color(color)),
                    )
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
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
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
            cx.new(|cx| {
                let store_subscription = cx.subscribe(
                    &store,
                    |_this: &mut DatabasePanel,
                     _store: Entity<DatabaseStore>,
                     _event: &DatabaseStoreEvent,
                     cx: &mut Context<DatabasePanel>| {
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
                    table_indexes_expanded: HashSet::default(),
                    table_fks_expanded: HashSet::default(),
                    table_triggers_expanded: HashSet::default(),
                    server_objects_expanded: HashSet::default(),
                    server_users: HashMap::default(),
                    table_filter_is_regex: false,
                    selected_tree_node: None,
                    dump: DumpUiState::default(),
                    context_menu: None,
                    tree_scroll_handle: ScrollHandle::new(),
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
        cx.notify();
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

    /// Applies a drop of `item` onto `target`. Folders reparent (cycle and depth
    /// are guarded by the store); connections move into the target folder.
    fn handle_drop(&mut self, item: DraggedDbItem, target: DropTarget, cx: &mut Context<Self>) {
        let folder = match target {
            DropTarget::Folder(id) => Some(id),
            DropTarget::TopLevel => None,
        };
        self.drag_target = None;
        self.store.update(cx, |store, cx| match item {
            DraggedDbItem::Connection(id) => store.move_connection_to_folder(id, folder, cx),
            DraggedDbItem::Folder(id) => {
                store.move_folder(id, folder, cx);
            }
        });
        cx.notify();
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
        let is_drop_target = self.drag_target == Some(DropTarget::Folder(folder_id));
        let editing = self
            .editing_folder
            .as_ref()
            .filter(|editing| editing.id == folder_id)
            .map(|editing| editing.editor.clone());

        let mut row = h_flex()
            .id(ElementId::from(SharedString::from(format!(
                "folder-row-{folder_id}"
            ))))
            .w_full()
            .items_center()
            .gap_1()
            .py_1()
            .pr_2()
            .pl(px(8. + depth as f32 * 12.))
            .rounded_sm()
            .hover(|style| style.bg(cx.theme().colors().element_hover))
            .when(is_drop_target, |el| {
                el.bg(cx.theme().colors().drop_target_background)
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
                this.toggle_folder_collapsed(folder_id, cx);
            }))
            .on_drag(DraggedDbItem::Folder(folder_id), {
                let name = name.clone();
                move |_, _, _, cx| Self::drag_preview(name.clone().into(), IconName::Folder, cx)
            })
            .on_drag_move(
                cx.listener(move |this, event: &DragMoveEvent<DraggedDbItem>, _, cx| {
                    if event.bounds.contains(&event.event.position)
                        && this.drag_target != Some(DropTarget::Folder(folder_id))
                    {
                        this.drag_target = Some(DropTarget::Folder(folder_id));
                        cx.notify();
                    }
                }),
            )
            .on_drop(cx.listener(move |this, item: &DraggedDbItem, _, cx| {
                this.handle_drop(*item, DropTarget::Folder(folder_id), cx);
            }))
            .child(Label::new(name.clone()).size(LabelSize::Small));

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

    fn quote_ident(name: &str, driver: DatabaseDriver) -> String {
        match driver {
            DatabaseDriver::MySQL => format!("`{}`", name.replace('`', "``")),
            _ => format!("\"{}\"", name.replace('"', "\"\"")),
        }
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
        let store = self.store.clone();
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |_this, cx| {
            let columns = describe.await.unwrap_or_default();
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
                    NativeDumpDialog::new(driver, config, preset_databases, preset_tables, window, cx)
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

    fn open_ddl_source(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace
            .update(cx, |workspace, cx| {
                let view = cx.new(|cx| DdlSourceView::new(window, cx));
                workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
            })
            .log_err();
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
                    let view = cx.new(|cx| ExplainPlanView::new(roots, window, cx));
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
                    let view = cx
                        .new(|cx| CompareDataView::new(left, right, key_columns, title, window, cx));
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
            cx.notify();
            return;
        }
        self.server_objects_expanded.insert(id);
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
        let qt = Self::quote_ident(table, driver);
        if columns.is_empty() {
            return format!("INSERT INTO {} () VALUES ();", qt);
        }
        let cols: Vec<String> = columns
            .iter()
            .map(|c| Self::quote_ident(&c.name, driver))
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
        let qt = Self::quote_ident(table, driver);
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
            .map(|c| format!("{} = '{}'", Self::quote_ident(&c.name, driver), c.name))
            .collect();
        let where_clause = if let Some(pk_col) = pk {
            format!(
                "{} = '{}'",
                Self::quote_ident(&pk_col.name, driver),
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
        let qt = Self::quote_ident(table, driver);
        let insertable_cols: Vec<&ColumnInfo> = columns
            .iter()
            .filter(|c| c.extra != "auto_increment" && c.extra != "GENERATED ALWAYS")
            .collect();

        if insertable_cols.is_empty() {
            return format!("-- No insertable columns found for table {table}");
        }

        let col_list: Vec<String> = insertable_cols
            .iter()
            .map(|c| Self::quote_ident(&c.name, driver))
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
        let qt = Self::quote_ident(table, driver);
        let pk = columns
            .iter()
            .find(|c| c.column_key.as_deref() == Some("PRI"));
        let where_clause = if let Some(pk_col) = pk {
            format!(
                "{} = '{}'",
                Self::quote_ident(&pk_col.name, driver),
                pk_col.name
            )
        } else {
            "1 = 1".to_string()
        };
        format!("DELETE FROM {}\nWHERE {};", qt, where_clause)
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
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .px_2()
                    .py_1()
                    .gap_1()
                    .border_t_1()
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
        let indent = px(8. + depth as f32 * 12.);
        let connection_folder = conn.config.folder_id;
        let drag_label: SharedString = conn.config.label.clone().into();
        let label = conn.config.label.clone();
        let query_label = conn.config.label.clone();
        let driver_label = conn.config.driver.to_string();
        let driver = conn.config.driver;
        let config_for_edit = conn.config.clone();

        let status_color = match &conn.status {
            ConnectionStatus::Connected => Color::Success,
            ConnectionStatus::Connecting => Color::Modified,
            ConnectionStatus::Disconnected => Color::Muted,
            ConnectionStatus::Error(_) => Color::Error,
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
        let table_indexes = conn.table_indexes.clone();
        let table_fks = conn.table_fks.clone();
        let table_triggers = conn.table_triggers;
        let views_expanded = self.views_expanded.clone();
        let indexes_expanded = self.table_indexes_expanded.clone();
        let fks_expanded = self.table_fks_expanded.clone();
        let triggers_expanded = self.table_triggers_expanded.clone();

        div()
            .flex()
            .flex_col()
            .child(
                div()
                    .id(ElementId::from(SharedString::from(format!("conn-header-{}", id))))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .pr_2()
                    .pl(indent)
                    .py_1()
                    .rounded_sm()
                    .hover(|style| style.bg(cx.theme().colors().element_hover))
                    .when(is_active, |el| el.bg(cx.theme().colors().element_selected))
                    .when(is_connected, |el| {
                        el.cursor_pointer().on_click(cx.listener(move |this, _, _, cx| {
                            this.store.update(cx, |store, cx| {
                                store.set_active_connection(id, cx);
                            });
                        }))
                    })
                    .on_drag(DraggedDbItem::Connection(id), move |_, _, _, cx| {
                        Self::drag_preview(drag_label.clone(), IconName::DatabaseZap, cx)
                    })
                    .on_drop(cx.listener(move |this, item: &DraggedDbItem, _, cx| {
                        let target = match connection_folder {
                            Some(folder_id) => DropTarget::Folder(folder_id),
                            None => DropTarget::TopLevel,
                        };
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
                                            .child(Indicator::dot().color(status_color)),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .overflow_hidden()
                            .child(Label::new(label).size(LabelSize::Small))
                            .child(Label::new(driver_label).size(LabelSize::XSmall).color(Color::Muted)),
                    )
                    .child(
                        IconButton::new(SharedString::from(format!("new-query-{}", id)), IconName::File)
                            .icon_size(IconSize::XSmall)
                            .tooltip(Tooltip::text("New SQL Query"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let query_label = query_label.clone();
                                this.workspace
                                    .update(cx, |workspace, cx| {
                                        open_new_sql_query(workspace, id, query_label, window, cx);
                                    })
                                    .log_err();
                            })),
                    )
                    .when(!is_connected, |el| {
                        el.child(
                            IconButton::new(SharedString::from(format!("connect-{}", id)), IconName::PlayFilled)
                                .icon_size(IconSize::XSmall)
                                .tooltip(Tooltip::text("Connect"))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.store.update(cx, |store, cx| {
                                        store.connect(id, cx).detach_and_log_err(cx);
                                    });
                                })),
                        )
                    })
                    .when(is_connected, |el| {
                        el.child(
                            IconButton::new(SharedString::from(format!("refresh-{}", id)), IconName::RefreshTitle)
                                .icon_size(IconSize::XSmall)
                                .tooltip(Tooltip::text("Refresh"))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.store.update(cx, |store, cx| {
                                        store.refresh_schema_cache(id, cx).detach_and_log_err(cx);
                                    });
                                })),
                        )
                        .child(
                            IconButton::new(SharedString::from(format!("disconnect-{}", id)), IconName::Disconnected)
                                .icon_size(IconSize::XSmall)
                                .tooltip(Tooltip::text("Disconnect"))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.store.update(cx, |store, cx| {
                                        store.disconnect(id, cx);
                                    });
                                })),
                        )
                    })
                    .child(
                        IconButton::new(SharedString::from(format!("move-up-{}", id)), IconName::ChevronUp)
                            .icon_size(IconSize::XSmall)
                            .tooltip(Tooltip::text("Move Up"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.store.update(cx, |store, cx| {
                                    store.reorder_connection(id, -1, cx);
                                });
                            })),
                    )
                    .child(
                        IconButton::new(SharedString::from(format!("move-down-{}", id)), IconName::ChevronDown)
                            .icon_size(IconSize::XSmall)
                            .tooltip(Tooltip::text("Move Down"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.store.update(cx, |store, cx| {
                                    store.reorder_connection(id, 1, cx);
                                });
                            })),
                    )
                    .child(
                        IconButton::new(SharedString::from(format!("edit-conn-{}", id)), IconName::Pencil)
                            .icon_size(IconSize::XSmall)
                            .tooltip(Tooltip::text("Edit Connection"))
                            .on_click(cx.listener({
                                let config = config_for_edit;
                                move |this, _, window, cx| {
                                    this.open_edit_connection_modal(config.clone(), window, cx);
                                }
                            })),
                    )
                    .when(dump_menu_label(driver).is_some(), |row| {
                        row.child(
                            IconButton::new(
                                SharedString::from(format!("dump-conn-{}", id)),
                                IconName::Download,
                            )
                            .icon_size(IconSize::XSmall)
                            .tooltip(Tooltip::text(
                                dump_menu_label(driver).unwrap_or("Export with dump tool"),
                            ))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_dump_dialog(id, Vec::new(), Vec::new(), window, cx);
                            })),
                        )
                    })
                    .child(
                        IconButton::new(SharedString::from(format!("dup-conn-{}", id)), IconName::Copy)
                            .icon_size(IconSize::XSmall)
                            .tooltip(Tooltip::text("Duplicate Connection"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.store.update(cx, |store, cx| {
                                    store.duplicate_connection(id, cx);
                                });
                            })),
                    )
                    .child(
                        IconButton::new(SharedString::from(format!("delete-conn-{}", id)), IconName::Trash)
                            .icon_size(IconSize::XSmall)
                            .tooltip(Tooltip::text("Remove Connection"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.store.update(cx, |store, cx| {
                                    store.remove_connection(id, cx);
                                });
                            })),
                    ),
            )
            .when_some(error_message, |el, msg| {
                el.child(
                    div()
                        .px_4()
                        .py_1()
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
                                .pl(px(16.))
                                .pr_2()
                                .py_1()
                                .cursor_pointer()
                                .hover(|s| s.bg(gpui::transparent_white()))
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
                                    .pl(px(32.))
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
                                        .pl(px(48.))
                                        .pr_2()
                                        .py_1()
                                        .child(Label::new(name).size(LabelSize::XSmall))
                                        .child(
                                            Label::new(format!("@{host}"))
                                                .size(LabelSize::XSmall)
                                                .color(Color::Muted),
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
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_1()
                        .pl(px(16.))
                        .pr_2()
                        .py_1()
                        .cursor_pointer()
                        .hover(|s| s.bg(gpui::transparent_white()))
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
                                    .entry("New Query", None, {
                                        let entity = entity.clone();
                                        let db = db.clone();
                                        let workspace = workspace.clone();
                                        move |window, cx| {
                                            let sql = format!("SELECT * FROM {db} LIMIT 1;");
                                            entity.update(cx, |panel, cx| {
                                                Self::open_sql_query_with_text(workspace.clone(), panel.store.downgrade(), id, sql, window, cx);
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
                                                let result_view = cx.new(|cx| ResultView::new(title, cx));
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
                                                        Err(e) => view.set_error(e.to_string(), cx),
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
                                                let result_view = cx.new(|cx| ResultView::new(title, cx));
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
                                                        Err(e) => view.set_error(e.to_string(), cx),
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
                                        .pl(px(32.))
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
                                                .child(HighlightedLabel::new(table_name.clone(), highlight_indices).size(LabelSize::Small))
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
                                                        Self::quote_ident(&tbl, driver)
                                                    );
                                                    let store_weak = this.store.downgrade();
                                                    let task = this.store.update(cx, |store, cx| {
                                                        store.execute_query(id, db.clone(), sql, cx)
                                                    });
                                                    let title = SharedString::from(tbl.as_str());
                                                    let workspace = this.workspace.clone();
                                                    let result_view = cx.new(|cx| {
                                                        ResultView::new(title, cx)
                                                            .with_table_context(store_weak, id, db.clone(), tbl.clone(), window, cx)
                                                            .with_workspace(workspace.clone())
                                                    });
                                                    let rv = result_view.clone();
                                                    cx.spawn_in(window, async move |_, cx| {
                                                        let outcome = task.await;
                                                        rv.update(cx, |view, cx| match outcome {
                                                            Ok(r) => view.set_result(r, cx),
                                                            Err(e) => view.set_error(e.to_string(), cx),
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
                                                                    Self::quote_ident(&tbl, driver)
                                                                );
                                                                let store_weak = panel.store.downgrade();
                                                                let task = panel.store.update(cx, |store, cx| {
                                                                    store.execute_query(id, db.clone(), sql, cx)
                                                                });
                                                                let title = SharedString::from(tbl.as_str());
                                                                let ws = workspace.clone();
                                                                let result_view = cx.new(|cx| {
                                                                    ResultView::new(title, cx)
                                                                        .with_table_context(store_weak, id, db.clone(), tbl.clone(), window, cx)
                                                                        .with_workspace(workspace.clone())
                                                                });
                                                                let rv = result_view.clone();
                                                                cx.spawn_in(window, async move |_, cx| {
                                                                    let outcome = task.await;
                                                                    rv.update(cx, |view, cx| match outcome {
                                                                        Ok(r) => view.set_result(r, cx),
                                                                        Err(e) => view.set_error(e.to_string(), cx),
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
                                                                    .map(|c| Self::quote_ident(&c.name, driver))
                                                                    .collect::<Vec<_>>()
                                                                    .join(", ")
                                                            };
                                                            let sql = format!(
                                                                "SELECT {}\nFROM {};",
                                                                col_list,
                                                                Self::quote_ident(&tbl, driver)
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
                                                                let result_view = cx.new(|cx| ResultView::new(title, cx));
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
                                                                        Err(e) => view.set_error(e.to_string(), cx),
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
                                                                let result_view = cx.new(|cx| ResultView::new(title, cx));
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
                                                                        Err(e) => view.set_error(e.to_string(), cx),
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
                                                    .entry("Rename Table", None, {
                                                        let entity = entity.clone();
                                                        let tbl = tbl.clone();
                                                        let workspace = workspace.clone();
                                                        move |window, cx| {
                                                            let sql = format!(
                                                                "ALTER TABLE {} RENAME TO {};",
                                                                Self::quote_ident(&tbl, driver),
                                                                Self::quote_ident(&format!("{}_renamed", tbl), driver),
                                                            );
                                                            entity.update(cx, |panel, cx| {
                                                                Self::open_sql_query_with_text(workspace.clone(), panel.store.downgrade(), id, sql, window, cx);
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
                                                    .separator()
                                                    .entry("Rename Table (Script)", None, {
                                                        let entity = entity.clone();
                                                        let tbl = tbl.clone();
                                                        let db = db.clone();
                                                        let workspace = workspace.clone();
                                                        move |window, cx| {
                                                            let sql = match driver {
                                                                DatabaseDriver::MySQL => format!(
                                                                    "-- name: RenameTable :exec\nRENAME TABLE `{db}`.`{tbl}` TO `{db}`.`new_name`;"),
                                                                DatabaseDriver::SQLite => format!(
                                                                    "-- name: RenameTable :exec\nALTER TABLE \"{tbl}\" RENAME TO \"new_name\";"),
                                                                _ => format!(
                                                                    "-- name: RenameTable :exec\nALTER TABLE \"{db}\".\"{tbl}\" RENAME TO \"new_name\";"),
                                                            };
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
                                                        .pl(px(48.))
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
                                                                .pl(px(48.))
                                                                .pr_2()
                                                                .py_1()
                                                                .cursor_pointer()
                                                                .hover(|s| s.bg(gpui::transparent_white()))
                                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                                    if this.table_indexes_expanded.contains(&idx_key) {
                                                                        this.table_indexes_expanded.remove(&idx_key);
                                                                    } else {
                                                                        this.table_indexes_expanded.insert(idx_key.clone());
                                                                    }
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
                                                                    .pl(px(64.))
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
                                                                .pl(px(48.))
                                                                .pr_2()
                                                                .py_1()
                                                                .cursor_pointer()
                                                                .hover(|s| s.bg(gpui::transparent_white()))
                                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                                    if this.table_fks_expanded.contains(&fk_key) {
                                                                        this.table_fks_expanded.remove(&fk_key);
                                                                    } else {
                                                                        this.table_fks_expanded.insert(fk_key.clone());
                                                                    }
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
                                                                    .pl(px(64.))
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
                                                                .pl(px(48.))
                                                                .pr_2()
                                                                .py_1()
                                                                .cursor_pointer()
                                                                .hover(|s| s.bg(gpui::transparent_white()))
                                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                                    if this.table_triggers_expanded.contains(&trig_key) {
                                                                        this.table_triggers_expanded.remove(&trig_key);
                                                                    } else {
                                                                        this.table_triggers_expanded.insert(trig_key.clone());
                                                                    }
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
                                                                h_flex()
                                                                    .gap_1()
                                                                    .items_center()
                                                                    .pl(px(64.))
                                                                    .pr_2()
                                                                    .py_1()
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
                                                .pl(px(32.))
                                                .pr_2()
                                                .py_1()
                                                .cursor_pointer()
                                                .hover(|s| s.bg(gpui::transparent_white()))
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    if this.views_expanded.contains(&views_key) {
                                                        this.views_expanded.remove(&views_key);
                                                    } else {
                                                        this.views_expanded.insert(views_key.clone());
                                                    }
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
                                                    .pl(px(48.))
                                                    .pr_2()
                                                    .py_1()
                                                    .child(Icon::new(IconName::Eye).size(IconSize::XSmall).color(Color::Muted))
                                                    .child(Label::new(view_name.clone()).size(LabelSize::Small));

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
                                                                            let result_view = cx.new(|cx| ResultView::new(title, cx));
                                                                            let rv = result_view.clone();
                                                                            cx.spawn_in(window, async move |_, cx| {
                                                                                let outcome = task.await;
                                                                                rv.update(cx, |view, cx| match outcome {
                                                                                    Ok(r) => view.set_result(r, cx),
                                                                                    Err(e) => view.set_error(e.to_string(), cx),
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
                    .hover(|style| style.bg(gpui::transparent_white()))
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
                        let guide_left = px(8. + depth as f32 * 12. + 6.);
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
            .entry("New Query", None, {
                let panel = panel.clone();
                let workspace = workspace.clone();
                move |window, cx| {
                    panel.update(cx, |panel, cx| {
                        Self::open_sql_query_with_text(
                            workspace.clone(),
                            panel.store.downgrade(),
                            id,
                            String::new(),
                            window,
                            cx,
                        );
                    });
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
        .entry("New Connection", None, move |window, cx| {
            panel.update(cx, |panel, cx| {
                panel.new_connection_in_folder(None, window, cx)
            });
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
                    .flex_1()
                    .min_h_0()
                    .overflow_scroll()
                    .track_scroll(&self.tree_scroll_handle)
                    .child(tree_background)
                    .custom_scrollbars(
                        Scrollbars::new(ScrollAxes::Both)
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
                },
                super::SqlStatementRun {
                    sql: "SELECT *\nFROM schema.table".to_string(),
                    start_row: 2,
                },
                super::SqlStatementRun {
                    sql: "SHOW CREATE TABLE schema.table".to_string(),
                    start_row: 4,
                },
            ]
        );
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
        let path = super::connection_query_path(target, "Local MySQL");

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
            table_indexes: std::collections::HashMap::new(),
            table_fks: std::collections::HashMap::new(),
            table_triggers: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn build_folder_tree_nests_folders_first_then_sorted_connections() {
        let work = Folder::new("Work".into(), None, 0);
        let personal = Folder::new("Personal".into(), None, 1);
        let sub = Folder::new("Sub".into(), Some(work.id), 0);
        let work_id = work.id;
        let sub_id = sub.id;
        let folders = vec![work, personal, sub];
        let connections = vec![
            connection_in("beta", None, 1),
            connection_in("alpha", None, 0),
            connection_in("inner", Some(sub_id), 0),
            connection_in("w1", Some(work_id), 5),
        ];

        let nodes = super::build_folder_tree(&folders, &connections, None, 1);

        // Folders come first, ordered by `order`: Work then Personal.
        assert!(matches!(&nodes[0], TreeNode::Folder { folder, .. } if folder.name == "Work"));
        assert!(matches!(&nodes[1], TreeNode::Folder { folder, .. } if folder.name == "Personal"));
        // Then top-level connections, ordered by `order`: alpha (0) then beta (1).
        let labels: Vec<&str> = nodes[2..]
            .iter()
            .filter_map(|node| match node {
                TreeNode::Connection { index } => Some(connections[*index].config.label.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(labels, vec!["alpha", "beta"]);

        // Work nests the Sub folder (which holds "inner") and connection "w1".
        let TreeNode::Folder { children, .. } = &nodes[0] else {
            panic!("expected Work folder");
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
                    table_indexes_expanded: HashSet::default(),
                    table_fks_expanded: HashSet::default(),
                    table_triggers_expanded: HashSet::default(),
                    server_objects_expanded: HashSet::default(),
                    server_users: HashMap::default(),
                    table_filter_is_regex: false,
                    selected_tree_node: None,
                    dump: DumpUiState::default(),
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
                    table_indexes_expanded: HashSet::default(),
                    table_fks_expanded: HashSet::default(),
                    table_triggers_expanded: HashSet::default(),
                    server_objects_expanded: HashSet::default(),
                    server_users: HashMap::default(),
                    table_filter_is_regex: false,
                    selected_tree_node: None,
                    dump: DumpUiState::default(),
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
                    table_indexes_expanded: HashSet::default(),
                    table_fks_expanded: HashSet::default(),
                    table_triggers_expanded: HashSet::default(),
                    server_objects_expanded: HashSet::default(),
                    server_users: HashMap::default(),
                    table_filter_is_regex: false,
                    selected_tree_node: None,
                    dump: DumpUiState::default(),
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
                table_indexes_expanded: HashSet::default(),
                table_fks_expanded: HashSet::default(),
                table_triggers_expanded: HashSet::default(),
                server_objects_expanded: HashSet::default(),
                server_users: HashMap::default(),
                table_filter_is_regex: false,
                selected_tree_node: None,
                dump: DumpUiState::default(),
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
                table_indexes_expanded: HashSet::default(),
                table_fks_expanded: HashSet::default(),
                table_triggers_expanded: HashSet::default(),
                server_objects_expanded: HashSet::default(),
                server_users: HashMap::default(),
                table_filter_is_regex: false,
                selected_tree_node: None,
                dump: DumpUiState::default(),
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
                table_indexes_expanded: HashSet::default(),
                table_fks_expanded: HashSet::default(),
                table_triggers_expanded: HashSet::default(),
                server_objects_expanded: HashSet::default(),
                server_users: HashMap::default(),
                table_filter_is_regex: false,
                selected_tree_node: None,
                dump: DumpUiState::default(),
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
                table_indexes_expanded: HashSet::default(),
                table_fks_expanded: HashSet::default(),
                table_triggers_expanded: HashSet::default(),
                server_objects_expanded: HashSet::default(),
                server_users: HashMap::default(),
                table_filter_is_regex: false,
                selected_tree_node: None,
                dump: DumpUiState::default(),
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
                table_indexes_expanded: HashSet::default(),
                table_fks_expanded: HashSet::default(),
                table_triggers_expanded: HashSet::default(),
                server_objects_expanded: HashSet::default(),
                server_users: HashMap::default(),
                table_filter_is_regex: false,
                selected_tree_node: None,
                dump: DumpUiState::default(),
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
                table_indexes_expanded: HashSet::default(),
                table_fks_expanded: HashSet::default(),
                table_triggers_expanded: HashSet::default(),
                server_objects_expanded: HashSet::default(),
                server_users: HashMap::default(),
                table_filter_is_regex: false,
                selected_tree_node: None,
                dump: DumpUiState::default(),
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
            install_db_editor_features(editor.clone(), store.downgrade(), workspace.downgrade(), cx);
        });

        assert!(
            editor.read_with(cx, |editor, _| editor.semantics_provider().is_some()),
            "console editor must carry the DbSemanticsProvider so Ctrl+click resolves DDL"
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
                    table_indexes_expanded: HashSet::default(),
                    table_fks_expanded: HashSet::default(),
                    table_triggers_expanded: HashSet::default(),
                    server_objects_expanded: HashSet::default(),
                    server_users: HashMap::default(),
                    table_filter_is_regex: false,
                    selected_tree_node: None,
                    dump: DumpUiState::default(),
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
            panel.deploy_connection_context_menu(
                connection_id,
                gpui::Point::default(),
                window,
                cx,
            );
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
}
