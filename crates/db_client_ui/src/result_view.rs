use crate::store::DatabaseStore;
use db_client::{
    ConnectionId, DatabaseDriver,
    schema::{ColumnInfo, FkInfo, QueryResult},
};
use editor::{Editor, EditorEvent, MinimapVisibility};
use gpui::{
    Anchor, AnyElement, App, ClipboardItem, Context, ElementId, Entity, EventEmitter, FocusHandle,
    Focusable, IntoElement, KeyDownEvent, ListSizingBehavior, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, PromptLevel, Render, ScrollWheelEvent, SharedString,
    StatefulInteractiveElement, Subscription, Task, UniformListScrollHandle, WeakEntity, Window,
    actions, uniform_list,
};
use language::language_settings::SoftWrap;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use ui::{
    Button, ButtonCommon, ButtonStyle, Checkbox, Color, CommonAnimationExt, ContextMenu, Divider,
    Icon, IconButton, IconName, IconSize, Label, LabelSize, PopoverMenu, ScrollableHandle, Tooltip,
    prelude::*, right_click_menu,
};
use util::ResultExt as _;

use crate::widgets::popup_surface;
use workspace::{Item, Workspace};

/// Rows fetched per network round-trip while the result grid fills (chunked
/// loading, as common GUI database clients do).
const FETCH_BATCH: usize = 500;
/// Default number of rows to load before pausing, until the user asks for more.
const DEFAULT_FETCH_TARGET: usize = 500;
/// Page-size choices offered in the result toolbar. `usize::MAX` means "all"
/// (still bounded by the provider's hard row cap).
const FETCH_TARGET_CHOICES: [(usize, &str); 6] = [
    (100, "100"),
    (500, "500"),
    (1_000, "1 000"),
    (5_000, "5 000"),
    (10_000, "10 000"),
    (usize::MAX, "All"),
];

const COPY_FORMAT_CHOICES: [(CopyFormat, &str); 6] = [
    (CopyFormat::Tsv, "TSV"),
    (CopyFormat::Csv, "CSV"),
    (CopyFormat::Json, "JSON"),
    (CopyFormat::Markdown, "Markdown"),
    (CopyFormat::Insert, "Insert"),
    (CopyFormat::MultiInsert, "Multi Insert"),
];

const DEFAULT_LIMIT: usize = 500;

actions!(
    db_result_view,
    [
        /// Writes the pending cell edits to the database.
        SubmitEdits,
        /// Discards the pending cell edits.
        RevertEdits,
        /// Appends a new blank row to the result, submitted as an INSERT.
        AddRow,
        /// Marks the selected row for deletion, submitted as a DELETE.
        DeleteRow,
        /// Duplicates the selected row as a new row, submitted as an INSERT.
        CloneRow,
        /// Sets the selected cell to SQL NULL.
        SetNull,
        /// Sets the selected cell to the column DEFAULT.
        SetDefault,
        /// Opens or closes the value editor panel (full-content view of the selected cell).
        ToggleValueEditor,
        /// Opens or closes the find-in-results bar.
        ToggleFind,
        /// Moves to the next find match.
        FindNext,
        /// Moves to the previous find match.
        FindPrevious,
        /// Toggles the per-column local filter row.
        ToggleLocalFilters,
        /// Toggles the column visibility popup.
        ToggleColumnList,
        /// Opens or closes the query history popup.
        OpenQueryHistory,
        /// Opens or closes the record view panel (single row shown as field/value pairs).
        ToggleRecordView,
        /// Moves the record view to the previous row.
        RecordViewPrev,
        /// Moves the record view to the next row.
        RecordViewNext,
        /// Shows or hides the Quick Documentation panel for the selected column.
        QuickDoc,
        /// Navigates to the referenced row in the target table for the selected FK cell.
        NavigateToFkRow,
        /// Copies the selected cells or rows using the selected copy format.
        CopySelection,
        /// Opens a popup showing the SQL that the pending changes would run.
        PreviewPendingChanges,
        /// Switches the result between auto-commit and manual-commit modes.
        ToggleTransactionMode,
        /// Commits the staged statements in manual-commit mode.
        CommitTransaction,
        /// Discards the staged statements in manual-commit mode.
        RollbackTransaction,
        /// Opens the go-to-row input to jump to a row by its number.
        GoToRow,
        /// Copies the aggregate summary of the selected column to the clipboard.
        CopyAggregation,
        /// Toggles whether find hides rows without a match.
        ToggleFindFilterRows,
        /// Toggles the transposed view (columns become rows, records become columns).
        ToggleTranspose,
        /// Resets column order, hidden columns, sort, filters, and transpose to defaults.
        ResetView,
        /// Opens the Export Data dialog (format, headers, DDL, transpose, destination).
        OpenExportDialog,
        /// Opens or closes the chart view for the current result.
        ToggleChart,
        /// Pins this result tab so the next query opens a new tab instead of reusing it.
        TogglePinResult,
    ]
);

// Total rows the grid actually built across all frames. Used by tests to verify
// the table virtualizes (only the visible window is built, not the whole result).
#[cfg(test)]
pub(crate) static RENDERED_ROW_COUNT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
// Cap how much of a cell is rendered. A single multi-megabyte cell (TEXT/BLOB/
// JSON) would otherwise shape a giant label and freeze the main thread, so the
// grid shows a bounded one-line preview; the full value is still copyable.
const MAX_CELL_DISPLAY_CHARS: usize = 200;
const ROW_GUTTER_WIDTH: f32 = 48.0;

// A lively rotating loading indicator. A short rotation period reads as active
// work; kept in one place so every spinner in the grid looks the same.
fn loading_spinner(id: impl Into<ElementId>, size: IconSize) -> impl IntoElement {
    Icon::new(IconName::ArrowCircle)
        .size(size)
        .color(Color::Hint)
        .with_keyed_rotate_animation(id, 1)
}

// Length-bounded preview of a cell value. Iterates at most
// MAX_CELL_DISPLAY_CHARS + 1 characters, so cost never scales with cell size.
fn display_cell(value: &str) -> String {
    let mut out = String::new();
    let mut truncated = false;
    let mut previous_was_cr = false;
    for (index, ch) in value.chars().enumerate() {
        if index >= MAX_CELL_DISPLAY_CHARS {
            truncated = true;
            break;
        }
        if ch == '\r' {
            out.push('\n');
            previous_was_cr = true;
        } else if ch == '\n' && previous_was_cr {
            previous_was_cr = false;
        } else {
            out.push(ch);
            previous_was_cr = false;
        }
    }
    if truncated {
        out.push('…');
    }
    out
}

fn cell_value_needs_expanded_editor(value: &str) -> bool {
    value.chars().count() > MAX_CELL_DISPLAY_CHARS
        || value.contains('\n')
        || value.contains('\r')
        || value.contains('\t')
}

// Dim markers for the absence of a value, so a NULL or DEFAULT reads differently
// from an empty string and from a real value.
const NULL_MARKER: &str = "<null>";
const DEFAULT_MARKER: &str = "<default>";

// The one-line text and color the grid shows for a buffered cell value.
fn render_cell_value(value: &CellValue) -> (String, Color) {
    match value {
        CellValue::Text(text) => (display_cell(text), Color::Default),
        CellValue::Null => (NULL_MARKER.to_string(), Color::Muted),
        CellValue::Default => (DEFAULT_MARKER.to_string(), Color::Muted),
    }
}

// The one-line text and color the grid shows for a loaded cell, treating a NULL
// (None) the same dim marker as a buffered Null.
fn render_loaded_value(value: Option<&str>) -> (String, Color) {
    match value {
        Some(text) => (display_cell(text), Color::Default),
        None => (NULL_MARKER.to_string(), Color::Muted),
    }
}

// The editing widget appropriate for a column's SQL data type.
#[derive(Debug, Clone, PartialEq)]
enum CellEditorKind {
    Text,
    Boolean,
    Numeric,
    Enum(Vec<String>),
    Date,
    DateTime,
}

// Determines the editor kind from a SQL data-type string (case-insensitive).
fn column_editor_kind(data_type: &str) -> CellEditorKind {
    let lower = data_type.trim().to_lowercase();
    // Boolean: check tinyint(1) before the generic tinyint prefix.
    if lower == "bool"
        || lower == "boolean"
        || lower == "tinyint(1)"
        || lower == "bit"
        || lower.starts_with("bit(")
    {
        return CellEditorKind::Boolean;
    }
    if lower.starts_with("enum(") || lower.starts_with("set(") {
        return CellEditorKind::Enum(parse_enum_values(data_type));
    }
    // datetime / timestamp contain "date" so check them first.
    if lower == "datetime"
        || lower == "timestamp"
        || lower.starts_with("datetime(")
        || lower.starts_with("timestamp(")
    {
        return CellEditorKind::DateTime;
    }
    if lower == "date" || lower == "time" || lower == "year" || lower.starts_with("time(") {
        return CellEditorKind::Date;
    }
    let numeric_prefixes = [
        "int",
        "bigint",
        "smallint",
        "mediumint",
        "tinyint",
        "decimal",
        "float",
        "double",
        "numeric",
        "real",
        "number",
        "integer",
    ];
    for prefix in &numeric_prefixes {
        if lower == *prefix
            || lower.starts_with(&format!("{prefix}("))
            || lower.starts_with(&format!("{prefix} "))
        {
            return CellEditorKind::Numeric;
        }
    }
    CellEditorKind::Text
}

// Parses the value list from an enum('a','b') or set('x','y') type string.
fn parse_enum_values(data_type: &str) -> Vec<String> {
    let inner = data_type.find('(').and_then(|open| {
        data_type
            .rfind(')')
            .map(|close| &data_type[open + 1..close])
    });
    let Some(inner) = inner else {
        return Vec::new();
    };
    let mut values = Vec::new();
    let mut current = String::new();
    let mut in_quote = false;
    let mut chars = inner.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\'' if !in_quote => {
                in_quote = true;
            }
            '\'' if in_quote => {
                if chars.peek() == Some(&'\'') {
                    // escaped single-quote inside the value
                    chars.next();
                    current.push('\'');
                } else {
                    in_quote = false;
                    values.push(current.clone());
                    current.clear();
                }
            }
            _ if in_quote => {
                current.push(ch);
            }
            _ => {}
        }
    }
    values
}

fn is_truthy_bool(s: &str) -> bool {
    matches!(s.to_lowercase().as_str(), "1" | "true" | "t" | "yes" | "on")
}

fn bool_cell_display(value: &CellValue) -> (IconName, Color) {
    match value {
        CellValue::Null | CellValue::Default => (IconName::SquareMinus, Color::Muted),
        CellValue::Text(s) => {
            if is_truthy_bool(s) {
                (IconName::Check, Color::Success)
            } else {
                (IconName::SquareMinus, Color::Muted)
            }
        }
    }
}

fn toggle_bool_value(current: &CellValue) -> CellValue {
    let is_true = match current {
        CellValue::Text(s) => is_truthy_bool(s),
        CellValue::Null | CellValue::Default => false,
    };
    CellValue::Text(if is_true { "0" } else { "1" }.to_string())
}

fn is_valid_numeric(text: &str) -> bool {
    text.is_empty() || text.parse::<f64>().is_ok()
}

fn is_valid_date(text: &str) -> bool {
    if text.is_empty() {
        return true;
    }
    let parts: Vec<&str> = text.splitn(3, '-').collect();
    if parts.len() != 3 {
        return false;
    }
    let ok_year = parts[0].len() == 4 && parts[0].chars().all(|c| c.is_ascii_digit());
    let ok_month = parts[1].len() == 2
        && parts[1]
            .parse::<u8>()
            .map_or(false, |m| (1..=12).contains(&m));
    let ok_day = parts[2].len() == 2
        && parts[2]
            .parse::<u8>()
            .map_or(false, |d| (1..=31).contains(&d));
    ok_year && ok_month && ok_day
}

fn is_valid_datetime(text: &str) -> bool {
    if text.is_empty() {
        return true;
    }
    // Accept "YYYY-MM-DD HH:MM:SS" (with optional fractional seconds).
    let (date_part, time_part) = match text.split_once(' ') {
        Some(pair) => pair,
        None => return false,
    };
    if !is_valid_date(date_part) {
        return false;
    }
    let time_base = time_part
        .split_once('.')
        .map_or(time_part, |(base, _)| base);
    let hms: Vec<&str> = time_base.splitn(3, ':').collect();
    if hms.len() != 3 {
        return false;
    }
    let ok_h = hms[0].len() == 2 && hms[0].parse::<u8>().map_or(false, |h| h < 24);
    let ok_m = hms[1].len() == 2 && hms[1].parse::<u8>().map_or(false, |m| m < 60);
    let ok_s = hms[2].len() == 2 && hms[2].parse::<u8>().map_or(false, |s| s < 60);
    ok_h && ok_m && ok_s
}

#[derive(Clone, Copy)]
struct SortColumn {
    col_idx: usize,
    ascending: bool,
}

// Auto commits each Submit immediately. Manual stages the generated SQL so it
// runs only on an explicit Commit (and Roll Back discards it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TransactionMode {
    Auto,
    Manual,
}

impl TransactionMode {
    fn label(self) -> &'static str {
        match self {
            TransactionMode::Auto => "Auto",
            TransactionMode::Manual => "Manual",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CopyFormat {
    Tsv,
    Csv,
    Json,
    Markdown,
    Insert,
    MultiInsert,
}

impl CopyFormat {
    fn label(self) -> &'static str {
        match self {
            CopyFormat::Tsv => "TSV",
            CopyFormat::Csv => "CSV",
            CopyFormat::Json => "JSON",
            CopyFormat::Markdown => "Markdown",
            CopyFormat::Insert => "Insert",
            CopyFormat::MultiInsert => "Multi Insert",
        }
    }
}

// Output format offered by the Export Data dialog. A superset of CopyFormat
// that also covers the file-oriented formats.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExportChoice {
    Csv,
    Tsv,
    Json,
    Markdown,
    Html,
    SqlInsert,
    SqlMultiInsert,
    SqlUpdate,
}

impl ExportChoice {
    fn label(self) -> &'static str {
        match self {
            ExportChoice::Csv => "CSV",
            ExportChoice::Tsv => "TSV",
            ExportChoice::Json => "JSON",
            ExportChoice::Markdown => "Markdown",
            ExportChoice::Html => "HTML",
            ExportChoice::SqlInsert => "SQL Insert",
            ExportChoice::SqlMultiInsert => "SQL Multi Insert",
            ExportChoice::SqlUpdate => "SQL Update",
        }
    }

    // File extension used as the default name when saving to a file.
    fn extension(self) -> &'static str {
        match self {
            ExportChoice::Csv => "csv",
            ExportChoice::Tsv => "tsv",
            ExportChoice::Json => "json",
            ExportChoice::Markdown => "md",
            ExportChoice::Html => "html",
            ExportChoice::SqlInsert | ExportChoice::SqlMultiInsert | ExportChoice::SqlUpdate => {
                "sql"
            }
        }
    }

    // Whether toggling column headers changes this format's output. SQL and JSON
    // carry column names intrinsically, so the headers toggle does not apply.
    fn honors_headers(self) -> bool {
        matches!(
            self,
            ExportChoice::Csv | ExportChoice::Tsv | ExportChoice::Markdown | ExportChoice::Html
        )
    }

    // Whether this format names a target table (the SQL writers do).
    fn needs_table(self) -> bool {
        matches!(
            self,
            ExportChoice::SqlInsert | ExportChoice::SqlMultiInsert | ExportChoice::SqlUpdate
        )
    }
}

const EXPORT_CHOICES: &[ExportChoice] = &[
    ExportChoice::Csv,
    ExportChoice::Tsv,
    ExportChoice::Json,
    ExportChoice::Markdown,
    ExportChoice::Html,
    ExportChoice::SqlInsert,
    ExportChoice::SqlMultiInsert,
    ExportChoice::SqlUpdate,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChartKind {
    Bar,
    Line,
}

// Results that read better as formatted text than as a one-cell grid: DDL from
// `SHOW CREATE …` and query plans from `EXPLAIN …`. Detected from the query and
// the result's column names; shown as the default view (the user can still flip
// back to the table).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SpecialResult {
    None,
    Ddl,
    ExplainPlan,
}

// Skips leading whitespace and SQL comments so the first keyword can be matched.
fn sql_effective_start(sql: &str) -> &str {
    let mut rest = sql.trim_start();
    loop {
        if let Some(after) = rest.strip_prefix("--") {
            match after.find('\n') {
                Some(newline) => rest = after[newline + 1..].trim_start(),
                None => return "",
            }
        } else if let Some(after) = rest.strip_prefix("/*") {
            match after.find("*/") {
                Some(close) => rest = after[close + 2..].trim_start(),
                None => return "",
            }
        } else {
            return rest;
        }
    }
}

fn detect_special_result(sql: Option<&str>, columns: &[String]) -> SpecialResult {
    if let Some(sql) = sql {
        let upper = sql_effective_start(sql).to_ascii_uppercase();
        if upper.starts_with("EXPLAIN") {
            return SpecialResult::ExplainPlan;
        }
        if upper.starts_with("SHOW CREATE") {
            return SpecialResult::Ddl;
        }
    }
    if columns
        .iter()
        .any(|column| column.trim().to_ascii_lowercase().starts_with("create "))
    {
        return SpecialResult::Ddl;
    }
    SpecialResult::None
}

// Pulls the DDL string out of a `SHOW CREATE …` result: the "Create …" column of
// the first row, falling back to that row's last column.
fn ddl_text_from_result(result: &QueryResult) -> Option<String> {
    let row = result.rows.first()?;
    let index = result
        .columns
        .iter()
        .position(|column| column.trim().to_ascii_lowercase().starts_with("create "))
        .or_else(|| row.len().checked_sub(1))?;
    row.get(index)?.clone()
}

// An open enum/set dropdown popup waiting for the user to pick a value.
struct EnumPopup {
    abs_idx: usize,
    col_idx: usize,
    // None means this popup is for a loaded row; Some(i) means added_rows[i].
    added_idx: Option<usize>,
    values: Vec<String>,
    nullable: bool,
}

pub enum ResultViewEvent {
    ResultChanged,
}

pub struct ResultView {
    focus_handle: FocusHandle,
    title: SharedString,
    pub result: Option<QueryResult>,
    pub error: Option<String>,
    // Active sort columns in priority order (index 0 = primary sort).
    sort_columns: Vec<SortColumn>,
    store: Option<WeakEntity<DatabaseStore>>,
    connection_id: Option<ConnectionId>,
    // A pinned tab is never reused by show_result_in_pane, so the next query for
    // the same connection opens a fresh tab instead of overwriting this one.
    pinned: bool,
    env_accent: Option<gpui::Hsla>,
    database: Option<String>,
    table_name: Option<String>,
    filter_editor: Option<Entity<Editor>>,
    workspace: Option<WeakEntity<Workspace>>,
    is_loading: bool,
    // The user's statement, kept so the grid can fetch further pages.
    base_sql: Option<String>,
    // Rows to load before pausing; chosen via the page-size selector.
    fetch_target: usize,
    copy_format: CopyFormat,
    // Number of rows loaded so far, shown as a live count while filling.
    loaded_rows: usize,
    // Set true to stop an in-progress fill (Stop button or a new query).
    fill_cancel: Arc<AtomicBool>,
    // The running fill task; dropping it also cancels the fill.
    fill_task: Option<Task<()>>,
    // Vertical scroll of the virtualized row list.
    scroll_handle: UniformListScrollHandle,
    // Horizontal scroll of the grid; bounds+offset map a click x to a column and
    // drive horizontal virtualization (only on-screen columns are built).
    h_scroll: gpui::ScrollHandle,
    // Cached row order (with sort applied), per-column widths, cumulative right
    // edges (content coords, for click→column hit testing), and total content
    // width. Recomputed only when the result or sort changes — never per frame.
    order: Vec<usize>,
    col_widths: Vec<gpui::Pixels>,
    column_edges: Vec<f32>,
    total_width: f32,
    // Currently selected cell as (absolute row index, column index), highlighted
    // like a spreadsheet/grid selection.
    selected_cell: Option<(usize, usize)>,
    // Rectangular cell range selected as anchor..end in (abs_row, col_idx).
    selected_cell_range: Option<((usize, usize), (usize, usize))>,
    cell_drag_anchor: Option<(usize, usize)>,
    suppress_next_cell_click: bool,
    // Set of absolute row indices that are currently selected (via click /
    // shift-click / ctrl-click). Shown with a row-level highlight.
    selected_rows: std::collections::HashSet<usize>,
    // The row that anchors range selection (shift-click extends from here).
    last_selected_row: Option<usize>,
    // Timestamps of recent render frames (within the last second), used to show a
    // live FPS readout above the grid so scroll performance is measurable.
    frame_instants: std::collections::VecDeque<std::time::Instant>,
    fps: usize,
    // Active scrollbar drag, if the user grabbed a thumb. While set, a window
    // overlay captures mouse moves so the drag keeps tracking even when the
    // cursor leaves the narrow gutter.
    scroll_drag: Option<ScrollDrag>,
    // The cell currently being edited inline, if any.
    cell_edit: Option<CellEdit>,
    // A short, non-alarming note shown in the toolbar (e.g. an edit was kept in
    // the grid but not written to the database). Distinct from `error`, which is
    // a failed query.
    status_message: Option<String>,
    // Primary-key column names for the backing table, loaded lazily the first
    // time a cell is edited. `Some(empty)` means "looked up, no usable key".
    primary_key_columns: Option<Vec<String>>,
    // Full column metadata for the backing table, populated together with
    // primary_key_columns from a single describe_table call.
    column_infos: Option<Vec<ColumnInfo>>,
    // FK metadata keyed by result-column index; populated alongside column_infos.
    fk_columns: std::collections::HashMap<usize, FkInfo>,
    // An open enum/set dropdown popup, if any. At most one at a time.
    enum_popup: Option<EnumPopup>,
    // When true, a read-only value editor popup is shown near the selected cell,
    // displaying the full content of the selected cell.
    value_editor_open: bool,
    value_editor: Option<Entity<Editor>>,
    value_editor_size: Option<(f32, f32)>,
    value_editor_resize_drag: Option<ValueEditorResizeDrag>,
    // Find-on-page state: None = bar hidden, Some(text) = bar open with this query.
    find_query: Option<String>,
    // Ordered list of (abs_row, col_idx) for cells that match `find_query`.
    find_matches: Vec<(usize, usize)>,
    // Which match in `find_matches` is currently highlighted (cyclic navigation).
    find_current: usize,
    // Editor widget for the find bar text input.
    find_editor: Option<Entity<Editor>>,
    // Cell edits accumulated locally, keyed by (absolute row index, column
    // index). Holds the loaded original and the pending value so Revert can
    // restore and the grid can show the pending value without touching the
    // loaded result. Submitted in one batch on Submit; discarded on Revert.
    pending_edits: std::collections::HashMap<(usize, usize), PendingEdit>,
    // Loaded rows (by absolute index) marked for deletion. Submitted as one
    // DELETE per row; discarded on Revert.
    deleted_rows: std::collections::HashSet<usize>,
    // Pending INSERT rows, each sized to the result columns. Anchors control
    // where they appear in the current display order.
    added_rows: Vec<Vec<CellValue>>,
    added_row_anchors: Vec<AddedRowAnchor>,
    // Last-read filter text per data column index (empty = no filter).
    local_filters: Vec<String>,
    // Editor widget per column, created lazily when the filter row first opens.
    local_filter_editors: Vec<Entity<Editor>>,
    // Subset of `order` whose rows satisfy all active column filters. Equals
    // `order` when no filter is active, so the no-filter path has no overhead.
    filtered_display_order: Vec<usize>,
    // Whether the per-column local-filter input row is visible below the header.
    local_filter_visible: bool,
    // Columns hidden by the user; stored as data column indices.
    hidden_columns: std::collections::HashSet<usize>,
    // Whether the column-visibility popup is open.
    column_list_visible: bool,
    // Result column indices in display order, excluding hidden columns. Populated
    // by `recompute_layout`; empty when no result is loaded.
    visible_columns: Vec<usize>,
    // Recent SQL queries (most recent first), pushed on every run_sql call.
    query_history: Vec<String>,
    // Whether the query history popup is open.
    history_open: bool,
    // Whether the record view panel (single-row transpose) is open.
    record_view_open: bool,
    // The display-order index of the row shown in the record view.
    record_view_row: Option<usize>,
    // Whether the Quick Documentation panel (column metadata) is open.
    quick_doc_open: bool,
    // Single-line editor displayed next to the row-limit dropdown for custom
    // values. Created lazily on first render (requires window).
    limit_editor: Option<Entity<Editor>>,
    // Whether Submit commits immediately (Auto) or stages for an explicit
    // Commit (Manual).
    transaction_mode: TransactionMode,
    // In Manual mode, the SQL staged by Submit and run as one transaction on
    // Commit. Empty in Auto mode.
    staged_statements: Vec<String>,
    // Whether the pending-changes preview popup is open.
    preview_open: bool,
    // Go-to-row bar state: None = hidden, Some = open. The editor is created
    // lazily when the bar first opens (requires window).
    goto_row_visible: bool,
    goto_row_editor: Option<Entity<Editor>>,
    // When true, the find bar hides rows without a match instead of only
    // highlighting them.
    find_filter_rows: bool,
    // The cell whose value currently populates the value editor. Used to detect
    // when to reload the editor text on a cell change without clobbering edits
    // the user is typing into the same cell.
    value_editor_cell: Option<(usize, usize)>,
    // When true, the grid is shown transposed: original columns become rows and
    // each record becomes a column.
    transposed: bool,
    // Data column indices in the user's chosen display order. Drives
    // `visible_columns` (which also drops hidden columns). Identity by default.
    column_order: Vec<usize>,
    // Substring that filters the query history list (case-insensitive).
    history_search: String,
    // Editor widget for the history search input, created lazily.
    history_search_editor: Option<Entity<Editor>>,
    // Export Data dialog state.
    export_dialog_open: bool,
    export_format: ExportChoice,
    export_add_ddl: bool,
    export_headers: bool,
    export_transpose: bool,
    // DDL fetched for the backing table, cached so the dialog can prepend it
    // without re-querying. Populated when Add DDL is enabled.
    export_ddl: Option<String>,
    // Chart view state.
    chart_open: bool,
    chart_kind: ChartKind,
    // Result column index plotted on the value (Y) axis; defaults to the first
    // numeric column.
    chart_value_column: Option<usize>,
    // Optional result column index used for bar/point labels (X axis); None uses
    // the row number.
    chart_label_column: Option<usize>,
    // When true, a DDL/EXPLAIN result is shown as the raw grid because the user
    // asked for the table view instead of the formatted default.
    special_table_override: bool,
    // Lazily-built read-only editor showing a DDL result as formatted SQL.
    ddl_view: Option<Entity<Editor>>,
    // Lazily-built tree view for an EXPLAIN result.
    explain_view: Option<Entity<crate::explain_plan::ExplainPlanView>>,
    // Bumped on each new query so cached special views rebuild for fresh output.
    result_generation: u64,
    // The generation the cached special view was built for.
    special_built_for: Option<u64>,
}

// Which buffer an inline edit writes into when committed.
#[derive(Clone, Copy)]
enum CellEditTarget {
    // A loaded row; commit goes through `pending_edits` keyed by (abs, col).
    Loaded,
    // An added row at this index in `added_rows`; commit writes the value there.
    Added(usize),
}

// How an edit was started, which decides the initial selection. Excel keeps the
// value and puts the caret at the end when you open a cell with double-click/F2,
// and only selects-all (so the first keystroke replaces) when you start by typing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CellEditEntry {
    // Double-click / F2 / move-to-next-cell: keep the value, caret at the end.
    CursorEnd,
    // Type-to-replace: select the value so the first keystroke overwrites it.
    Replace,
}

// An in-progress inline cell edit: a single-line editor overlaid on the cell,
// prefilled with the cell's full value. The subscription cancels the edit when
// the editor loses focus.
struct CellEdit {
    abs_idx: usize,
    col_idx: usize,
    target: CellEditTarget,
    editor: Entity<Editor>,
    // Becomes true once the editor has actually gained focus. A blur is only
    // treated as a commit after this, so the spurious blur that can fire while
    // focus is still settling on open never commits an empty/half value.
    commit_armed: bool,
    _subscription: Subscription,
}

// A buffered cell value. `Text` is a literal string (numeric detection + quoting
// happen at SQL-build time); `Null` is SQL NULL; `Default` is the column DEFAULT
// keyword. Loaded NULLs in the result stay as `None` in the data — this enum is
// only for buffered edits and added-row cells.
#[derive(Clone, Debug, PartialEq, Eq)]
enum CellValue {
    Text(String),
    Null,
    Default,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AddedRowAnchor {
    End,
    AfterLoaded(usize),
    AfterAdded(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResultDisplayRow {
    Loaded(usize),
    Added(usize),
}

impl ResultDisplayRow {
    fn abs_idx(self, loaded_count: usize) -> usize {
        match self {
            Self::Loaded(abs_idx) => abs_idx,
            Self::Added(added_idx) => loaded_count + added_idx,
        }
    }
}

impl CellValue {
    // Builds a buffered value from raw editor text. An empty string stays an
    // explicit empty `Text`, never NULL — Set NULL is the only way to NULL a cell.
    fn from_text(text: String) -> Self {
        CellValue::Text(text)
    }

    // The matching `CellValue` for a value loaded from the database, used to
    // compare a buffered edit against its original.
    fn from_loaded(value: &Option<String>) -> Self {
        match value {
            None => CellValue::Null,
            Some(text) => CellValue::Text(text.clone()),
        }
    }
}

// One buffered cell change. `original` is the value loaded from the database
// (so reverting a single cell back to it drops the entry); `new_value` is what
// the grid shows and what Submit writes.
struct PendingEdit {
    original: Option<String>,
    new_value: CellValue,
}

// State of an in-progress scrollbar drag. The grab is relative: the content
// follows the cursor's movement from where it was grabbed, so the thumb is not
// re-centered under the cursor on press.
#[derive(Clone, Copy)]
struct ScrollDrag {
    vertical: bool,
    // Window coordinate on the drag axis where the grab started.
    grab_pos: f32,
    // Scroll offset (pixels, <= 0) at the moment of the grab.
    grab_offset: f32,
}

#[derive(Clone, Copy)]
struct ValueEditorResizeDrag {
    grab_x: f32,
    grab_y: f32,
    start_width: f32,
    start_height: f32,
}

impl ResultView {
    pub fn new(title: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            title: title.into(),
            result: None,
            error: None,
            sort_columns: Vec::new(),
            store: None,
            connection_id: None,
            pinned: false,
            env_accent: None,
            database: None,
            table_name: None,
            filter_editor: None,
            workspace: None,
            is_loading: false,
            base_sql: None,
            fetch_target: DEFAULT_FETCH_TARGET,
            copy_format: CopyFormat::Tsv,
            loaded_rows: 0,
            fill_cancel: Arc::new(AtomicBool::new(false)),
            fill_task: None,
            scroll_handle: UniformListScrollHandle::new(),
            h_scroll: gpui::ScrollHandle::new(),
            order: Vec::new(),
            col_widths: Vec::new(),
            column_edges: Vec::new(),
            total_width: 0.0,
            selected_cell: None,
            selected_cell_range: None,
            cell_drag_anchor: None,
            suppress_next_cell_click: false,
            selected_rows: std::collections::HashSet::new(),
            last_selected_row: None,
            frame_instants: std::collections::VecDeque::new(),
            fps: 0,
            scroll_drag: None,
            cell_edit: None,
            status_message: None,
            primary_key_columns: None,
            column_infos: None,
            fk_columns: std::collections::HashMap::new(),
            enum_popup: None,
            value_editor_open: false,
            value_editor: None,
            value_editor_size: None,
            value_editor_resize_drag: None,
            find_query: None,
            find_matches: Vec::new(),
            find_current: 0,
            find_editor: None,
            pending_edits: std::collections::HashMap::new(),
            deleted_rows: std::collections::HashSet::new(),
            added_rows: Vec::new(),
            added_row_anchors: Vec::new(),
            local_filters: Vec::new(),
            local_filter_editors: Vec::new(),
            filtered_display_order: Vec::new(),
            local_filter_visible: false,
            hidden_columns: std::collections::HashSet::new(),
            column_list_visible: false,
            visible_columns: Vec::new(),
            query_history: Vec::new(),
            history_open: false,
            record_view_open: false,
            record_view_row: None,
            quick_doc_open: false,
            limit_editor: None,
            transaction_mode: TransactionMode::Auto,
            staged_statements: Vec::new(),
            preview_open: false,
            goto_row_visible: false,
            goto_row_editor: None,
            find_filter_rows: false,
            value_editor_cell: None,
            transposed: false,
            column_order: Vec::new(),
            history_search: String::new(),
            history_search_editor: None,
            export_dialog_open: false,
            export_format: ExportChoice::Csv,
            export_add_ddl: false,
            export_headers: true,
            export_transpose: false,
            export_ddl: None,
            chart_open: false,
            chart_kind: ChartKind::Bar,
            chart_value_column: None,
            chart_label_column: None,
            special_table_override: false,
            ddl_view: None,
            explain_view: None,
            result_generation: 0,
            special_built_for: None,
        }
    }

    // Scrolls so the clicked point on a scrollbar gutter maps to the matching
    // scroll position (used when the track is clicked outside the thumb).
    fn scroll_to_gutter_pos(&self, vertical: bool, window_pos: f32) {
        if vertical {
            scroll_axis_to(&self.scroll_handle, true, window_pos);
        } else {
            scroll_axis_to(&self.h_scroll, false, window_pos);
        }
    }

    // Starts a scrollbar drag. Grabbing the thumb keeps its position (relative
    // drag); clicking the empty track first jumps the thumb under the cursor and
    // then continues as a relative drag from there.
    fn begin_scroll_drag(&mut self, vertical: bool, window_pos: f32) {
        let (origin, viewport_len, content_len, offset) = if vertical {
            axis_metrics(&self.scroll_handle, true)
        } else {
            axis_metrics(&self.h_scroll, false)
        };
        if content_len <= viewport_len {
            return;
        }
        let (thumb_start, thumb_end) = thumb_range(origin, viewport_len, content_len, offset);
        let grab_offset = if (thumb_start..=thumb_end).contains(&window_pos) {
            offset
        } else {
            let jumped = gutter_scroll_offset(window_pos, origin, viewport_len, content_len)
                .unwrap_or(offset);
            self.scroll_to_gutter_pos(vertical, window_pos);
            jumped
        };
        self.scroll_drag = Some(ScrollDrag {
            vertical,
            grab_pos: window_pos,
            grab_offset,
        });
    }

    // Continues an in-progress drag: the content follows the cursor's movement
    // from the grab point. `window_pos` is the cursor on the drag axis.
    fn update_scroll_drag(&mut self, window_pos: f32) {
        let Some(drag) = self.scroll_drag else {
            return;
        };
        let (_, viewport_len, content_len, _) = if drag.vertical {
            axis_metrics(&self.scroll_handle, true)
        } else {
            axis_metrics(&self.h_scroll, false)
        };
        let Some(new_offset) = drag_scroll_offset(
            drag.grab_offset,
            drag.grab_pos,
            window_pos,
            viewport_len,
            content_len,
        ) else {
            return;
        };
        if drag.vertical {
            let current = self.scroll_handle.offset();
            self.scroll_handle
                .set_offset(gpui::point(current.x, px(new_offset)));
        } else {
            let current = self.h_scroll.offset();
            self.h_scroll
                .set_offset(gpui::point(px(new_offset), current.y));
        }
    }

    fn end_scroll_drag(&mut self) {
        self.scroll_drag = None;
    }

    fn value_editor_auto_size(
        value: &str,
        available_width: f32,
        available_height: f32,
    ) -> (f32, f32) {
        let longest_line = value
            .lines()
            .map(|line| line.chars().count())
            .max()
            .unwrap_or(0);
        let line_count = value.lines().count().max(1);
        let max_width = available_width.max(420.0) - 16.0;
        let max_height = available_height.max(320.0) - 16.0;
        let width = (longest_line as f32 * 7.5 + 96.0).clamp(420.0, max_width.max(420.0));
        let height = (line_count as f32 * 18.0 + 68.0).clamp(300.0, max_height.max(300.0));
        (width, height)
    }

    fn begin_value_editor_resize(&mut self, grab_x: f32, grab_y: f32, width: f32, height: f32) {
        self.value_editor_size = Some((width, height));
        self.value_editor_resize_drag = Some(ValueEditorResizeDrag {
            grab_x,
            grab_y,
            start_width: width,
            start_height: height,
        });
    }

    fn update_value_editor_resize(&mut self, x: f32, y: f32) {
        let Some(drag) = self.value_editor_resize_drag else {
            return;
        };
        let width = (drag.start_width + x - drag.grab_x).clamp(360.0, 1400.0);
        let height = (drag.start_height + y - drag.grab_y).clamp(220.0, 900.0);
        self.value_editor_size = Some((width, height));
    }

    fn end_value_editor_resize(&mut self) {
        self.value_editor_resize_drag = None;
    }

    // Resolves which data column a window-space x coordinate falls in. Returns the
    // data column index (into result.rows[row]) rather than the display position.
    fn column_at_x(&self, window_x: f32) -> Option<usize> {
        let content_x = window_x
            - f32::from(self.h_scroll.bounds().origin.x)
            - f32::from(self.h_scroll.offset().x);
        let display_pos = self
            .column_edges
            .iter()
            .position(|&edge| content_x < edge)?;
        // Map display position to data column (identity when no columns are hidden).
        self.visible_columns.get(display_pos).copied()
    }

    // Records this frame and updates the rolling 1-second FPS, so the watermark
    // reflects the real render rate while scrolling.
    fn tick_fps(&mut self) {
        let now = std::time::Instant::now();
        self.frame_instants.push_back(now);
        while let Some(&front) = self.frame_instants.front() {
            if now.duration_since(front) > Duration::from_secs(1) {
                self.frame_instants.pop_front();
            } else {
                break;
            }
        }
        self.fps = self.frame_instants.len();
    }

    // Recomputes the cached row order (with sort applied) and per-column widths.
    // Called only when the result or sort changes — never per scroll frame — so
    // scrolling stays cheap.
    fn recompute_layout(&mut self) {
        let Some(result) = self.result.as_ref() else {
            self.order.clear();
            self.col_widths.clear();
            self.column_edges.clear();
            self.visible_columns.clear();
            self.filtered_display_order.clear();
            return;
        };
        let total_rows = result.rows.len();
        let total_cols = result.columns.len();

        self.order = if self.sort_columns.is_empty() {
            (0..total_rows).collect()
        } else {
            let sorts = self.sort_columns.clone();
            let mut indices: Vec<usize> = (0..total_rows).collect();
            indices.sort_by(|&a, &b| {
                for sc in &sorts {
                    let a_val = result.rows[a]
                        .get(sc.col_idx)
                        .and_then(|v| v.as_deref())
                        .unwrap_or("");
                    let b_val = result.rows[b]
                        .get(sc.col_idx)
                        .and_then(|v| v.as_deref())
                        .unwrap_or("");
                    let ord = a_val.cmp(b_val);
                    let ord = if sc.ascending { ord } else { ord.reverse() };
                    if ord != std::cmp::Ordering::Equal {
                        return ord;
                    }
                }
                std::cmp::Ordering::Equal
            });
            indices
        };

        // Column order defaults to identity and is reset whenever the result's
        // column count changes, so a stale order from a previous result never
        // points past the new column range.
        let order_matches = self.column_order.len() == total_cols
            && self.column_order.iter().all(|&c| c < total_cols);
        if !order_matches {
            self.column_order = (0..total_cols).collect();
        }

        // Visible columns: the chosen column order minus those hidden by the user.
        self.visible_columns = self
            .column_order
            .iter()
            .copied()
            .filter(|i| !self.hidden_columns.contains(i))
            .collect();

        let sample = total_rows.min(100);
        // col_widths[i] is the width for display position i (= visible_columns[i] data col).
        self.col_widths = self
            .visible_columns
            .iter()
            .map(|&data_col| {
                let col = result
                    .columns
                    .get(data_col)
                    .map(|s| s.as_str())
                    .unwrap_or("");
                let widest = result.rows[..sample]
                    .iter()
                    .map(|row| {
                        row.get(data_col)
                            .map(|cell| {
                                cell.as_deref()
                                    .unwrap_or("<null>")
                                    .chars()
                                    .take(MAX_CELL_DISPLAY_CHARS)
                                    .count()
                            })
                            .unwrap_or(0)
                    })
                    .max()
                    .unwrap_or(0)
                    .max(col.chars().count())
                    .max(3);
                px((widest as f32 * 7.5 + 28.0).clamp(80.0, 360.0))
            })
            .collect();

        // Cumulative right edge of each visible column (content coords) for
        // click→column hit testing, plus the total content width.
        let mut running = 0.0f32;
        self.column_edges = self
            .col_widths
            .iter()
            .map(|w| {
                running += f32::from(*w);
                running
            })
            .collect();
        self.total_width = running;

        self.recompute_local_filter_inner();
    }

    fn recompute_local_filter_inner(&mut self) {
        let Some(result) = self.result.as_ref() else {
            self.filtered_display_order.clear();
            return;
        };

        // A leading `!` negates the predicate (used by the "Exclude" quick
        // filter and typeable by hand), so a value starting with `!` cannot be
        // matched literally — an acceptable trade for an inline exclude.
        let active: Vec<(usize, String, bool)> = self
            .local_filters
            .iter()
            .enumerate()
            .filter(|(_, f)| !f.trim().is_empty())
            .map(|(i, f)| {
                let lowered = f.to_lowercase();
                match lowered.strip_prefix('!') {
                    Some(rest) => (i, rest.trim().to_string(), true),
                    None => (i, lowered, false),
                }
            })
            .filter(|(_, needle, _)| !needle.is_empty())
            .collect();

        // When find's "filter rows" mode is on, also hide rows with no match.
        let find_needle = if self.find_filter_rows {
            self.find_query
                .as_ref()
                .map(|query| query.trim().to_lowercase())
                .filter(|query| !query.is_empty())
        } else {
            None
        };

        if active.is_empty() && find_needle.is_none() {
            self.filtered_display_order = self.order.clone();
            return;
        }

        self.filtered_display_order = self
            .order
            .iter()
            .copied()
            .filter(|&abs_idx| {
                let local_ok = active.iter().all(|(vis_pos, needle, negate)| {
                    let data_col = match self.visible_columns.get(*vis_pos).copied() {
                        Some(c) => c,
                        None => return true,
                    };
                    let cell = result
                        .rows
                        .get(abs_idx)
                        .and_then(|row| row.get(data_col))
                        .and_then(|v| v.as_deref())
                        .unwrap_or("");
                    let contains = cell.to_lowercase().contains(needle.as_str());
                    contains != *negate
                });
                if !local_ok {
                    return false;
                }
                if let Some(needle) = &find_needle {
                    let row_matches = result.rows.get(abs_idx).is_some_and(|row| {
                        row.iter().any(|cell| {
                            cell.as_deref()
                                .unwrap_or("")
                                .to_lowercase()
                                .contains(needle.as_str())
                        })
                    });
                    if !row_matches {
                        return false;
                    }
                }
                true
            })
            .collect();
    }

    fn added_row_anchor(&self, added_idx: usize) -> AddedRowAnchor {
        self.added_row_anchors
            .get(added_idx)
            .copied()
            .unwrap_or(AddedRowAnchor::End)
    }

    fn anchor_for_abs_idx(&self, abs_idx: usize) -> AddedRowAnchor {
        let loaded_count = self.loaded_row_count();
        if abs_idx < loaded_count {
            AddedRowAnchor::AfterLoaded(abs_idx)
        } else {
            AddedRowAnchor::AfterAdded(abs_idx - loaded_count)
        }
    }

    fn push_added_rows_after_anchor(
        &self,
        anchor: AddedRowAnchor,
        entries: &mut Vec<ResultDisplayRow>,
        placed: &mut [bool],
    ) {
        while let Some(added_idx) = self.added_rows.iter().enumerate().position(|(idx, _)| {
            !placed.get(idx).copied().unwrap_or(true) && self.added_row_anchor(idx) == anchor
        }) {
            if let Some(placed) = placed.get_mut(added_idx) {
                *placed = true;
            }
            entries.push(ResultDisplayRow::Added(added_idx));
            self.push_added_rows_after_anchor(
                AddedRowAnchor::AfterAdded(added_idx),
                entries,
                placed,
            );
        }
    }

    fn display_row_entries(&self) -> Vec<ResultDisplayRow> {
        let mut entries =
            Vec::with_capacity(self.filtered_display_order.len() + self.added_rows.len());
        let mut placed = vec![false; self.added_rows.len()];
        for &abs_idx in &self.filtered_display_order {
            entries.push(ResultDisplayRow::Loaded(abs_idx));
            self.push_added_rows_after_anchor(
                AddedRowAnchor::AfterLoaded(abs_idx),
                &mut entries,
                &mut placed,
            );
        }

        for added_idx in 0..self.added_rows.len() {
            if !placed.get(added_idx).copied().unwrap_or(true) {
                entries.push(ResultDisplayRow::Added(added_idx));
                if let Some(placed) = placed.get_mut(added_idx) {
                    *placed = true;
                }
                self.push_added_rows_after_anchor(
                    AddedRowAnchor::AfterAdded(added_idx),
                    &mut entries,
                    &mut placed,
                );
            }
        }

        entries
    }

    fn abs_idx_at_display_idx(&self, display_idx: usize) -> Option<usize> {
        self.display_row_entries()
            .get(display_idx)
            .copied()
            .map(|row| row.abs_idx(self.loaded_row_count()))
    }

    fn remove_added_row(&mut self, added_idx: usize) {
        if added_idx >= self.added_rows.len() {
            return;
        }
        self.added_rows.remove(added_idx);
        if added_idx < self.added_row_anchors.len() {
            self.added_row_anchors.remove(added_idx);
        }
        for anchor in &mut self.added_row_anchors {
            match *anchor {
                AddedRowAnchor::AfterAdded(anchor_idx) if anchor_idx == added_idx => {
                    *anchor = AddedRowAnchor::End;
                }
                AddedRowAnchor::AfterAdded(anchor_idx) if anchor_idx > added_idx => {
                    *anchor = AddedRowAnchor::AfterAdded(anchor_idx - 1);
                }
                _ => {}
            }
        }
    }

    fn last_selectable_column(&self) -> Option<usize> {
        self.result
            .as_ref()
            .and_then(|result| result.columns.len().checked_sub(1))
    }

    fn select_entire_row(&mut self, abs_idx: usize, display_idx: usize) {
        self.selected_rows.clear();
        self.selected_rows.insert(abs_idx);
        self.last_selected_row = Some(display_idx);
        self.selected_cell = None;
        self.selected_cell_range = self
            .last_selectable_column()
            .map(|last_col| ((abs_idx, 0), (abs_idx, last_col)));
        self.record_view_row = Some(display_idx);
    }

    fn select_row_range(&mut self, anchor_display_idx: usize, display_idx: usize) {
        let lo = anchor_display_idx.min(display_idx);
        let hi = anchor_display_idx.max(display_idx);
        self.selected_rows.clear();
        for display_idx in lo..=hi {
            if let Some(abs_idx) = self.abs_idx_at_display_idx(display_idx) {
                self.selected_rows.insert(abs_idx);
            }
        }
        self.selected_cell = None;
        self.selected_cell_range = match (
            self.abs_idx_at_display_idx(lo),
            self.abs_idx_at_display_idx(hi),
            self.last_selectable_column(),
        ) {
            (Some(start_abs_idx), Some(end_abs_idx), Some(last_col)) => {
                Some(((start_abs_idx, 0), (end_abs_idx, last_col)))
            }
            _ => None,
        };
    }

    fn select_cell_from_click(
        &mut self,
        abs_idx: usize,
        display_idx: usize,
        cell_idx: usize,
        shift: bool,
        control: bool,
    ) {
        if shift {
            if let Some(anchor) = self.selected_cell {
                self.selected_rows.clear();
                self.selected_cell_range = Some((anchor, (abs_idx, cell_idx)));
            } else {
                let anchor_display_idx = self.last_selected_row.unwrap_or(display_idx);
                self.select_row_range(anchor_display_idx, display_idx);
            }
        } else if control {
            self.selected_rows.clear();
            self.last_selected_row = Some(display_idx);
            self.selected_cell_range = None;
        } else {
            self.selected_rows.clear();
            self.last_selected_row = Some(display_idx);
            self.selected_cell_range = None;
        }
        self.selected_cell = Some((abs_idx, cell_idx));
        self.record_view_row = Some(display_idx);
        self.value_editor_open = self
            .selected_cell_full_value()
            .as_deref()
            .is_some_and(cell_value_needs_expanded_editor);
    }

    fn begin_cell_drag(&mut self, abs_idx: usize, cell_idx: usize) {
        self.cell_drag_anchor = Some((abs_idx, cell_idx));
        self.suppress_next_cell_click = false;
    }

    fn update_cell_drag(
        &mut self,
        abs_idx: usize,
        display_idx: usize,
        cell_idx: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(anchor) = self.cell_drag_anchor else {
            return;
        };
        self.selected_rows.clear();
        self.last_selected_row = Some(display_idx);
        self.selected_cell = Some((abs_idx, cell_idx));
        self.selected_cell_range = Some((anchor, (abs_idx, cell_idx)));
        self.record_view_row = Some(display_idx);
        self.suppress_next_cell_click = true;
        cx.notify();
    }

    fn end_cell_drag(&mut self) {
        self.cell_drag_anchor = None;
    }

    fn should_suppress_cell_click(&mut self) -> bool {
        if self.suppress_next_cell_click {
            self.suppress_next_cell_click = false;
            true
        } else {
            false
        }
    }

    fn click_loaded_cell(
        &mut self,
        abs_idx: usize,
        display_idx: usize,
        cell_idx: usize,
        click_count: usize,
        shift: bool,
        control: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.should_suppress_cell_click() {
            cx.notify();
            return;
        }
        let repeated_selected_cell_click = !shift
            && !control
            && self.selected_cell == Some((abs_idx, cell_idx))
            && self.selected_cell_range.is_none();
        self.select_cell_from_click(abs_idx, display_idx, cell_idx, shift, control);
        if (click_count >= 2 || repeated_selected_cell_click)
            && !matches!(self.column_kind_at(cell_idx), CellEditorKind::Boolean)
        {
            self.begin_cell_edit(abs_idx, cell_idx, CellEditEntry::CursorEnd, window, cx);
        } else if matches!(self.column_kind_at(cell_idx), CellEditorKind::Boolean) {
            self.toggle_boolean_cell_loaded(abs_idx, cell_idx, cx);
        } else if let Some(value) = self
            .result
            .as_ref()
            .and_then(|result| result.rows.get(abs_idx))
            .and_then(|row| row.get(cell_idx))
            .and_then(|cell| cell.clone())
        {
            cx.write_to_clipboard(ClipboardItem::new_string(value));
        }
        cx.notify();
    }

    fn click_added_cell(
        &mut self,
        abs_idx: usize,
        display_idx: usize,
        cell_idx: usize,
        added_idx: usize,
        click_count: usize,
        shift: bool,
        control: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.should_suppress_cell_click() {
            cx.notify();
            return;
        }
        let repeated_selected_cell_click = !shift
            && !control
            && self.selected_cell == Some((abs_idx, cell_idx))
            && self.selected_cell_range.is_none();
        self.select_cell_from_click(abs_idx, display_idx, cell_idx, shift, control);
        if (click_count >= 2 || repeated_selected_cell_click)
            && !matches!(self.column_kind_at(cell_idx), CellEditorKind::Boolean)
        {
            self.begin_added_cell_edit(
                abs_idx,
                cell_idx,
                added_idx,
                CellEditEntry::CursorEnd,
                window,
                cx,
            );
        } else if matches!(self.column_kind_at(cell_idx), CellEditorKind::Boolean) {
            self.toggle_boolean_cell_added(cell_idx, added_idx, cx);
        } else if let Some(CellValue::Text(value)) = self
            .added_rows
            .get(added_idx)
            .and_then(|row| row.get(cell_idx))
        {
            cx.write_to_clipboard(ClipboardItem::new_string(value.clone()));
        }
        cx.notify();
    }

    fn selected_cell_range_contains(
        &self,
        _abs_idx: usize,
        display_idx: usize,
        cell_idx: usize,
    ) -> bool {
        let Some(((anchor_abs_idx, anchor_col_idx), (end_abs_idx, end_col_idx))) =
            self.selected_cell_range
        else {
            return false;
        };

        let anchor_display_idx = self
            .display_idx_of(anchor_abs_idx)
            .unwrap_or(anchor_abs_idx);
        let end_display_idx = self.display_idx_of(end_abs_idx).unwrap_or(end_abs_idx);
        let row_lo = anchor_display_idx.min(end_display_idx);
        let row_hi = anchor_display_idx.max(end_display_idx);
        let col_lo = anchor_col_idx.min(end_col_idx);
        let col_hi = anchor_col_idx.max(end_col_idx);
        display_idx >= row_lo && display_idx <= row_hi && cell_idx >= col_lo && cell_idx <= col_hi
    }

    pub fn with_workspace(mut self, workspace: WeakEntity<Workspace>) -> Self {
        self.workspace = Some(workspace);
        self
    }

    pub fn with_connection(mut self, connection_id: ConnectionId) -> Self {
        self.connection_id = Some(connection_id);
        self
    }

    pub fn with_env_color(mut self, color: Option<gpui::Hsla>) -> Self {
        self.env_accent = color;
        self
    }

    pub fn connection_id(&self) -> Option<ConnectionId> {
        self.connection_id
    }

    pub fn is_pinned(&self) -> bool {
        self.pinned
    }

    fn toggle_pinned(&mut self, cx: &mut Context<Self>) {
        self.pinned = !self.pinned;
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn set_pinned_for_test(&mut self, pinned: bool) {
        self.pinned = pinned;
    }

    #[cfg(test)]
    pub(crate) fn table_context_for_test(&self) -> Option<(String, String)> {
        Some((self.database.clone()?, self.table_name.clone()?))
    }

    pub fn with_table_context(
        mut self,
        store: WeakEntity<DatabaseStore>,
        connection_id: ConnectionId,
        database: String,
        table_name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        self.set_table_context(store, connection_id, database, table_name, window, cx);
        self
    }

    pub fn set_table_context(
        &mut self,
        store: WeakEntity<DatabaseStore>,
        connection_id: ConnectionId,
        database: String,
        table_name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editor = cx.new(|cx| {
            let mut ed = Editor::single_line(window, cx);
            ed.set_placeholder_text("WHERE clause (e.g. id > 100)", window, cx);
            ed
        });
        self.store = Some(store);
        self.connection_id = Some(connection_id);
        self.database = Some(database);
        self.table_name = Some(table_name);
        self.filter_editor = Some(editor);
        self.primary_key_columns = None;
        self.column_infos = None;
        self.fk_columns.clear();
        self.enum_popup = None;
        self.sort_columns.clear();
    }

    pub fn clear_table_context(&mut self) {
        self.table_name = None;
        self.filter_editor = None;
        self.primary_key_columns = None;
        self.column_infos = None;
        self.fk_columns.clear();
        self.enum_popup = None;
    }

    pub fn refresh_table_data(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (store, conn_id, db, table) = match (
            self.store.as_ref(),
            self.connection_id,
            self.database.as_ref(),
            self.table_name.as_ref(),
        ) {
            (Some(s), Some(id), Some(db), Some(tbl)) => (s.clone(), id, db.clone(), tbl.clone()),
            _ => return,
        };

        let filter_text = self
            .filter_editor
            .as_ref()
            .map(|ed| ed.read(cx).text(cx))
            .unwrap_or_default();
        let filter_text = filter_text.trim().to_string();

        let quote = match store.upgrade().and_then(|s| {
            let store_ref = s.read(cx);
            store_ref
                .connections()
                .iter()
                .find(|c| c.config.id == conn_id)
                .map(|c| c.config.driver)
        }) {
            Some(db_client::DatabaseDriver::MySQL) => '`',
            _ => '"',
        };

        let mut sql = format!("SELECT * FROM {0}{1}{0}", quote, table);
        if !filter_text.is_empty() {
            sql.push_str(&format!(" WHERE {}", filter_text));
        }
        if !self.sort_columns.is_empty() {
            let clauses: Vec<String> = self
                .sort_columns
                .iter()
                .filter_map(|sc| {
                    self.result
                        .as_ref()
                        .and_then(|r| r.columns.get(sc.col_idx))
                        .map(|name| {
                            let dir = if sc.ascending { "ASC" } else { "DESC" };
                            format!("{0}{1}{0} {2}", quote, name, dir)
                        })
                })
                .collect();
            if !clauses.is_empty() {
                sql.push_str(&format!(" ORDER BY {}", clauses.join(", ")));
            }
        }
        sql.push_str(&format!(" LIMIT {}", DEFAULT_LIMIT));

        self.is_loading = true;
        cx.notify();

        let task = store.upgrade().map(|s| {
            s.update(cx, |store, cx| {
                store.execute_query(conn_id, db.clone(), sql, cx)
            })
        });

        let Some(task) = task else {
            self.is_loading = false;
            cx.notify();
            return;
        };

        cx.spawn_in(window, async move |this, cx| {
            let outcome = task.await;
            this.update(cx, |this, cx| {
                this.is_loading = false;
                match outcome {
                    Ok(result) => this.set_result(result, cx),
                    Err(err) => this.set_error(err.to_string(), cx),
                }
            })
            .log_err();
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    // Clears any previous output and marks the view as running, so the table
    // area shows a centered spinner until the result or error arrives.
    pub fn set_loading(&mut self, cx: &mut Context<Self>) {
        self.is_loading = true;
        self.result = None;
        self.error = None;
        self.cell_drag_anchor = None;
        self.suppress_next_cell_click = false;
        self.value_editor_open = false;
        self.value_editor = None;
        self.value_editor_resize_drag = None;
        cx.notify();
    }

    pub fn set_result(&mut self, result: QueryResult, cx: &mut Context<Self>) {
        self.result = Some(result);
        self.reset_special_view();
        self.error = None;
        self.cell_edit = None;
        self.status_message = None;
        self.pending_edits.clear();
        self.deleted_rows.clear();
        self.added_rows.clear();
        self.added_row_anchors.clear();
        self.cell_drag_anchor = None;
        self.suppress_next_cell_click = false;
        self.selected_cell = None;
        self.selected_cell_range = None;
        self.selected_rows.clear();
        self.last_selected_row = None;
        self.value_editor_open = false;
        self.value_editor_resize_drag = None;
        self.is_loading = false;
        // Reset per-column filter editors when the result schema changes.
        self.local_filter_editors.clear();
        self.local_filters.clear();
        // sort_columns is intentionally preserved here so that a server-side ORDER
        // BY refresh (triggered by a column header click) keeps the sort indicator
        // visible after the new result arrives.  The sort state is reset in run_sql
        // (new user query) and with_table_context (new table).
        self.recompute_layout();
        cx.emit(ResultViewEvent::ResultChanged);
        cx.notify();
    }

    fn reset_special_view(&mut self) {
        self.result_generation = self.result_generation.wrapping_add(1);
        self.special_table_override = false;
        self.ddl_view = None;
        self.explain_view = None;
        self.special_built_for = None;
    }

    // The formatted view to show for the current result, or None to fall back to
    // the grid (no result, not a DDL/EXPLAIN result, or the user chose Table).
    fn active_special(&self) -> SpecialResult {
        if self.special_table_override {
            return SpecialResult::None;
        }
        match self.result.as_ref() {
            Some(result) => detect_special_result(self.base_sql.as_deref(), &result.columns),
            None => SpecialResult::None,
        }
    }

    // Builds (once per result) the read-only editor or plan tree backing the
    // formatted view. Runs in render(), where a Window is available.
    fn sync_special_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let special = self.active_special();
        if special == SpecialResult::None || self.special_built_for == Some(self.result_generation) {
            return;
        }
        let Some(result) = self.result.clone() else {
            return;
        };
        match special {
            SpecialResult::Ddl => {
                let text = ddl_text_from_result(&result).unwrap_or_default();
                let editor = cx.new(|cx| {
                    let mut editor = Editor::multi_line(window, cx);
                    editor.set_show_gutter(false, cx);
                    editor.disable_expand_excerpt_buttons(cx);
                    editor.set_minimap_visibility(MinimapVisibility::Disabled, window, cx);
                    editor.set_soft_wrap_mode(SoftWrap::EditorWidth, cx);
                    editor.set_show_indent_guides(false, cx);
                    editor.disable_mouse_wheel_zoom();
                    editor.set_text(text, window, cx);
                    editor.set_read_only(true);
                    editor
                });
                self.ddl_view = Some(editor);
                self.explain_view = None;
            }
            SpecialResult::ExplainPlan => {
                let plan_text = crate::explain_plan::plan_text_from_result(&result);
                let roots = crate::explain_plan::parse_plan_tree(&plan_text);
                let view = cx.new(|cx| crate::explain_plan::ExplainPlanView::new(roots, window, cx));
                self.explain_view = Some(view);
                self.ddl_view = None;
            }
            SpecialResult::None => {}
        }
        self.special_built_for = Some(self.result_generation);
    }

    fn render_special_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let special = self.active_special();
        let (title, body) = match special {
            SpecialResult::Ddl => {
                let body = self.ddl_view.clone().map(|editor| {
                    div()
                        .id("ddl-result-view")
                        .debug_selector(|| "DDL_RESULT_VIEW".to_string())
                        .flex_1()
                        .min_h_0()
                        .w_full()
                        .p_2()
                        .child(editor)
                        .into_any_element()
                });
                ("DDL", body)
            }
            SpecialResult::ExplainPlan => {
                let body = self.explain_view.clone().map(|view| {
                    div()
                        .id("explain-plan-view")
                        .debug_selector(|| "EXPLAIN_PLAN_VIEW".to_string())
                        .flex_1()
                        .min_h_0()
                        .w_full()
                        .overflow_scroll()
                        .p_2()
                        .child(view)
                        .into_any_element()
                });
                ("Query plan", body)
            }
            SpecialResult::None => return self.render_result(cx),
        };
        let body = body.unwrap_or_else(|| div().flex_1().into_any_element());

        v_flex()
            .size_full()
            .child(
                h_flex()
                    .flex_none()
                    .px_2()
                    .py_1()
                    .gap_2()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        Label::new(title)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("special-show-as-table", "Show as Table")
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.special_table_override = true;
                                cx.notify();
                            })),
                    ),
            )
            .child(body)
            .into_any_element()
    }

    pub fn set_error(&mut self, error: String, cx: &mut Context<Self>) {
        self.error = Some(error);
        self.result = None;
        self.is_loading = false;
        self.fill_task = None;
        self.cell_drag_anchor = None;
        self.suppress_next_cell_click = false;
        self.value_editor_open = false;
        self.value_editor = None;
        self.value_editor_resize_drag = None;
        cx.emit(ResultViewEvent::ResultChanged);
        cx.notify();
    }

    /// Runs `base_sql` for `connection_id` and fills the grid in chunks: fetch
    /// FETCH_BATCH rows at a time and append until the target is reached or the
    /// user stops. The view owns the fetch so it can page and cancel without
    /// round-tripping through the panel.
    pub fn run_sql(
        &mut self,
        store: WeakEntity<DatabaseStore>,
        connection_id: ConnectionId,
        database: String,
        base_sql: String,
        cx: &mut Context<Self>,
    ) {
        self.store = Some(store);
        self.connection_id = Some(connection_id);
        self.database = Some(database);
        // Push to history before overwriting base_sql.
        let trimmed = base_sql.trim().to_string();
        if !trimmed.is_empty() {
            self.query_history.retain(|q| q != &trimmed);
            self.query_history.insert(0, trimmed);
            self.query_history.truncate(50);
        }
        self.base_sql = Some(base_sql);
        self.fetch_target = DEFAULT_FETCH_TARGET;
        self.sort_columns.clear();
        self.start_fill(cx);
    }

    pub fn set_query_result(
        &mut self,
        store: WeakEntity<DatabaseStore>,
        connection_id: ConnectionId,
        database: String,
        base_sql: String,
        result: QueryResult,
        cx: &mut Context<Self>,
    ) {
        self.fill_cancel.store(true, Ordering::SeqCst);
        self.fill_task = None;
        self.store = Some(store);
        self.connection_id = Some(connection_id);
        self.database = Some(database);
        let trimmed = base_sql.trim().to_string();
        if !trimmed.is_empty() {
            self.query_history.retain(|query| query != &trimmed);
            self.query_history.insert(0, trimmed);
            self.query_history.truncate(50);
        }
        self.base_sql = Some(base_sql);
        self.fetch_target = DEFAULT_FETCH_TARGET;
        self.sort_columns.clear();
        self.loaded_rows = result.rows.len();
        self.set_result(result, cx);
    }

    fn start_fill(&mut self, cx: &mut Context<Self>) {
        // Cancel any prior fill, then arm a fresh cancel flag for this run.
        self.fill_cancel.store(true, Ordering::SeqCst);
        let cancel = Arc::new(AtomicBool::new(false));
        self.fill_cancel = cancel.clone();

        let (Some(store), Some(connection_id), Some(base_sql)) = (
            self.store.clone(),
            self.connection_id,
            self.base_sql.clone(),
        ) else {
            return;
        };
        let database = self.database.clone().unwrap_or_default();
        let target = self.fetch_target;
        let pageable = db_client::is_pageable_query(&base_sql);

        self.result = None;
        self.reset_special_view();
        self.error = None;
        self.loaded_rows = 0;
        self.is_loading = true;
        cx.notify();

        self.fill_task = Some(cx.spawn(async move |this, cx| {
            let mut offset = 0usize;
            loop {
                if cancel.load(Ordering::SeqCst) {
                    break;
                }
                let sql = if pageable {
                    Self::wrap_paged(&base_sql, FETCH_BATCH, offset)
                } else {
                    base_sql.clone()
                };
                let Ok(task) = store.update(cx, |store, cx| {
                    store.execute_query(connection_id, database.clone(), sql, cx)
                }) else {
                    break;
                };
                match task.await {
                    Ok(batch) => {
                        let fetched = batch.rows.len();
                        if this
                            .update(cx, |view, cx| view.append_batch(batch, cx))
                            .is_err()
                        {
                            break;
                        }
                        offset += fetched;
                        if !pageable || fetched < FETCH_BATCH || offset >= target {
                            break;
                        }
                    }
                    Err(err) => {
                        this.update(cx, |view, cx| view.set_error(err.to_string(), cx))
                            .ok();
                        return;
                    }
                }
                // Yield briefly between chunks so the grid visibly fills and the
                // UI stays responsive (the Stop button keeps working).
                cx.background_executor()
                    .timer(Duration::from_millis(30))
                    .await;
            }
            this.update(cx, |view, cx| {
                view.is_loading = false;
                cx.notify();
            })
            .ok();
        }));
    }

    fn append_batch(&mut self, batch: QueryResult, cx: &mut Context<Self>) {
        match &mut self.result {
            Some(existing) => existing.rows.extend(batch.rows),
            None => self.result = Some(batch),
        }
        self.loaded_rows = self.result.as_ref().map_or(0, |result| result.rows.len());
        self.error = None;
        self.recompute_layout();
        cx.notify();
    }

    fn stop_fill(&mut self, cx: &mut Context<Self>) {
        self.fill_cancel.store(true, Ordering::SeqCst);
        self.fill_task = None;
        self.is_loading = false;
        cx.notify();
    }

    fn set_fetch_target(&mut self, target: usize, cx: &mut Context<Self>) {
        if self.fetch_target != target {
            self.fetch_target = target;
            if self.base_sql.is_some() {
                self.start_fill(cx);
            }
        }
    }

    fn limit_display_text(fetch_target: usize) -> String {
        if fetch_target == usize::MAX {
            "All".to_string()
        } else {
            fetch_target.to_string()
        }
    }

    fn sync_limit_editor_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ed) = self.limit_editor.clone() {
            let text = Self::limit_display_text(self.fetch_target);
            ed.update(cx, |e, cx| e.set_text(text, window, cx));
        }
    }

    // Reads the current text of the limit editor and applies it as the active
    // fetch target. `force` must be true when called from the Enter key handler
    // (the target may equal the current value after an up/down adjustment, so
    // equality alone cannot gate the refetch). `force = false` on blur avoids
    // a second refetch when Enter already triggered one.
    fn apply_custom_limit(&mut self, force: bool, cx: &mut Context<Self>) {
        let Some(ed) = self.limit_editor.clone() else {
            return;
        };
        let raw = ed.read(cx).text(cx);
        let text = raw.trim();
        let target = if text.eq_ignore_ascii_case("all") || text == "0" || text.is_empty() {
            usize::MAX
        } else if let Ok(n) = text.parse::<usize>() {
            n.max(1)
        } else {
            return;
        };
        let changed = target != self.fetch_target;
        self.fetch_target = target;
        if (changed || force) && self.base_sql.is_some() {
            self.start_fill(cx);
        }
    }

    // Starts inline editing of one loaded cell. For table-backed results the
    // commit buffers a pending update; for arbitrary results the editor still
    // opens so the user can select/copy part of a large value.
    fn begin_cell_edit(
        &mut self,
        abs_idx: usize,
        col_idx: usize,
        entry: CellEditEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.table_name.is_some() {
            self.ensure_primary_key_columns(cx);
        }

        match self.column_kind_at(col_idx) {
            CellEditorKind::Boolean => {
                self.toggle_boolean_cell_loaded(abs_idx, col_idx, cx);
                return;
            }
            CellEditorKind::Enum(values) => {
                let nullable = self.is_column_nullable_at(col_idx);
                self.enum_popup = Some(EnumPopup {
                    abs_idx,
                    col_idx,
                    added_idx: None,
                    values,
                    nullable,
                });
                cx.notify();
                return;
            }
            _ => {}
        }

        let Some(initial) = self
            .result
            .as_ref()
            .and_then(|result| result.rows.get(abs_idx))
            .and_then(|row| row.get(col_idx))
            .map(|cell| cell.clone().unwrap_or_default())
        else {
            return;
        };

        self.spawn_cell_editor(
            abs_idx,
            col_idx,
            CellEditTarget::Loaded,
            initial,
            entry,
            window,
            cx,
        );
    }

    // Starts inline editing of a cell in an added row. The added row has no
    // loaded value or key, so the edit writes straight into `added_rows`.
    fn begin_added_cell_edit(
        &mut self,
        abs_idx: usize,
        col_idx: usize,
        added_idx: usize,
        entry: CellEditEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.table_name.is_none() {
            return;
        }
        match self.column_kind_at(col_idx) {
            CellEditorKind::Boolean => {
                self.toggle_boolean_cell_added(col_idx, added_idx, cx);
                return;
            }
            CellEditorKind::Enum(values) => {
                let nullable = self.is_column_nullable_at(col_idx);
                self.enum_popup = Some(EnumPopup {
                    abs_idx,
                    col_idx,
                    added_idx: Some(added_idx),
                    values,
                    nullable,
                });
                cx.notify();
                return;
            }
            _ => {}
        }
        // A Null/Default cell prefills empty; typing a value turns it into Text.
        let initial = match self
            .added_rows
            .get(added_idx)
            .and_then(|row| row.get(col_idx))
        {
            Some(CellValue::Text(text)) => text.clone(),
            _ => String::new(),
        };
        self.spawn_cell_editor(
            abs_idx,
            col_idx,
            CellEditTarget::Added(added_idx),
            initial,
            entry,
            window,
            cx,
        );
    }

    fn first_added_row_edit_column(&self) -> Option<usize> {
        self.visible_columns
            .iter()
            .copied()
            .find(|&col_idx| {
                if matches!(self.column_kind_at(col_idx), CellEditorKind::Boolean) {
                    return false;
                }
                !self.column_infos.as_ref().is_some_and(|infos| {
                    self.result
                        .as_ref()
                        .and_then(|result| result.columns.get(col_idx))
                        .and_then(|column_name| infos.iter().find(|info| &info.name == column_name))
                        .is_some_and(|info| {
                            info.extra.to_ascii_lowercase().contains("auto_increment")
                        })
                })
            })
            .or_else(|| self.visible_columns.first().copied())
    }

    fn begin_added_row_edit(
        &mut self,
        added_idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(col_idx) = self.first_added_row_edit_column() else {
            return;
        };
        let abs_idx = self.loaded_row_count() + added_idx;
        if let Some(display_idx) = self.display_idx_of(abs_idx) {
            self.select_cell_from_click(abs_idx, display_idx, col_idx, false, false);
        }
        self.begin_added_cell_edit(abs_idx, col_idx, added_idx, CellEditEntry::CursorEnd, window, cx);
    }

    fn render_cell_editor(editor: Entity<Editor>, kind: CellEditorKind) -> AnyElement {
        h_flex()
            .size_full()
            .gap_1()
            .items_center()
            .when_some(Self::cell_kind_icon(&kind), |element, (icon_name, _)| {
                element.child(
                    div().flex_none().child(
                        Icon::new(icon_name)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    ),
                )
            })
            .child(div().flex_1().min_w_0().child(editor))
            .into_any_element()
    }

    fn cell_kind_icon(kind: &CellEditorKind) -> Option<(IconName, &'static str)> {
        match kind {
            CellEditorKind::Numeric => Some((IconName::Binary, "Number")),
            CellEditorKind::Date => Some((IconName::CountdownTimer, "Date: YYYY-MM-DD")),
            CellEditorKind::DateTime => Some((IconName::Clock, "Date-time: YYYY-MM-DD HH:MM:SS")),
            _ => None,
        }
    }

    fn render_typed_cell_body(body: AnyElement, kind: CellEditorKind) -> AnyElement {
        h_flex()
            .size_full()
            .gap_1()
            .items_center()
            .when_some(Self::cell_kind_icon(&kind), |element, (icon_name, _)| {
                element.child(
                    div().flex_none().child(
                        Icon::new(icon_name)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    ),
                )
            })
            .child(div().flex_1().min_w_0().overflow_hidden().child(body))
            .into_any_element()
    }

    // Builds the overlay editor shared by loaded and added cell edits.
    fn spawn_cell_editor(
        &mut self,
        abs_idx: usize,
        col_idx: usize,
        target: CellEditTarget,
        initial: String,
        entry: CellEditEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let placeholder = match self.column_kind_at(col_idx) {
            CellEditorKind::Numeric => Some("number"),
            CellEditorKind::Date => Some("YYYY-MM-DD"),
            CellEditorKind::DateTime => Some("YYYY-MM-DD HH:MM:SS"),
            _ => None,
        };
        let editor = cx.new(|cx| {
            let mut ed = Editor::single_line(window, cx);
            if let Some(placeholder) = placeholder {
                ed.set_placeholder_text(placeholder, window, cx);
            }
            ed.set_text(initial, window, cx);
            // CursorEnd (double-click / F2 / move) leaves the caret at the end so
            // the existing value is kept and editable. Only Replace selects all,
            // so a type-to-replace entry overwrites on the first keystroke.
            if entry == CellEditEntry::Replace {
                ed.select_all(&Default::default(), window, cx);
            }
            ed
        });
        let subscription = cx.subscribe_in(&editor, window, |this, _editor, event, window, cx| {
            match event {
                EditorEvent::Focused => {
                    if let Some(edit) = this.cell_edit.as_mut() {
                        edit.commit_armed = true;
                    }
                }
                EditorEvent::Blurred => {
                    // Ignore the blur that can fire while focus is still settling
                    // on open; only commit once the editor has actually focused.
                    if this.cell_edit.as_ref().is_some_and(|edit| edit.commit_armed) {
                        this.commit_cell_edit(window, cx);
                    }
                }
                _ => {}
            }
        });

        self.status_message = None;
        // An inline edit owns the keyboard; close any value-editor popup that a
        // click may have auto-opened so the two don't fight over focus.
        self.value_editor_open = false;
        self.selected_cell = Some((abs_idx, col_idx));
        if let Some(disp) = self.display_idx_of(abs_idx) {
            self.record_view_row = Some(disp);
        }
        self.cell_edit = Some(CellEdit {
            abs_idx,
            col_idx,
            target,
            editor: editor.clone(),
            commit_armed: false,
            _subscription: subscription,
        });
        // Focus before notify so the caret and keyboard land in the editor on the
        // first click rather than a frame later.
        editor.update(cx, |editor, cx| {
            let handle = editor.focus_handle(cx);
            window.focus(&handle, cx);
        });
        cx.notify();
    }

    // Loads the table's primary-key column names and FK metadata once and caches
    // them. Falls back to an empty list (no usable key) on any failure, so a
    // later edit degrades to "kept locally, not persisted" rather than running
    // an unsafe UPDATE.
    fn ensure_primary_key_columns(&mut self, cx: &mut Context<Self>) {
        if self.primary_key_columns.is_some() {
            return;
        }
        let (Some(store), Some(conn_id), Some(db), Some(table)) = (
            self.store.clone(),
            self.connection_id,
            self.database.clone(),
            self.table_name.clone(),
        ) else {
            return;
        };
        let Some(s) = store.upgrade() else {
            return;
        };
        let col_task = s.update(cx, |store, cx| {
            store.describe_table(conn_id, db.clone(), table.clone(), cx)
        });
        let fk_task = s.update(cx, |store, cx| {
            store.list_foreign_keys(conn_id, db, table, cx)
        });
        cx.spawn(async move |this, cx| {
            let (columns, fks) = futures::join!(col_task, fk_task);
            this.update(cx, |this, cx| {
                let all_cols = columns.unwrap_or_default();
                let keys = all_cols
                    .iter()
                    .filter(|col| col.column_key.as_deref() == Some("PRI"))
                    .map(|col| col.name.clone())
                    .collect::<Vec<_>>();
                this.primary_key_columns = Some(keys);
                // Build FK column map: result-column index → FkInfo.
                let fk_list = fks.unwrap_or_default();
                this.fk_columns = all_cols
                    .iter()
                    .enumerate()
                    .filter_map(|(i, col)| {
                        fk_list
                            .iter()
                            .find(|fk| fk.from_column == col.name)
                            .map(|fk| (i, fk.clone()))
                    })
                    .collect();
                this.column_infos = Some(all_cols);
                cx.notify();
            })
            .ok();
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn cancel_cell_edit(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.cell_edit.take().is_some() {
            cx.notify();
        }
    }

    // Commits the current cell edit then immediately opens the editor on an
    // adjacent cell. delta_col / delta_row are -1, 0, or +1.
    // Only navigates within loaded rows; added rows are committed with no move.
    fn commit_and_move(
        &mut self,
        delta_col: i64,
        delta_row: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let edit = match self.cell_edit.as_ref() {
            Some(e) => (
                e.abs_idx,
                e.col_idx,
                matches!(e.target, CellEditTarget::Loaded),
            ),
            None => return,
        };
        let (abs_idx, col_idx, is_loaded) = edit;
        if !self.commit_cell_edit(window, cx) {
            return;
        }
        if !is_loaded {
            return;
        }
        let row_count = self.result.as_ref().map_or(0, |r| r.rows.len());
        let vis_cols = self.visible_columns.clone();
        let vis_count = vis_cols.len();
        if vis_count == 0 || row_count == 0 {
            return;
        }
        let new_abs = (abs_idx as i64 + delta_row).clamp(0, row_count as i64 - 1) as usize;
        let new_col = if delta_col != 0 {
            let cur_vis = vis_cols.iter().position(|&c| c == col_idx).unwrap_or(0);
            let new_vis = (cur_vis as i64 + delta_col).rem_euclid(vis_count as i64) as usize;
            vis_cols[new_vis]
        } else {
            col_idx
        };
        self.begin_cell_edit(new_abs, new_col, CellEditEntry::CursorEnd, window, cx);
    }

    // Commits the inline edit into the local buffer (no SQL runs here). The grid
    // shows the pending value immediately; the change is written only on Submit.
    // If the edited value equals the loaded original, the buffer entry is dropped
    // so a no-op never lingers as a "pending" change.
    // Returns true on success, false when validation rejected the input. On false
    // the editor stays alive so the user can correct the value.
    fn commit_cell_edit(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(edit_ref) = self.cell_edit.as_ref() else {
            return true;
        };
        let abs_idx = edit_ref.abs_idx;
        let col_idx = edit_ref.col_idx;
        let editor = edit_ref.editor.clone();
        let target = edit_ref.target;

        let raw_text = editor.read(cx).text(cx);

        // Reject non-numeric input for numeric columns; keep the editor open.
        if matches!(self.column_kind_at(col_idx), CellEditorKind::Numeric) {
            if !is_valid_numeric(&raw_text) {
                self.status_message = Some(format!("Not a valid number: {raw_text}"));
                cx.notify();
                return false;
            }
        }

        if matches!(self.column_kind_at(col_idx), CellEditorKind::Date) {
            if !is_valid_date(&raw_text) {
                self.status_message = Some(format!("Expected YYYY-MM-DD, got: {raw_text}"));
                cx.notify();
                return false;
            }
        }

        if matches!(self.column_kind_at(col_idx), CellEditorKind::DateTime) {
            if !is_valid_datetime(&raw_text) {
                self.status_message =
                    Some(format!("Expected YYYY-MM-DD HH:MM:SS, got: {raw_text}"));
                cx.notify();
                return false;
            }
        }

        // Validation passed — remove the editor from the slot.
        self.cell_edit.take();
        let new_value = CellValue::from_text(raw_text);

        // Added rows have no loaded value or key: the edit writes straight into
        // the added-row buffer and is submitted as part of the INSERT.
        if let CellEditTarget::Added(added_idx) = target {
            if let Some(cell) = self
                .added_rows
                .get_mut(added_idx)
                .and_then(|row| row.get_mut(col_idx))
            {
                *cell = new_value;
            }
            cx.notify();
            return true;
        }

        self.buffer_loaded_cell_value(abs_idx, col_idx, new_value, cx);
        true
    }

    // Buffers a new value for a loaded cell, mirroring the no-op handling Submit
    // relies on: an edit equal to the current displayed value changes nothing, and
    // an edit equal to the loaded original drops the buffer entry. Shared by the
    // inline editor and the Set NULL / Set DEFAULT actions.
    fn buffer_loaded_cell_value(
        &mut self,
        abs_idx: usize,
        col_idx: usize,
        new_value: CellValue,
        cx: &mut Context<Self>,
    ) {
        // The original is the value loaded from the database: an existing entry
        // already holds it; otherwise the current loaded cell is the original.
        let loaded = self.loaded_cell_value(abs_idx, col_idx);
        let original = self
            .pending_edits
            .get(&(abs_idx, col_idx))
            .map(|edit| edit.original.clone())
            .unwrap_or(loaded);

        let current = self
            .pending_edits
            .get(&(abs_idx, col_idx))
            .map(|edit| edit.new_value.clone())
            .unwrap_or_else(|| CellValue::from_loaded(&original));
        if new_value == current {
            cx.notify();
            return;
        }
        if new_value == CellValue::from_loaded(&original) {
            self.pending_edits.remove(&(abs_idx, col_idx));
            cx.notify();
            return;
        }

        // The edit is buffered regardless of persistability, so the grid keeps
        // showing it; if the row cannot be safely targeted, a status note
        // explains it and Submit will refuse it rather than guess a WHERE clause.
        if let Err(note) = self.can_persist_row(abs_idx) {
            self.status_message = Some(note);
        } else {
            self.status_message = None;
        }
        self.pending_edits.insert(
            (abs_idx, col_idx),
            PendingEdit {
                original,
                new_value,
            },
        );
        cx.notify();
    }

    // The raw value loaded from the database for a cell, ignoring any pending
    // edit. Cheap; safe per cell.
    fn loaded_cell_value(&self, abs_idx: usize, col_idx: usize) -> Option<String> {
        self.result
            .as_ref()
            .and_then(|result| result.rows.get(abs_idx))
            .and_then(|row| row.get(col_idx))
            .cloned()
            .flatten()
    }

    // The buffered value the grid shows for a loaded cell, if a pending edit
    // exists for it. Cheap HashMap lookup; safe per cell.
    fn pending_cell_value(&self, abs_idx: usize, col_idx: usize) -> Option<&CellValue> {
        self.pending_edits
            .get(&(abs_idx, col_idx))
            .map(|edit| &edit.new_value)
    }

    // The editor kind for a column, resolved from cached column metadata.
    // Falls back to Text when no metadata has been loaded yet.
    fn column_kind_at(&self, col_idx: usize) -> CellEditorKind {
        let col_name = self.result.as_ref().and_then(|r| r.columns.get(col_idx));
        let (Some(col_name), Some(infos)) = (col_name, self.column_infos.as_ref()) else {
            return CellEditorKind::Text;
        };
        let Some(info) = infos.iter().find(|ci| &ci.name == col_name) else {
            return CellEditorKind::Text;
        };
        column_editor_kind(&info.data_type)
    }

    fn is_column_nullable_at(&self, col_idx: usize) -> bool {
        let col_name = self.result.as_ref().and_then(|r| r.columns.get(col_idx));
        let (Some(col_name), Some(infos)) = (col_name, self.column_infos.as_ref()) else {
            return false;
        };
        infos
            .iter()
            .find(|ci| &ci.name == col_name)
            .map(|ci| ci.is_nullable)
            .unwrap_or(false)
    }

    fn toggle_boolean_cell_loaded(
        &mut self,
        abs_idx: usize,
        col_idx: usize,
        cx: &mut Context<Self>,
    ) {
        if self.table_name.is_none() {
            return;
        }
        let current = match self.pending_cell_value(abs_idx, col_idx) {
            Some(cv) => cv.clone(),
            None => CellValue::from_loaded(&self.loaded_cell_value(abs_idx, col_idx)),
        };
        let new = toggle_bool_value(&current);
        self.buffer_loaded_cell_value(abs_idx, col_idx, new, cx);
    }

    fn toggle_boolean_cell_added(
        &mut self,
        col_idx: usize,
        added_idx: usize,
        cx: &mut Context<Self>,
    ) {
        if self.table_name.is_none() {
            return;
        }
        let Some(cell) = self
            .added_rows
            .get(added_idx)
            .and_then(|row| row.get(col_idx))
        else {
            return;
        };
        let new = toggle_bool_value(cell);
        if let Some(cell) = self
            .added_rows
            .get_mut(added_idx)
            .and_then(|row| row.get_mut(col_idx))
        {
            *cell = new;
        }
        cx.notify();
    }

    // Applies the enum popup selection and closes the popup.
    fn apply_enum_selection(&mut self, value: CellValue, cx: &mut Context<Self>) {
        let Some(popup) = self.enum_popup.take() else {
            return;
        };
        if let Some(added_idx) = popup.added_idx {
            if let Some(cell) = self
                .added_rows
                .get_mut(added_idx)
                .and_then(|row| row.get_mut(popup.col_idx))
            {
                *cell = value;
            }
        } else {
            self.buffer_loaded_cell_value(popup.abs_idx, popup.col_idx, value, cx);
        }
        cx.notify();
    }

    // Sets the selected cell to a NULL or DEFAULT buffered value. Works for loaded
    // cells (into `pending_edits`) and added cells (into `added_rows`). Mirrors
    // `commit_cell_edit`'s no-op handling for loaded cells.
    fn set_selected_cell_value(&mut self, value: CellValue, cx: &mut Context<Self>) {
        if self.table_name.is_none() {
            self.status_message = Some("Row operations need a table-backed result.".to_string());
            cx.notify();
            return;
        }
        let Some((abs_idx, col_idx)) = self.selected_cell else {
            self.status_message = Some("Select a cell first.".to_string());
            cx.notify();
            return;
        };
        // Drop any inline edit on this cell so its blur-commit does not overwrite
        // the value we are about to set.
        self.cell_edit = None;

        let loaded_count = self.loaded_row_count();
        if abs_idx >= loaded_count {
            let added_idx = abs_idx - loaded_count;
            if let Some(cell) = self
                .added_rows
                .get_mut(added_idx)
                .and_then(|row| row.get_mut(col_idx))
            {
                *cell = value;
            }
            self.status_message = None;
            cx.notify();
            return;
        }
        self.buffer_loaded_cell_value(abs_idx, col_idx, value, cx);
    }

    // True for a table-backed result that can receive row operations. INSERT
    // needs only the table; DELETE additionally needs a usable primary key,
    // which the submit-time guard enforces per row.
    fn row_ops_enabled(&self) -> bool {
        self.table_name.is_some()
            && self.store.is_some()
            && self.connection_id.is_some()
            && self.database.is_some()
    }

    // Number of loaded rows in the result (0 when there is no result). Added rows
    // render after these, so this is the boundary between loaded and added rows.
    fn loaded_row_count(&self) -> usize {
        self.result.as_ref().map_or(0, |result| result.rows.len())
    }

    fn select_row_for_context_action(&mut self, abs_idx: usize, cell_idx: usize) {
        self.selected_rows.clear();
        self.selected_rows.insert(abs_idx);
        self.last_selected_row = self.display_idx_of(abs_idx);
        self.selected_cell = Some((abs_idx, cell_idx));
        self.selected_cell_range = None;
    }

    // Toggles deletion for all selected loaded rows. A second toggle un-marks
    // them. Falls back to the selected cell when no rows are selected.
    // Added rows are not deletable this way; they are dropped via Revert.
    fn toggle_delete_selected_row(&mut self, cx: &mut Context<Self>) {
        if !self.row_ops_enabled() {
            self.status_message = Some("Row operations need a table-backed result.".to_string());
            cx.notify();
            return;
        }
        let loaded_count = self.loaded_row_count();
        let selected_abs_rows: Vec<usize> = if !self.selected_rows.is_empty() {
            self.selected_rows.iter().copied().collect()
        } else if let Some((abs_idx, _)) = self.selected_cell {
            vec![abs_idx]
        } else {
            vec![]
        };
        let mut loaded_rows = Vec::new();
        let mut added_rows = Vec::new();
        for abs_idx in selected_abs_rows {
            if abs_idx < loaded_count {
                loaded_rows.push(abs_idx);
            } else if let Some(added_idx) = abs_idx.checked_sub(loaded_count) {
                if added_idx < self.added_rows.len() {
                    added_rows.push(added_idx);
                }
            }
        }
        if loaded_rows.is_empty() && added_rows.is_empty() {
            self.status_message = Some("Select a row to delete first.".to_string());
            cx.notify();
            return;
        }
        for abs_idx in loaded_rows {
            if !self.deleted_rows.remove(&abs_idx) {
                self.deleted_rows.insert(abs_idx);
            }
        }
        let removed_added_rows = !added_rows.is_empty();
        added_rows.sort_unstable();
        added_rows.dedup();
        for added_idx in added_rows.into_iter().rev() {
            self.remove_added_row(added_idx);
        }
        if removed_added_rows {
            self.selected_rows.retain(|&abs_idx| abs_idx < loaded_count);
            if let Some((abs_idx, _)) = self.selected_cell {
                if abs_idx >= loaded_count {
                    self.selected_cell = None;
                }
            }
            self.selected_cell_range = None;
        } else if let Some((abs_idx, _)) = self.selected_cell {
            if abs_idx >= loaded_count + self.added_rows.len() {
                self.selected_cell = None;
            }
        }
        self.ensure_primary_key_columns(cx);
        self.status_message = None;
        cx.notify();
    }

    fn add_blank_row(&mut self, cx: &mut Context<Self>) {
        self.add_blank_row_after(None, cx);
    }

    fn add_blank_row_after(
        &mut self,
        after_abs_idx: Option<usize>,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        if !self.row_ops_enabled() {
            self.status_message = Some("Row operations need a table-backed result.".to_string());
            cx.notify();
            return None;
        }
        let Some(col_count) = self.result.as_ref().map(|result| result.columns.len()) else {
            self.status_message = Some("No result to add a row to.".to_string());
            cx.notify();
            return None;
        };
        let added_idx = self.added_rows.len();
        self.added_rows.push(vec![CellValue::Null; col_count]);
        self.added_row_anchors.push(
            after_abs_idx
                .map(|abs_idx| self.anchor_for_abs_idx(abs_idx))
                .unwrap_or(AddedRowAnchor::End),
        );
        self.status_message = None;
        cx.notify();
        Some(added_idx)
    }

    // Duplicates the selected row (loaded or added) into a new added row, so it
    // submits as an INSERT. When multiple rows are selected the first one is cloned.
    // Pending cell edits are carried into the clone for loaded rows.
    fn clone_selected_row(&mut self, cx: &mut Context<Self>) {
        if !self.row_ops_enabled() {
            self.status_message = Some("Row operations need a table-backed result.".to_string());
            cx.notify();
            return;
        }
        let source_abs_idx = if self.selected_rows.is_empty() {
            self.selected_cell.map(|(row, _)| row)
        } else {
            self.selected_rows
                .iter()
                .copied()
                .min_by_key(|abs_idx| self.display_idx_of(*abs_idx).unwrap_or(*abs_idx))
        };
        let Some(source_abs_idx) = source_abs_idx else {
            self.status_message = Some("Select a row to clone first.".to_string());
            cx.notify();
            return;
        };
        self.clone_row_after(source_abs_idx, source_abs_idx, cx);
    }

    fn clone_row_after(
        &mut self,
        source_abs_idx: usize,
        after_abs_idx: usize,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        if !self.row_ops_enabled() {
            self.status_message = Some("Row operations need a table-backed result.".to_string());
            cx.notify();
            return None;
        }
        let loaded_count = self.loaded_row_count();
        let clone = if source_abs_idx < loaded_count {
            let Some(col_count) = self.result.as_ref().map(|result| result.columns.len()) else {
                return None;
            };
            (0..col_count)
                .map(
                    |col_idx| match self.pending_cell_value(source_abs_idx, col_idx) {
                        Some(value) => value.clone(),
                        None => {
                            CellValue::from_loaded(&self.loaded_cell_value(source_abs_idx, col_idx))
                        }
                    },
                )
                .collect()
        } else {
            let added_idx = source_abs_idx - loaded_count;
            let Some(row) = self.added_rows.get(added_idx) else {
                return None;
            };
            row.clone()
        };
        let added_idx = self.added_rows.len();
        self.added_rows.push(clone);
        self.added_row_anchors
            .push(self.anchor_for_abs_idx(after_abs_idx));
        self.status_message = None;
        cx.notify();
        Some(added_idx)
    }

    // Checks whether a single row can be safely targeted by a PK-based WHERE
    // clause: a table connection exists, the primary key is loaded and non-empty,
    // every key column is present in the result, and no key value was truncated
    // (a truncated key would match the wrong row). Returns the user-facing note
    // on failure, reusing the wording shown elsewhere.
    fn can_persist_row(&self, abs_idx: usize) -> Result<(), String> {
        if self.table_name.is_none() || self.store.is_none() {
            return Err("Edit kept in grid: no table connection to write to.".to_string());
        }
        let Some(result) = self.result.as_ref() else {
            return Err("Edit kept in grid: no result to write back to.".to_string());
        };
        let Some(key_columns) = self.primary_key_columns.as_ref() else {
            return Err("Edit kept in grid: still loading the table key.".to_string());
        };
        if key_columns.is_empty() {
            return Err(
                "Edit kept in grid: this table has no primary key to target a single row."
                    .to_string(),
            );
        }
        let Some(row) = result.rows.get(abs_idx) else {
            return Err("Edit kept in grid: row is no longer in the result.".to_string());
        };
        for key in key_columns {
            let Some(value_idx) = result.columns.iter().position(|col| col == key) else {
                return Err(
                    "Edit kept in grid: key column is not in the result; not written.".to_string(),
                );
            };
            if let Some(text) = row.get(value_idx).and_then(|cell| cell.as_deref())
                && db_client::is_cell_possibly_truncated(text)
            {
                return Err(
                    "Edit kept in grid: key value was truncated, so it cannot safely target the row."
                        .to_string(),
                );
            }
        }
        Ok(())
    }

    // Submits all buffered changes as DELETE, then UPDATE, then INSERT
    // statements. Building runs up front, so a non-persistable change aborts with
    // a clear note and every buffer is kept. Statements run sequentially on the
    // store. DELETE runs before INSERT so a re-inserted unique key cannot collide
    // with a row that is about to be removed. On any error the buffers are kept
    // so the user does not lose work.
    // Builds the DELETE/UPDATE/INSERT statements for the current buffered
    // changes, in the safe execution order. Pure with respect to the database;
    // shared by Submit, Preview, and Manual-mode staging so they never diverge.
    fn build_pending_statements(&self, cx: &App) -> Result<Vec<String>, String> {
        let (Some(table), Some(result)) = (self.table_name.as_ref(), self.result.as_ref()) else {
            return Err("No table connection to write to.".to_string());
        };
        let key_columns = self.primary_key_columns.clone().unwrap_or_default();
        let quote = self.identifier_quote(cx);

        // Deterministic order so the statements and any tests are stable.
        let mut edits: Vec<((usize, usize), CellValue)> = self
            .pending_edits
            .iter()
            .map(|(&key, edit)| (key, edit.new_value.clone()))
            .collect();
        edits.sort_by_key(|(key, _)| *key);
        let mut deleted: Vec<usize> = self.deleted_rows.iter().copied().collect();
        deleted.sort_unstable();

        let updates = build_pending_updates(
            quote,
            table,
            &result.columns,
            &key_columns,
            &result.rows,
            &edits,
        )?;
        let deletes = build_pending_deletes(
            quote,
            table,
            &result.columns,
            &key_columns,
            &result.rows,
            &deleted,
        )?;
        let inserts: Vec<String> = self
            .added_rows
            .iter()
            .map(|row| build_insert_sql(quote, table, &result.columns, row))
            .collect();
        Ok(combine_row_statements(deletes, updates, inserts))
    }

    fn submit_pending_edits(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending_edits.is_empty()
            && self.deleted_rows.is_empty()
            && self.added_rows.is_empty()
        {
            return;
        }
        let statements = match self.build_pending_statements(cx) {
            Ok(statements) => statements,
            Err(note) => {
                self.status_message = Some(note);
                cx.notify();
                return;
            }
        };

        // Manual mode does not touch the database here; it stages the SQL so the
        // grid keeps showing the pending highlights until an explicit Commit.
        if self.transaction_mode == TransactionMode::Manual {
            let count = statements.len();
            self.staged_statements = statements;
            self.status_message = Some(format!(
                "{count} statement{} staged — Commit to apply.",
                if count == 1 { "" } else { "s" }
            ));
            cx.notify();
            return;
        }

        let (Some(store), Some(conn_id), Some(db)) = (
            self.store.clone(),
            self.connection_id,
            self.database.clone(),
        ) else {
            self.status_message =
                Some("Edit kept in grid: no table connection to write to.".to_string());
            cx.notify();
            return;
        };

        // A row insert or delete changes which rows exist, so refreshing keeps the
        // grid in sync with the database (and shows real auto-increment keys). A
        // DEFAULT edit stores a DB-computed value we cannot know locally, so it
        // also forces a refresh. An edit-only submit otherwise applies in place to
        // preserve the existing behavior.
        let has_default_edit = self
            .pending_edits
            .values()
            .any(|edit| matches!(edit.new_value, CellValue::Default));
        let refresh =
            !self.deleted_rows.is_empty() || !self.added_rows.is_empty() || has_default_edit;

        cx.spawn_in(window, async move |this, cx| {
            for sql in statements {
                let Some(task) = store.upgrade().map(|s| {
                    s.update(cx, |store, cx| {
                        store.execute_query(conn_id, db.clone(), sql, cx)
                    })
                }) else {
                    return Ok(());
                };
                if let Err(err) = task.await {
                    this.update(cx, |this, cx| this.set_error(err.to_string(), cx))
                        .log_err();
                    return Ok(());
                }
            }
            this.update_in(cx, |this, window, cx| {
                if refresh {
                    this.pending_edits.clear();
                    this.deleted_rows.clear();
                    this.added_rows.clear();
                    this.added_row_anchors.clear();
                    this.status_message = None;
                    this.refresh_table_data(window, cx);
                } else {
                    this.apply_pending_edits_to_result();
                    this.pending_edits.clear();
                    this.status_message = None;
                    this.recompute_layout();
                    cx.notify();
                }
            })
            .log_err();
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    // Writes the buffered new values into the loaded result. Called only after a
    // successful Submit, so the grid keeps showing the written values once the
    // buffer is cleared.
    fn apply_pending_edits_to_result(&mut self) {
        let Some(result) = self.result.as_mut() else {
            return;
        };
        for (&(abs_idx, col_idx), edit) in self.pending_edits.iter() {
            if let Some(cell) = result
                .rows
                .get_mut(abs_idx)
                .and_then(|row| row.get_mut(col_idx))
            {
                // A DEFAULT edit forces a refresh instead, so only Text/Null reach
                // here; map them back to the loaded `Option<String>` shape.
                *cell = match &edit.new_value {
                    CellValue::Text(text) => Some(text.clone()),
                    CellValue::Null | CellValue::Default => None,
                };
            }
        }
    }

    // Discards all buffered changes: cell edits, deletions and added rows. The
    // grid renders the loaded values again because rendering reads a buffered
    // value only when an entry exists.
    fn revert_pending_edits(&mut self, cx: &mut Context<Self>) {
        if self.pending_edits.is_empty()
            && self.deleted_rows.is_empty()
            && self.added_rows.is_empty()
        {
            return;
        }
        self.pending_edits.clear();
        self.deleted_rows.clear();
        self.added_rows.clear();
        self.added_row_anchors.clear();
        self.status_message = None;
        cx.notify();
    }

    // Total number of buffered changes shown in the toolbar.
    fn pending_change_count(&self) -> usize {
        self.pending_edits.len() + self.deleted_rows.len() + self.added_rows.len()
    }

    fn toggle_transaction_mode(&mut self, cx: &mut Context<Self>) {
        self.transaction_mode = match self.transaction_mode {
            TransactionMode::Auto => TransactionMode::Manual,
            TransactionMode::Manual => {
                // Leaving Manual with staged work would silently drop it, so
                // discard it explicitly and tell the user.
                if !self.staged_statements.is_empty() {
                    self.staged_statements.clear();
                    self.status_message =
                        Some("Switched to Auto; staged changes were discarded.".to_string());
                }
                TransactionMode::Auto
            }
        };
        cx.notify();
    }

    // Runs the staged statements as one transaction. On success the buffers and
    // staged list clear and the grid refreshes from the database.
    fn commit_transaction(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.staged_statements.is_empty() {
            return;
        }
        let (Some(store), Some(conn_id), Some(db)) = (
            self.store.clone(),
            self.connection_id,
            self.database.clone(),
        ) else {
            self.status_message = Some("No table connection to commit to.".to_string());
            cx.notify();
            return;
        };
        let transaction = wrap_in_transaction(&self.staged_statements);
        cx.spawn_in(window, async move |this, cx| {
            let Some(task) = store.upgrade().map(|s| {
                s.update(cx, |store, cx| {
                    store.execute_query(conn_id, db.clone(), transaction, cx)
                })
            }) else {
                return Ok(());
            };
            if let Err(err) = task.await {
                this.update(cx, |this, cx| this.set_error(err.to_string(), cx))
                    .log_err();
                return Ok(());
            }
            this.update_in(cx, |this, window, cx| {
                this.staged_statements.clear();
                this.pending_edits.clear();
                this.deleted_rows.clear();
                this.added_rows.clear();
                this.added_row_anchors.clear();
                this.status_message = None;
                this.refresh_table_data(window, cx);
            })
            .log_err();
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    // Discards the staged statements and the buffered changes that produced them.
    fn rollback_transaction(&mut self, cx: &mut Context<Self>) {
        if self.staged_statements.is_empty()
            && self.pending_edits.is_empty()
            && self.deleted_rows.is_empty()
            && self.added_rows.is_empty()
        {
            return;
        }
        self.staged_statements.clear();
        self.pending_edits.clear();
        self.deleted_rows.clear();
        self.added_rows.clear();
        self.added_row_anchors.clear();
        self.status_message = None;
        cx.notify();
    }

    // Copies the selected column's aggregate summary (the same text shown in the
    // status bar) to the clipboard.
    fn copy_aggregation_to_clipboard(&mut self, cx: &mut Context<Self>) {
        let Some(result) = self.result.as_ref() else {
            return;
        };
        let Some((_, col_idx)) = self.selected_cell else {
            self.status_message = Some("Select a cell to copy its column aggregates.".to_string());
            cx.notify();
            return;
        };
        if col_idx >= result.columns.len() {
            return;
        }
        let column_name = result.columns.get(col_idx).cloned().unwrap_or_default();
        let summary =
            Self::compute_column_aggregates(result, col_idx, &self.filtered_display_order);
        cx.write_to_clipboard(ClipboardItem::new_string(format!(
            "{column_name}  {summary}"
        )));
        self.status_message = None;
        cx.notify();
    }

    // Sets a column's local filter to match (or exclude) the given cell value.
    // Opens the filter row if hidden so the predicate is visible and editable.
    fn apply_quick_filter(
        &mut self,
        col_idx: usize,
        value: String,
        exclude: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.local_filter_visible {
            self.toggle_local_filter_row(window, cx);
        }
        let text = if exclude { format!("!{value}") } else { value };
        if let Some(editor) = self.local_filter_editors.get(col_idx).cloned() {
            editor.update(cx, |editor, cx| editor.set_text(text, window, cx));
        }
        self.recompute_local_filter(cx);
    }

    fn toggle_goto_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.goto_row_visible {
            self.close_goto_row(cx);
        } else {
            self.open_goto_row(window, cx);
        }
    }

    fn open_goto_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.goto_row_editor.is_none() {
            let editor = cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_placeholder_text("Row number", window, cx);
                editor
            });
            self.goto_row_editor = Some(editor);
        }
        self.goto_row_visible = true;
        if let Some(editor) = self.goto_row_editor.clone() {
            let handle = editor.focus_handle(cx);
            window.focus(&handle, cx);
        }
        cx.notify();
    }

    fn close_goto_row(&mut self, cx: &mut Context<Self>) {
        self.goto_row_visible = false;
        cx.notify();
    }

    // Parses the go-to-row input (a plain row number, or `column:row`), then
    // scrolls to and selects that row in the current display order.
    fn confirm_goto_row(&mut self, cx: &mut Context<Self>) {
        let raw = self
            .goto_row_editor
            .as_ref()
            .map(|editor| editor.read(cx).text(cx))
            .unwrap_or_default();
        let row_token = raw.rsplit(':').next().unwrap_or("").trim();
        let Ok(row_number) = row_token.parse::<usize>() else {
            self.status_message = Some("Enter a row number to jump to.".to_string());
            cx.notify();
            return;
        };
        let total = self.filtered_display_order.len();
        if total == 0 {
            return;
        }
        let display_idx = row_number.saturating_sub(1).min(total - 1);
        if let Some(&abs_idx) = self.filtered_display_order.get(display_idx) {
            self.selected_rows.clear();
            self.selected_rows.insert(abs_idx);
            self.last_selected_row = Some(display_idx);
        }
        self.scroll_handle
            .scroll_to_item(display_idx, gpui::ScrollStrategy::Center);
        self.goto_row_visible = false;
        cx.notify();
    }

    // The identifier-quoting character for the active connection's driver.
    fn identifier_quote(&self, cx: &App) -> char {
        let driver = self
            .store
            .as_ref()
            .zip(self.connection_id)
            .and_then(|(store, conn_id)| {
                store.upgrade().and_then(|s| {
                    s.read(cx)
                        .connections()
                        .iter()
                        .find(|c| c.config.id == conn_id)
                        .map(|c| c.config.driver)
                })
            });
        match driver {
            Some(DatabaseDriver::MySQL) => '`',
            Some(_) => '"',
            None => '`',
        }
    }

    // Wraps a read-only statement so a page can be sliced from it without
    // mutating the user's own ordering or clauses.
    fn wrap_paged(base_sql: &str, limit: usize, offset: usize) -> String {
        let trimmed = base_sql.trim().trim_end_matches(';').trim();
        format!("SELECT * FROM (\n{trimmed}\n) AS db_client_page LIMIT {limit} OFFSET {offset}")
    }

    fn export_tsv(result: &QueryResult) -> String {
        let mut out = String::new();
        out.push_str(&result.columns.join("\t"));
        out.push('\n');
        for row in &result.rows {
            let cells: Vec<&str> = row.iter().map(|c| c.as_deref().unwrap_or("")).collect();
            out.push_str(&cells.join("\t"));
            out.push('\n');
        }
        out
    }

    fn export_sql_insert(result: &QueryResult, table: &str) -> String {
        Self::export_sql_insert_with_quote(result, table, '`')
    }

    fn export_sql_insert_with_quote(result: &QueryResult, table: &str, quote: char) -> String {
        if result.rows.is_empty() {
            return String::new();
        }
        let cols = result
            .columns
            .iter()
            .map(|column| quote_identifier(quote, column))
            .collect::<Vec<_>>()
            .join(", ");
        let mut out = String::new();
        for row in &result.rows {
            let values = Self::row_sql_values(row);
            out.push_str(&format!(
                "INSERT INTO {} ({}) VALUES ({});\n",
                quote_identifier(quote, table),
                cols,
                values
            ));
        }
        out
    }

    fn export_sql_multi_insert(result: &QueryResult, table: &str, quote: char) -> String {
        if result.rows.is_empty() {
            return String::new();
        }
        let cols = result
            .columns
            .iter()
            .map(|column| quote_identifier(quote, column))
            .collect::<Vec<_>>()
            .join(", ");
        let values = result
            .rows
            .iter()
            .map(|row| format!("({})", Self::row_sql_values(row)))
            .collect::<Vec<_>>()
            .join(",\n  ");

        format!(
            "INSERT INTO {} ({}) VALUES\n  {};\n",
            quote_identifier(quote, table),
            cols,
            values
        )
    }

    fn row_sql_values(row: &[Option<String>]) -> String {
        row.iter()
            .map(|cell| match cell.as_deref() {
                None => "NULL".to_string(),
                Some(value) => format!("'{}'", value.replace('\'', "''")),
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn export_markdown(result: &QueryResult) -> String {
        let widths: Vec<usize> = result
            .columns
            .iter()
            .enumerate()
            .map(|(i, col)| {
                result
                    .rows
                    .iter()
                    .map(|row| row.get(i).and_then(|c| c.as_deref()).unwrap_or("").len())
                    .fold(col.len().max(3), usize::max)
            })
            .collect();

        let mut out = String::new();
        // Header row
        out.push('|');
        for (col, w) in result.columns.iter().zip(&widths) {
            out.push_str(&format!(" {:w$} |", col, w = w));
        }
        out.push('\n');
        // Separator
        out.push('|');
        for w in &widths {
            out.push_str(&format!(" {} |", "-".repeat(*w)));
        }
        out.push('\n');
        // Data rows
        for row in &result.rows {
            out.push('|');
            for (i, w) in widths.iter().enumerate() {
                let cell = row.get(i).and_then(|c| c.as_deref()).unwrap_or("NULL");
                out.push_str(&format!(" {:w$} |", cell, w = w));
            }
            out.push('\n');
        }
        out
    }

    fn export_csv(result: &QueryResult) -> String {
        let mut out = String::new();
        out.push_str(&result.columns.join(","));
        out.push('\n');
        for row in &result.rows {
            let cells: Vec<String> = row
                .iter()
                .map(|c| {
                    let s = c.as_deref().unwrap_or("");
                    if s.contains(',') || s.contains('"') || s.contains('\n') {
                        format!("\"{}\"", s.replace('"', "\"\""))
                    } else {
                        s.to_string()
                    }
                })
                .collect();
            out.push_str(&cells.join(","));
            out.push('\n');
        }
        out
    }

    fn export_json(result: &QueryResult) -> String {
        let rows: Vec<String> = result
            .rows
            .iter()
            .map(|row| {
                let pairs: Vec<String> = result
                    .columns
                    .iter()
                    .zip(row.iter())
                    .map(|(col, cell)| match cell {
                        Some(v) => format!("\"{}\":\"{}\"", col, v.replace('"', "\\\"")),
                        None => format!("\"{}\":null", col),
                    })
                    .collect();
                format!("{{{}}}", pairs.join(","))
            })
            .collect();
        format!("[{}]", rows.join(","))
    }

    // Builds a result where rows and columns are swapped: the first output
    // column lists the original column names, and each following column is one
    // original record. Used by the export "Transpose" option.
    fn transpose_result(result: &QueryResult) -> QueryResult {
        let mut columns = Vec::with_capacity(result.rows.len() + 1);
        columns.push("column".to_string());
        for index in 0..result.rows.len() {
            columns.push((index + 1).to_string());
        }
        let rows = result
            .columns
            .iter()
            .enumerate()
            .map(|(col_idx, name)| {
                let mut row = Vec::with_capacity(result.rows.len() + 1);
                row.push(Some(name.clone()));
                for source_row in &result.rows {
                    row.push(source_row.get(col_idx).cloned().flatten());
                }
                row
            })
            .collect();
        QueryResult {
            columns,
            rows,
            rows_affected: 0,
            execution_time_ms: 0,
        }
    }

    fn drop_first_line(text: &str) -> String {
        match text.split_once('\n') {
            Some((_, rest)) => rest.to_string(),
            None => String::new(),
        }
    }

    // Removes the <thead>...</thead> block so an HTML export without headers
    // keeps only the body rows.
    fn strip_html_thead(html: &str) -> String {
        if let (Some(start), Some(end)) = (html.find("<thead>"), html.find("</thead>")) {
            let mut out = String::with_capacity(html.len());
            out.push_str(&html[..start]);
            out.push_str(&html[end + "</thead>".len()..]);
            out
        } else {
            html.to_string()
        }
    }

    // Assembles the export payload for the dialog: applies transpose, the chosen
    // format, the header toggle, and an optional DDL prefix. Reuses the existing
    // per-format writers so there is one source of formatting truth.
    fn build_export_text(
        result: &QueryResult,
        choice: ExportChoice,
        include_headers: bool,
        transpose: bool,
        table: Option<&str>,
        ddl: Option<&str>,
    ) -> String {
        let transposed;
        let source = if transpose {
            transposed = Self::transpose_result(result);
            &transposed
        } else {
            result
        };
        let table_name = table.unwrap_or("exported_table");
        let mut body = match choice {
            ExportChoice::Csv => Self::export_csv(source),
            ExportChoice::Tsv => Self::export_tsv(source),
            ExportChoice::Json => Self::export_json(source),
            ExportChoice::Markdown => Self::export_markdown(source),
            ExportChoice::Html => Self::export_html(source),
            ExportChoice::SqlInsert => Self::export_sql_insert(source, table_name),
            ExportChoice::SqlMultiInsert => Self::export_sql_multi_insert(source, table_name, '`'),
            ExportChoice::SqlUpdate => Self::export_sql_update(source, table_name),
        };
        if !include_headers && choice.honors_headers() {
            body = match choice {
                ExportChoice::Html => Self::strip_html_thead(&body),
                ExportChoice::Markdown => {
                    // Markdown's first two lines are the header and its separator.
                    Self::drop_first_line(&Self::drop_first_line(&body))
                }
                _ => Self::drop_first_line(&body),
            };
        }
        match ddl {
            Some(ddl) if !ddl.trim().is_empty() => format!("{}\n\n{}", ddl.trim_end(), body),
            _ => body,
        }
    }

    // Extracts (label, value) points from a numeric column, skipping rows whose
    // value does not parse as a number. Labels come from `label_column` when set,
    // otherwise the 1-based row number.
    fn chart_series(
        result: &QueryResult,
        label_column: Option<usize>,
        value_column: usize,
    ) -> Vec<(String, f64)> {
        result
            .rows
            .iter()
            .enumerate()
            .filter_map(|(row_idx, row)| {
                let raw = row.get(value_column)?.as_deref()?;
                let value: f64 = raw.trim().parse().ok()?;
                let label = match label_column.and_then(|col| row.get(col)) {
                    Some(Some(text)) => text.clone(),
                    _ => (row_idx + 1).to_string(),
                };
                Some((label, value))
            })
            .collect()
    }

    // Returns the (min, max) of a series, with the baseline pulled to zero so
    // bars share a common origin. Returns None for an empty series.
    fn series_bounds(series: &[(String, f64)]) -> Option<(f64, f64)> {
        if series.is_empty() {
            return None;
        }
        let mut min = 0.0_f64;
        let mut max = 0.0_f64;
        for (_, value) in series {
            min = min.min(*value);
            max = max.max(*value);
        }
        if (max - min).abs() < f64::EPSILON {
            max = min + 1.0;
        }
        Some((min, max))
    }

    // First column whose every non-null cell parses as a number. Used as the
    // default value axis for the chart.
    fn first_numeric_column(result: &QueryResult) -> Option<usize> {
        (0..result.columns.len()).find(|&col| {
            let mut saw_value = false;
            for row in &result.rows {
                if let Some(Some(text)) = row.get(col) {
                    saw_value = true;
                    if text.trim().parse::<f64>().is_err() {
                        return false;
                    }
                }
            }
            saw_value
        })
    }

    // Detects latitude/longitude columns by name, returning (lat_col, lon_col).
    fn detect_lat_lon(result: &QueryResult) -> Option<(usize, usize)> {
        let find = |names: &[&str]| {
            result.columns.iter().position(|column| {
                let lower = column.to_ascii_lowercase();
                names.iter().any(|name| lower == *name)
            })
        };
        let lat = find(&["lat", "latitude"])?;
        let lon = find(&["lon", "lng", "long", "longitude"])?;
        Some((lat, lon))
    }

    fn copy_cell_value(&self, abs_idx: usize, col_idx: usize) -> Option<String> {
        if let Some(value) = self.pending_cell_value(abs_idx, col_idx) {
            return match value {
                CellValue::Text(text) => Some(text.clone()),
                CellValue::Null => None,
                CellValue::Default => Some(DEFAULT_MARKER.to_string()),
            };
        }
        let loaded_count = self.loaded_row_count();
        if abs_idx < loaded_count {
            return self.loaded_cell_value(abs_idx, col_idx);
        }
        let added_idx = abs_idx.checked_sub(loaded_count)?;
        self.added_rows
            .get(added_idx)
            .and_then(|row| row.get(col_idx))
            .and_then(|value| match value {
                CellValue::Text(text) => Some(text.clone()),
                CellValue::Null => None,
                CellValue::Default => Some(DEFAULT_MARKER.to_string()),
            })
    }

    fn selected_display_rows_for_copy(&self) -> Vec<usize> {
        if !self.selected_rows.is_empty() {
            let loaded_count = self.loaded_row_count();
            return self
                .display_row_entries()
                .into_iter()
                .map(|row| row.abs_idx(loaded_count))
                .filter(|abs_idx| self.selected_rows.contains(abs_idx))
                .collect();
        }

        if let Some(((anchor_abs_idx, _), (end_abs_idx, _))) = self.selected_cell_range {
            let anchor_display_idx = self
                .display_idx_of(anchor_abs_idx)
                .unwrap_or(anchor_abs_idx);
            let end_display_idx = self.display_idx_of(end_abs_idx).unwrap_or(end_abs_idx);
            let lo = anchor_display_idx.min(end_display_idx);
            let hi = anchor_display_idx.max(end_display_idx);
            return (lo..=hi)
                .filter_map(|display_idx| self.abs_idx_at_display_idx(display_idx))
                .collect();
        }

        self.selected_cell
            .map(|(abs_idx, _)| vec![abs_idx])
            .unwrap_or_default()
    }

    fn selected_columns_for_copy(&self, result: &QueryResult) -> Vec<usize> {
        if !self.selected_rows.is_empty() {
            return (0..result.columns.len()).collect();
        }

        if let Some(((_, anchor_col_idx), (_, end_col_idx))) = self.selected_cell_range {
            let lo = anchor_col_idx.min(end_col_idx);
            let hi = anchor_col_idx.max(end_col_idx);
            return (lo..=hi)
                .filter(|col_idx| *col_idx < result.columns.len())
                .collect();
        }

        self.selected_cell
            .and_then(|(_, col_idx)| (col_idx < result.columns.len()).then_some(vec![col_idx]))
            .unwrap_or_default()
    }

    fn selected_result_for_copy(&self) -> Option<QueryResult> {
        let result = self.result.as_ref()?;
        let columns = self.selected_columns_for_copy(result);
        let rows = self.selected_display_rows_for_copy();
        if columns.is_empty() || rows.is_empty() {
            return None;
        }

        Some(QueryResult {
            columns: columns
                .iter()
                .filter_map(|col_idx| result.columns.get(*col_idx).cloned())
                .collect(),
            rows: rows
                .into_iter()
                .map(|abs_idx| {
                    columns
                        .iter()
                        .map(|col_idx| self.copy_cell_value(abs_idx, *col_idx))
                        .collect()
                })
                .collect(),
            rows_affected: 0,
            execution_time_ms: 0,
        })
    }

    fn copy_selected_to_clipboard(&mut self, cx: &mut Context<Self>) {
        let Some(result) = self.selected_result_for_copy() else {
            self.status_message = Some("Select cells or rows to copy.".to_string());
            cx.notify();
            return;
        };
        let table_name = self.table_name.clone();
        let quote = self.identifier_quote(cx);
        let text = match self.copy_format {
            CopyFormat::Tsv => Self::export_tsv(&result),
            CopyFormat::Csv => Self::export_csv(&result),
            CopyFormat::Json => Self::export_json(&result),
            CopyFormat::Markdown => Self::export_markdown(&result),
            CopyFormat::Insert => {
                let Some(table) = table_name else {
                    self.status_message =
                        Some("Copy as INSERT requires a table-backed result.".to_string());
                    cx.notify();
                    return;
                };
                Self::export_sql_insert_with_quote(&result, &table, quote)
            }
            CopyFormat::MultiInsert => {
                let Some(table) = table_name else {
                    self.status_message =
                        Some("Copy as INSERT requires a table-backed result.".to_string());
                    cx.notify();
                    return;
                };
                Self::export_sql_multi_insert(&result, &table, quote)
            }
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.status_message = None;
        cx.notify();
    }

    fn export_html(result: &QueryResult) -> String {
        let mut out = String::from("<table>\n<thead><tr>");
        for col in &result.columns {
            out.push_str("<th>");
            html_escape_into(&mut out, col);
            out.push_str("</th>");
        }
        out.push_str("</tr></thead>\n<tbody>\n");
        for row in &result.rows {
            out.push_str("<tr>");
            for cell in row {
                out.push_str("<td>");
                match cell {
                    Some(v) => html_escape_into(&mut out, v),
                    None => out.push_str("<em>NULL</em>"),
                }
                out.push_str("</td>");
            }
            out.push_str("</tr>\n");
        }
        out.push_str("</tbody>\n</table>");
        out
    }

    // Generates UPDATE statements treating the first column as the primary key.
    fn export_sql_update(result: &QueryResult, table: &str) -> String {
        if result.columns.is_empty() {
            return String::new();
        }
        let pk_col = &result.columns[0];
        let mut out = String::new();
        for row in &result.rows {
            let Some(pk_val) = row.first().and_then(|v| v.as_deref()) else {
                continue;
            };
            let sets: Vec<String> = result
                .columns
                .iter()
                .zip(row.iter())
                .skip(1)
                .map(|(col, cell)| {
                    let val = match cell {
                        Some(v) => sql_literal(Some(v.as_str())),
                        None => "NULL".to_string(),
                    };
                    format!("{} = {}", col, val)
                })
                .collect();
            if sets.is_empty() {
                continue;
            }
            out.push_str(&format!(
                "UPDATE {} SET {} WHERE {} = {};\n",
                table,
                sets.join(", "),
                pk_col,
                sql_literal(Some(pk_val))
            ));
        }
        out
    }

    fn export_xlsx(result: &QueryResult) -> Vec<u8> {
        let buf = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(buf);
        let opts =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);

        let content_types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
<Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
<Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/>
</Types>"#;

        let rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>"#;

        let workbook = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<sheets><sheet name="Sheet1" sheetId="1" r:id="rId1"/></sheets>
</workbook>"#;

        let workbook_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
</Relationships>"#;

        let styles = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<fonts count="2">
<font><sz val="11"/><name val="Calibri"/></font>
<font><b/><sz val="11"/><name val="Calibri"/></font>
</fonts>
<fills count="2"><fill><patternFill patternType="none"/></fill><fill><patternFill patternType="gray125"/></fill></fills>
<borders count="1"><border><left/><right/><top/><bottom/><diagonal/></border></borders>
<cellStyleXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/></cellStyleXfs>
<cellXfs count="2">
<xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0"/>
<xf numFmtId="0" fontId="1" fillId="0" borderId="0" xfId="0"/>
</cellXfs>
</styleSheet>"#;

        fn xml_escape(s: &str) -> String {
            s.replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
                .replace('"', "&quot;")
                .replace('\'', "&apos;")
        }

        fn col_name(idx: usize) -> String {
            let mut n = idx + 1;
            let mut name = String::new();
            while n > 0 {
                n -= 1;
                name.insert(0, (b'A' + (n % 26) as u8) as char);
                n /= 26;
            }
            name
        }

        let mut sheet = String::from(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><sheetData>",
        );

        let header_row: Vec<String> = result
            .columns
            .iter()
            .enumerate()
            .map(|(i, col)| {
                format!(
                    "<c r=\"{}1\" t=\"inlineStr\" s=\"1\"><is><t>{}</t></is></c>",
                    col_name(i),
                    xml_escape(col)
                )
            })
            .collect();
        sheet.push_str(&format!("<row r=\"1\">{}</row>", header_row.join("")));

        for (row_idx, row) in result.rows.iter().enumerate() {
            let row_num = row_idx + 2;
            let cells: Vec<String> = row
                .iter()
                .enumerate()
                .map(|(col_idx, cell)| {
                    let cell_ref = format!("{}{}", col_name(col_idx), row_num);
                    match cell {
                        Some(v) => {
                            if v.parse::<f64>().is_ok() {
                                format!("<c r=\"{}\"><v>{}</v></c>", cell_ref, xml_escape(v))
                            } else {
                                format!(
                                    "<c r=\"{}\" t=\"inlineStr\"><is><t>{}</t></is></c>",
                                    cell_ref,
                                    xml_escape(v)
                                )
                            }
                        }
                        None => {
                            format!("<c r=\"{}\" t=\"inlineStr\"><is><t></t></is></c>", cell_ref)
                        }
                    }
                })
                .collect();
            sheet.push_str(&format!("<row r=\"{}\">{}</row>", row_num, cells.join("")));
        }
        sheet.push_str("</sheetData></worksheet>");

        let _ = zip.start_file("[Content_Types].xml", opts);
        let _ = zip.write_all(content_types.as_bytes());
        let _ = zip.start_file("_rels/.rels", opts);
        let _ = zip.write_all(rels.as_bytes());
        let _ = zip.start_file("xl/workbook.xml", opts);
        let _ = zip.write_all(workbook.as_bytes());
        let _ = zip.start_file("xl/_rels/workbook.xml.rels", opts);
        let _ = zip.write_all(workbook_rels.as_bytes());
        let _ = zip.start_file("xl/styles.xml", opts);
        let _ = zip.write_all(styles.as_bytes());
        let _ = zip.start_file("xl/worksheets/sheet1.xml", opts);
        let _ = zip.write_all(sheet.as_bytes());

        zip.finish().map(|c| c.into_inner()).unwrap_or_default()
    }

    // Parses `text` as CSV or TSV (auto-detected by the first line) and returns
    // a list of rows, each sized to `col_count`. Empty fields map to Null; all
    // others to Text. If the first row looks like a header matching the result
    // column names (case-insensitive), it is skipped.
    fn parse_clipboard_rows(
        text: &str,
        col_count: usize,
        column_names: &[String],
    ) -> Vec<Vec<CellValue>> {
        if col_count == 0 || text.trim().is_empty() {
            return Vec::new();
        }

        // Detect delimiter from the first line.
        let first_line = text.lines().next().unwrap_or("");
        let delimiter = if first_line.contains('\t') { '\t' } else { ',' };

        let parse_row = |line: &str| -> Vec<String> {
            if delimiter == '\t' {
                line.split('\t').map(|s| s.to_string()).collect()
            } else {
                // RFC 4180 CSV: fields may be quoted with `"`, internal `""` → `"`.
                let mut fields = Vec::new();
                let mut chars = line.chars().peekable();
                loop {
                    if chars.peek().is_none() {
                        break;
                    }
                    if chars.peek() == Some(&'"') {
                        chars.next();
                        let mut field = String::new();
                        loop {
                            match chars.next() {
                                None => break,
                                Some('"') => {
                                    if chars.peek() == Some(&'"') {
                                        chars.next();
                                        field.push('"');
                                    } else {
                                        break;
                                    }
                                }
                                Some(c) => field.push(c),
                            }
                        }
                        if chars.peek() == Some(&',') {
                            chars.next();
                        }
                        fields.push(field);
                    } else {
                        let mut field = String::new();
                        loop {
                            match chars.peek() {
                                None | Some(&',') => break,
                                _ => field.push(chars.next().unwrap()),
                            }
                        }
                        if chars.peek() == Some(&',') {
                            chars.next();
                        }
                        fields.push(field);
                    }
                }
                fields
            }
        };

        let mut lines = text.lines().peekable();

        // Check whether the first row is a header.
        let skip_header = if !column_names.is_empty() {
            if let Some(first) = lines.peek() {
                let fields = parse_row(first);
                fields.len() == column_names.len()
                    && fields
                        .iter()
                        .zip(column_names.iter())
                        .all(|(f, col)| f.trim().eq_ignore_ascii_case(col.trim()))
            } else {
                false
            }
        } else {
            false
        };
        if skip_header {
            lines.next();
        }

        lines
            .filter(|l| !l.trim().is_empty())
            .map(|line| {
                let fields = parse_row(line);
                let mut row: Vec<CellValue> = fields
                    .into_iter()
                    .take(col_count)
                    .map(|f| {
                        if f.is_empty() {
                            CellValue::Null
                        } else {
                            CellValue::Text(f)
                        }
                    })
                    .collect();
                // Pad with Null if the row is shorter than col_count.
                while row.len() < col_count {
                    row.push(CellValue::Null);
                }
                row
            })
            .collect()
    }

    // Computes aggregate statistics for a single column across the given display
    // order. Returns a formatted string like
    //   "COUNT 42 | NULLS 3 | MIN 0 | MAX 100 | SUM 1234 | AVG 29.38"
    // For non-numeric columns the SUM / AVG part is omitted.
    fn compute_column_aggregates(
        result: &QueryResult,
        col_idx: usize,
        display_order: &[usize],
    ) -> String {
        let mut count = 0usize;
        let mut null_count = 0usize;
        let mut min_str: Option<String> = None;
        let mut max_str: Option<String> = None;
        let mut numeric_sum = 0.0f64;
        let mut numeric_count = 0usize;

        for &abs_idx in display_order {
            let cell = result.rows.get(abs_idx).and_then(|row| row.get(col_idx));
            match cell.and_then(|c| c.as_deref()) {
                None => null_count += 1,
                Some(val) => {
                    count += 1;
                    min_str = Some(match min_str.take() {
                        None => val.to_string(),
                        Some(m) => {
                            if val < m.as_str() {
                                val.to_string()
                            } else {
                                m
                            }
                        }
                    });
                    max_str = Some(match max_str.take() {
                        None => val.to_string(),
                        Some(m) => {
                            if val > m.as_str() {
                                val.to_string()
                            } else {
                                m
                            }
                        }
                    });
                    if let Ok(n) = val.parse::<f64>() {
                        numeric_sum += n;
                        numeric_count += 1;
                    }
                }
            }
        }

        if numeric_count == count && count > 0 {
            let min_s = min_str.as_deref().unwrap_or("—");
            let max_s = max_str.as_deref().unwrap_or("—");
            let mut out = format!("COUNT {count} | NULLS {null_count} | MIN {min_s} | MAX {max_s}");
            let avg = numeric_sum / count as f64;
            out.push_str(&format!(" | SUM {numeric_sum} | AVG {avg:.2}"));
            out
        } else {
            format!("COUNT {count} | NULLS {null_count}")
        }
    }

    fn render_status_bar(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let result = self.result.as_ref()?;
        let total_rows = result.rows.len();
        let total_cols = result.columns.len();
        let ms = result.execution_time_ms;

        let row_summary = format!(
            "{} row{} · {} col{} · {}ms",
            total_rows,
            if total_rows == 1 { "" } else { "s" },
            total_cols,
            if total_cols == 1 { "" } else { "s" },
            ms,
        );

        let col_summary = self.selected_cell.and_then(|(_, col_idx)| {
            if col_idx < total_cols {
                Some(format!(
                    "{}  {}",
                    result
                        .columns
                        .get(col_idx)
                        .map(|s| s.as_str())
                        .unwrap_or(""),
                    Self::compute_column_aggregates(result, col_idx, &self.filtered_display_order),
                ))
            } else {
                None
            }
        });

        // Show a FK navigation button when the selected cell belongs to a FK column
        // and has a non-null value.
        let fk_button = self.selected_cell.and_then(|(abs_idx, col_idx)| {
            let fk = self.fk_columns.get(&col_idx)?;
            let value = result
                .rows
                .get(abs_idx)
                .and_then(|row| row.get(col_idx))
                .and_then(|v| v.as_deref())?;
            let label = SharedString::from(format!("→ {}", fk.to_table));
            let tooltip = SharedString::from(format!(
                "Navigate to {}.{} = {}",
                fk.to_table, fk.to_column, value
            ));
            Some((label, tooltip))
        });

        Some(
            h_flex()
                .flex_none()
                .h(px(22.))
                .px_2()
                .gap_3()
                .border_t_1()
                .border_color(cx.theme().colors().border)
                .bg(cx.theme().colors().surface_background)
                .items_center()
                .child(
                    Label::new(row_summary)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .when_some(fk_button, |el, (label, tip)| {
                    el.child(
                        Button::new("fk-nav", label)
                            .style(ButtonStyle::Subtle)
                            .tooltip(Tooltip::text(tip))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.navigate_to_fk_row(window, cx);
                            })),
                    )
                })
                .child(div().flex_1())
                .when_some(col_summary, |el, summary| {
                    el.child(
                        Label::new(summary)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                }),
        )
    }

    fn render_query_history_popup(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.history_open || self.query_history.is_empty() {
            return None;
        }

        let items: Vec<AnyElement> = self
            .filtered_history()
            .into_iter()
            .map(|(i, sql)| {
                let preview: String = sql.chars().take(80).collect();
                let preview = if sql.chars().count() > 80 {
                    format!("{}…", preview)
                } else {
                    preview
                };
                let sql_owned = sql;
                div()
                    .id(("history-item", i))
                    .px_2()
                    .py_1()
                    .cursor_pointer()
                    .hover(|el| el.bg(cx.theme().colors().element_hover))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.history_open = false;
                        if let Some(editor) = &this.filter_editor {
                            editor.update(cx, |ed, cx| {
                                ed.set_text(sql_owned.clone(), window, cx);
                            });
                        } else {
                            this.base_sql = Some(sql_owned.clone());
                            this.refresh_table_data(window, cx);
                        }
                        cx.notify();
                    }))
                    .child(
                        Label::new(preview)
                            .size(LabelSize::Small)
                            .color(Color::Default),
                    )
                    .into_any_element()
            })
            .collect();

        let search_box = self.history_search_editor.clone().map(|editor| {
            h_flex()
                .flex_none()
                .px_2()
                .py_1()
                .gap_1()
                .border_b_1()
                .border_color(cx.theme().colors().border_variant)
                .child(
                    Icon::new(IconName::MagnifyingGlass)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                )
                .child(div().flex_1().child(editor))
        });

        Some(
            popup_surface(cx)
                .id("query-history-popup")
                .debug_selector(|| "QUERY_HISTORY_POPUP".to_string())
                .absolute()
                .top_8()
                .left_0()
                .flex()
                .flex_col()
                .min_w(px(360.0))
                .max_h(px(360.0))
                .when_some(search_box, |el, box_| el.child(box_))
                .child(
                    div()
                        .id("query-history-list")
                        .flex_1()
                        .overflow_y_scroll()
                        .children(items),
                )
                .into_any_element(),
        )
    }

    #[allow(dead_code)]
    fn generate_update_sql(table: &str, columns: &[String], row: &[Option<String>]) -> String {
        if columns.is_empty() {
            return format!("UPDATE {} SET  WHERE ;", table);
        }
        let set_clauses: Vec<String> = columns
            .iter()
            .zip(row.iter())
            .map(|(col, val)| match val {
                Some(v) => format!("{} = '{}'", col, v.replace('\'', "''")),
                None => format!("{} = NULL", col),
            })
            .collect();
        let where_clause = columns
            .first()
            .zip(row.first())
            .map(|(col, val)| match val {
                Some(v) => format!("{} = '{}'", col, v.replace('\'', "''")),
                None => format!("{} IS NULL", col),
            })
            .unwrap_or_else(|| "1 = 1".to_string());
        format!(
            "UPDATE {} SET {} WHERE {};",
            table,
            set_clauses.join(", "),
            where_clause
        )
    }

    #[allow(dead_code)]
    fn generate_delete_sql(table: &str, columns: &[String], row: &[Option<String>]) -> String {
        let where_clause = columns
            .first()
            .zip(row.first())
            .map(|(col, val)| match val {
                Some(v) => format!("{} = '{}'", col, v.replace('\'', "''")),
                None => format!("{} IS NULL", col),
            })
            .unwrap_or_else(|| "1 = 1".to_string());
        format!("DELETE FROM {} WHERE {};", table, where_clause)
    }

    #[allow(dead_code)]
    fn edit_row_as_sql(
        &self,
        row: &[Option<String>],
        columns: &[String],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.clone() else {
            return;
        };
        let table = self.table_name.clone().unwrap_or_default();
        let sql = Self::generate_update_sql(&table, columns, row);
        Self::open_sql_in_workspace(workspace, sql, window, cx);
    }

    #[allow(dead_code)]
    fn open_sql_in_workspace(
        workspace: WeakEntity<Workspace>,
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
                    cx.spawn_in(window, async move |workspace, cx| {
                        let buffer = buffer_task.await?;
                        let multi = cx.new(|cx| {
                            multi_buffer::MultiBuffer::singleton(buffer, cx)
                                .with_title("query.sql".into())
                        });
                        workspace.update_in(cx, |workspace, window, cx| {
                            let editor = cx.new(|cx| {
                                let mut ed = Editor::for_multibuffer(multi, None, window, cx);
                                ed.set_text(text.clone(), window, cx);
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

    #[allow(dead_code)]
    fn delete_row(
        &mut self,
        row: Vec<Option<String>>,
        columns: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (store, conn_id, db, table) = match (
            self.store.clone(),
            self.connection_id,
            self.database.clone(),
            self.table_name.clone(),
        ) {
            (Some(s), Some(id), Some(db), Some(tbl)) => (s, id, db, tbl),
            _ => return,
        };
        let sql = Self::generate_delete_sql(&table, &columns, &row);
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Delete this row from '{}'?", table),
            Some(&sql),
            &["Delete", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await.ok() == Some(0) {
                let task =
                    store.update(cx, |store, cx| store.execute_query(conn_id, db, sql, cx))?;
                task.await.log_err();
                this.update_in(cx, |this, window, cx| {
                    this.refresh_table_data(window, cx);
                })
                .log_err();
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn render_filter_bar(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let editor = self.filter_editor.clone()?;
        let is_loading = self.is_loading;
        Some(
            div()
                .flex()
                .flex_row()
                .items_center()
                .px_2()
                .py_1()
                .gap_2()
                .border_b_1()
                .child(
                    Label::new("WHERE")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .child(div().flex_1().border_1().rounded_md().px_1().child(editor))
                .child(
                    IconButton::new("refresh-data", IconName::RefreshTitle)
                        .icon_size(IconSize::Small)
                        .disabled(is_loading)
                        .tooltip(Tooltip::text("Refresh (apply filter)"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.refresh_table_data(window, cx);
                        })),
                ),
        )
    }

    fn render_empty_state(&self) -> impl IntoElement {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_1()
            .child(
                Icon::new(IconName::DatabaseZap)
                    .size(IconSize::Medium)
                    .color(Color::Muted),
            )
            .child(
                Label::new("Run a query to see rows here")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
    }

    fn render_error(&self, error: &str) -> impl IntoElement {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_2()
            .p_4()
            .child(Icon::new(IconName::Warning).color(Color::Error))
            .child(
                div()
                    .id("query-error")
                    .max_w(px(560.))
                    .max_h(px(240.))
                    .overflow_y_scroll()
                    .child(
                        Label::new(error.to_string())
                            .size(LabelSize::Small)
                            .color(Color::Error),
                    ),
            )
    }

    // Builds ONE grid row (by absolute index), horizontally virtualized: only the
    // columns intersecting the current horizontal viewport are built; off-screen
    // columns collapse into left/right spacer divs so total width and alignment
    // with the header are preserved. Reads the row from `self.result` so nothing
    // is cloned wholesale during scroll.
    fn render_grid_row(
        &self,
        abs_idx: usize,
        display_idx: usize,
        grid_border: gpui::Hsla,
        zebra_bg: gpui::Hsla,
        modified_bg: gpui::Hsla,
        deleted_bg: gpui::Hsla,
        cell_hover_bg: gpui::Hsla,
        has_table_context: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let modified_border = cx.theme().status().modified;
        // Bounds-check only; actual cell access goes through self.result inside closures.
        let Some(_row) = self
            .result
            .as_ref()
            .and_then(|result| result.rows.get(abs_idx))
        else {
            return div().into_any_element();
        };
        let selection_bg = cx.theme().colors().element_selected;
        let search_match_bg = cx.theme().colors().search_match_background;
        let active_line_bg = cx.theme().colors().editor_active_line_background;
        let row_selected_bg = cx.theme().colors().editor_highlighted_line_background;
        let is_deleted = self.deleted_rows.contains(&abs_idx);
        let is_row_selected = self.selected_rows.contains(&abs_idx);
        let weak_this = cx.weak_entity();

        let view_left = -f32::from(self.h_scroll.offset().x);
        let view_width = f32::from(self.h_scroll.bounds().size.width);
        let (visible_lo, visible_hi) = if view_width <= 1.0 {
            (f32::MIN, f32::MAX)
        } else {
            (view_left - 400.0, view_left + view_width + 400.0)
        };

        let mut x = 0.0f32;
        let mut left_spacer = 0.0f32;
        let mut last_visible_end = 0.0f32;
        let mut cells: Vec<AnyElement> = Vec::new();
        // Iterate display positions; visible_columns[display_pos] is the data column.
        for (display_pos, &cell_idx) in self.visible_columns.iter().enumerate() {
            let width = self
                .col_widths
                .get(display_pos)
                .copied()
                .unwrap_or(px(120.));
            let start = x;
            x += f32::from(width);
            let end = x;
            if end < visible_lo || start > visible_hi {
                if cells.is_empty() {
                    left_spacer = end;
                }
                continue;
            }
            let is_selected = self.selected_cell == Some((abs_idx, cell_idx))
                || self.selected_cell_range_contains(abs_idx, display_idx, cell_idx);
            let is_modified = self.pending_edits.contains_key(&(abs_idx, cell_idx));
            let editing = self
                .cell_edit
                .as_ref()
                .filter(|edit| edit.abs_idx == abs_idx && edit.col_idx == cell_idx);
            let cell_body: AnyElement = if let Some(edit) = editing {
                let editor = edit.editor.clone();
                let editor_kind = self.column_kind_at(cell_idx);
                div()
                    // Capture phase: intercept before the editor processes them.
                    // Enter → commit + next row; Tab/Shift-Tab → commit + next/prev column.
                    .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                        match event.keystroke.key.as_str() {
                            "enter" if event.keystroke.modifiers.shift => {
                                this.commit_and_move(0, -1, window, cx);
                            }
                            "enter" if !event.keystroke.modifiers.modified() => {
                                this.commit_and_move(0, 1, window, cx);
                            }
                            "tab" if !event.keystroke.modifiers.shift => {
                                this.commit_and_move(1, 0, window, cx);
                            }
                            "tab" if event.keystroke.modifiers.shift => {
                                this.commit_and_move(-1, 0, window, cx);
                            }
                            "escape" => this.cancel_cell_edit(window, cx),
                            _ => {}
                        }
                    }))
                    .child(Self::render_cell_editor(editor, editor_kind))
                    .into_any_element()
            } else if matches!(self.column_kind_at(cell_idx), CellEditorKind::Boolean) {
                let cell_val = match self.pending_cell_value(abs_idx, cell_idx) {
                    Some(cv) => cv.clone(),
                    None => CellValue::from_loaded(&self.loaded_cell_value(abs_idx, cell_idx)),
                };
                let (icon_name, color) = bool_cell_display(&cell_val);
                Icon::new(icon_name)
                    .size(IconSize::Small)
                    .color(if is_deleted { Color::Muted } else { color })
                    .into_any_element()
            } else {
                // Show the pending value when one exists, else the loaded cell.
                let (display, color) = match self.pending_cell_value(abs_idx, cell_idx) {
                    Some(value) => render_cell_value(value),
                    None => render_loaded_value(
                        self.result
                            .as_ref()
                            .and_then(|result| result.rows.get(abs_idx))
                            .and_then(|row| row.get(cell_idx))
                            .and_then(|cell| cell.as_deref()),
                    ),
                };
                let label = Label::new(display.clone())
                    .size(LabelSize::Small)
                    .color(color)
                    .when(
                        display == NULL_MARKER || display == DEFAULT_MARKER,
                        |label| label.italic(),
                    )
                    .when(is_deleted, |label| label.strikethrough())
                    .into_any_element();
                Self::render_typed_cell_body(label, self.column_kind_at(cell_idx))
            };
            let is_find_match = self.find_query.as_ref().is_some_and(|q| !q.is_empty())
                && self.find_matches.contains(&(abs_idx, cell_idx));
            let is_current_find = is_find_match
                && self.find_matches.get(self.find_current) == Some(&(abs_idx, cell_idx));
            let cell_display_idx = display_idx;

            let cell_div = div()
                .id(ElementId::from(SharedString::from(format!(
                    "cell-{abs_idx}-{cell_idx}"
                ))))
                .debug_selector(move || format!("CELL-{abs_idx}-{cell_idx}"))
                .px_2()
                .h(px(Self::GRID_ROW_H))
                .w(width)
                .flex_none()
                .flex()
                .items_center()
                .border_r_1()
                .border_color(grid_border)
                .overflow_hidden()
                // Priority (high→low): selection, find, row-selected, modified.
                .when(
                    is_row_selected && !is_selected && !is_find_match,
                    move |this| this.bg(row_selected_bg),
                )
                .when(
                    is_find_match && !is_selected && !is_current_find,
                    move |this| this.bg(search_match_bg),
                )
                .when(is_current_find && !is_selected, move |this| {
                    this.bg(active_line_bg)
                })
                .when(is_modified && !is_selected && !is_find_match, |this| {
                    this.bg(modified_bg)
                        .border_l(px(2.))
                        .border_color(modified_border)
                })
                .when(is_selected, |this| this.bg(selection_bg))
                .when(is_deleted, |this| this.bg(deleted_bg))
                .when(
                    !is_selected && !is_find_match && !is_modified && !is_deleted,
                    |this| this.hover(move |style| style.bg(cell_hover_bg)),
                )
                .child(cell_body)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        this.begin_cell_drag(abs_idx, cell_idx);
                    }),
                )
                .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
                    if event.pressed_button == Some(MouseButton::Left) {
                        cx.stop_propagation();
                        this.update_cell_drag(abs_idx, cell_display_idx, cell_idx, cx);
                    }
                }))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseUpEvent, _, cx| {
                        cx.stop_propagation();
                        this.end_cell_drag();
                    }),
                )
                .on_click(
                    cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                        let gpui::ClickEvent::Mouse(mouse) = event else {
                            return;
                        };
                        cx.stop_propagation();
                        this.click_loaded_cell(
                            abs_idx,
                            cell_display_idx,
                            cell_idx,
                            event.click_count(),
                            mouse.down.modifiers.shift,
                            mouse.down.modifiers.control,
                            window,
                            cx,
                        );
                    }),
                );

            let cell_value_copy = self
                .pending_cell_value(abs_idx, cell_idx)
                .map(|cv| match cv {
                    CellValue::Text(s) => s.clone(),
                    CellValue::Null | CellValue::Default => String::new(),
                })
                .or_else(|| self.loaded_cell_value(abs_idx, cell_idx))
                .unwrap_or_default();

            let has_tc = has_table_context;
            let is_mod = is_modified;
            let wt_for_cell = weak_this.clone();

            cells.push(
                right_click_menu(ElementId::from(SharedString::from(format!(
                    "cell-ctx-{abs_idx}-{cell_idx}"
                ))))
                .trigger(move |_, _, _| cell_div)
                .menu(move |window, cx| {
                    let wt = wt_for_cell.clone();
                    let cv = cell_value_copy.clone();
                    ContextMenu::build(window, cx, move |menu, _, _| {
                        let wt_edit = wt.clone();
                        let wt_record = wt.clone();
                        let wt_val = wt.clone();
                        let wt_null = wt.clone();
                        let wt_default = wt.clone();
                        let wt_revert = wt.clone();
                        let wt_add = wt.clone();
                        let wt_del = wt.clone();
                        let wt_clone = wt.clone();
                        let wt_qdoc = wt.clone();
                        let wt_filter = wt.clone();
                        let wt_exclude = wt.clone();
                        let filter_value = cv.clone();
                        let exclude_value = cv.clone();
                        let menu = menu
                            .header("View")
                            .entry("Edit", None, move |window, cx| {
                                wt_edit
                                    .update(cx, |this, cx| {
                                        this.selected_cell = Some((abs_idx, cell_idx));
                                        this.begin_cell_edit(
                                            abs_idx,
                                            cell_idx,
                                            CellEditEntry::CursorEnd,
                                            window,
                                            cx,
                                        );
                                    })
                                    .ok();
                            })
                            .entry("Record View", None, move |_, cx| {
                                wt_record
                                    .update(cx, |this, cx| {
                                        this.record_view_open = true;
                                        cx.notify();
                                    })
                                    .ok();
                            })
                            .entry("Value Editor", None, move |_, cx| {
                                wt_val
                                    .update(cx, |this, cx| {
                                        this.value_editor_open = true;
                                        cx.notify();
                                    })
                                    .ok();
                            })
                            .separator()
                            .header("Clipboard")
                            .entry("Copy", None, move |_, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(cv.clone()));
                            })
                            .separator()
                            .header("Filter")
                            .entry(
                                format!("Filter by \"{}\"", display_cell(&filter_value)),
                                None,
                                move |window, cx| {
                                    let value = filter_value.clone();
                                    wt_filter
                                        .update(cx, |this, cx| {
                                            this.apply_quick_filter(
                                                cell_idx, value, false, window, cx,
                                            );
                                        })
                                        .ok();
                                },
                            )
                            .entry(
                                format!("Exclude \"{}\"", display_cell(&exclude_value)),
                                None,
                                move |window, cx| {
                                    let value = exclude_value.clone();
                                    wt_exclude
                                        .update(cx, |this, cx| {
                                            this.apply_quick_filter(
                                                cell_idx, value, true, window, cx,
                                            );
                                        })
                                        .ok();
                                },
                            )
                            .separator()
                            .header("Quick Help")
                            .entry("Quick Documentation", None, move |_, cx| {
                                wt_qdoc
                                    .update(cx, |this, cx| {
                                        this.quick_doc_open = !this.quick_doc_open;
                                        cx.notify();
                                    })
                                    .ok();
                            });

                        if has_tc {
                            let menu = menu.separator().header("Edit");
                            let menu = if is_mod {
                                menu.entry("Revert Cell", None, move |_, cx| {
                                    wt_revert
                                        .update(cx, |this, cx| {
                                            this.pending_edits.remove(&(abs_idx, cell_idx));
                                            cx.notify();
                                        })
                                        .ok();
                                })
                            } else {
                                menu
                            };
                            menu.entry("Set NULL", None, move |_, cx| {
                                wt_null
                                    .update(cx, |this, cx| {
                                        this.selected_cell = Some((abs_idx, cell_idx));
                                        this.set_selected_cell_value(CellValue::Null, cx);
                                    })
                                    .ok();
                            })
                            .entry("Set DEFAULT", None, move |_, cx| {
                                wt_default
                                    .update(cx, |this, cx| {
                                        this.selected_cell = Some((abs_idx, cell_idx));
                                        this.set_selected_cell_value(CellValue::Default, cx);
                                    })
                                    .ok();
                            })
                            .separator()
                            .header("Row")
                            .entry("Add Row", None, move |window, cx| {
                                wt_add
                                    .update(cx, |this, cx| {
                                        this.select_row_for_context_action(abs_idx, cell_idx);
                                        if let Some(added_idx) =
                                            this.add_blank_row_after(Some(abs_idx), cx)
                                        {
                                            this.begin_added_row_edit(added_idx, window, cx);
                                        }
                                    })
                                    .ok();
                            })
                            .entry("Delete Row", None, move |_, cx| {
                                wt_del
                                    .update(cx, |this, cx| {
                                        this.select_row_for_context_action(abs_idx, cell_idx);
                                        this.toggle_delete_selected_row(cx);
                                    })
                                    .ok();
                            })
                            .entry(
                                "Clone Row",
                                None,
                                move |window, cx| {
                                    wt_clone
                                        .update(cx, |this, cx| {
                                            this.select_row_for_context_action(abs_idx, cell_idx);
                                            if let Some(added_idx) =
                                                this.clone_row_after(abs_idx, abs_idx, cx)
                                            {
                                                this.begin_added_row_edit(added_idx, window, cx);
                                            }
                                        })
                                        .ok();
                                },
                            )
                        } else {
                            menu
                        }
                    })
                })
                .into_any_element(),
            );
            last_visible_end = end;
        }
        // Everything past the last visible cell collapses into the right spacer so
        // the row keeps its full content width and lines up with the header.
        let right_spacer = (x - last_visible_end).max(0.0);

        let row_el = div()
            .id(ElementId::from(SharedString::from(format!(
                "row-{abs_idx}"
            ))))
            .flex()
            .flex_row()
            .h(px(Self::GRID_ROW_H))
            .border_b_1()
            .border_color(grid_border)
            .when(display_idx % 2 == 1, |this| this.bg(zebra_bg))
            // The deleted fill spans the whole row so a marked row reads as a
            // pending deletion at a glance.
            .when(is_deleted, |this| this.bg(deleted_bg))
            .child(div().w(px(left_spacer)).flex_none())
            .children(cells)
            .child(div().w(px(right_spacer)).flex_none())
            // Fallback for clicks that land on row whitespace rather than a cell.
            .on_click(
                cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                    let gpui::ClickEvent::Mouse(mouse) = event else {
                        return;
                    };
                    let Some(cell_idx) = this.column_at_x(f32::from(mouse.up.position.x)) else {
                        return;
                    };
                    this.click_loaded_cell(
                        abs_idx,
                        display_idx,
                        cell_idx,
                        event.click_count(),
                        mouse.down.modifiers.shift,
                        mouse.down.modifiers.control,
                        window,
                        cx,
                    );
                }),
            );

        row_el.into_any_element()
    }

    // Builds ONE added row (a pending INSERT). Same horizontal virtualization as
    // a loaded row, but cells read from `added_rows` and edits write there. The
    // selection/click math uses an absolute index past the loaded rows so the
    // shared column hit-testing keeps working.
    fn render_added_row(
        &self,
        added_idx: usize,
        display_idx: usize,
        grid_border: gpui::Hsla,
        zebra_bg: gpui::Hsla,
        added_bg: gpui::Hsla,
        cell_hover_bg: gpui::Hsla,
        _has_table_context: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(row) = self.added_rows.get(added_idx) else {
            return div().into_any_element();
        };
        let abs_idx = self.loaded_row_count() + added_idx;
        let selection_bg = cx.theme().colors().element_selected;
        let column_count = self
            .result
            .as_ref()
            .map_or(row.len(), |result| result.columns.len());

        let view_left = -f32::from(self.h_scroll.offset().x);
        let view_width = f32::from(self.h_scroll.bounds().size.width);
        let (visible_lo, visible_hi) = if view_width <= 1.0 {
            (f32::MIN, f32::MAX)
        } else {
            (view_left - 400.0, view_left + view_width + 400.0)
        };

        let mut x = 0.0f32;
        let mut left_spacer = 0.0f32;
        let mut last_visible_end = 0.0f32;
        let mut cells: Vec<AnyElement> = Vec::new();
        for (display_pos, &cell_idx) in self.visible_columns.iter().enumerate() {
            if cell_idx >= column_count {
                continue;
            }
            let width = self
                .col_widths
                .get(display_pos)
                .copied()
                .unwrap_or(px(120.));
            let start = x;
            x += f32::from(width);
            let end = x;
            if end < visible_lo || start > visible_hi {
                if cells.is_empty() {
                    left_spacer = end;
                }
                continue;
            }
            let is_selected = self.selected_cell == Some((abs_idx, cell_idx))
                || self.selected_cell_range_contains(abs_idx, display_idx, cell_idx);
            let editing = self
                .cell_edit
                .as_ref()
                .filter(|edit| edit.abs_idx == abs_idx && edit.col_idx == cell_idx);
            let cell_body: AnyElement = if let Some(edit) = editing {
                let editor = edit.editor.clone();
                let editor_kind = self.column_kind_at(cell_idx);
                div()
                    .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                        match event.keystroke.key.as_str() {
                            "enter" if event.keystroke.modifiers.shift => {
                                this.commit_and_move(0, -1, window, cx);
                            }
                            "enter" if !event.keystroke.modifiers.modified() => {
                                this.commit_and_move(0, 1, window, cx);
                            }
                            "tab" if !event.keystroke.modifiers.shift => {
                                this.commit_and_move(1, 0, window, cx);
                            }
                            "tab" if event.keystroke.modifiers.shift => {
                                this.commit_and_move(-1, 0, window, cx);
                            }
                            "escape" => this.cancel_cell_edit(window, cx),
                            _ => {}
                        }
                    }))
                    .child(Self::render_cell_editor(editor, editor_kind))
                    .into_any_element()
            } else if matches!(self.column_kind_at(cell_idx), CellEditorKind::Boolean) {
                let cell_val = row.get(cell_idx).cloned().unwrap_or(CellValue::Null);
                let (icon_name, color) = bool_cell_display(&cell_val);
                Icon::new(icon_name)
                    .size(IconSize::Small)
                    .color(color)
                    .into_any_element()
            } else {
                let (display, color) = match row.get(cell_idx) {
                    Some(value) => render_cell_value(value),
                    None => (NULL_MARKER.to_string(), Color::Muted),
                };
                let label = Label::new(display)
                    .size(LabelSize::Small)
                    .color(color)
                    .into_any_element();
                Self::render_typed_cell_body(label, self.column_kind_at(cell_idx))
            };
            let cell_display_idx = display_idx;
            cells.push(
                div()
                    .id(ElementId::from(SharedString::from(format!(
                        "added-cell-{added_idx}-{cell_idx}"
                    ))))
                    .debug_selector(move || format!("ADDED_CELL-{added_idx}-{cell_idx}"))
                    .px_2()
                    .h(px(Self::GRID_ROW_H))
                    .w(width)
                    .flex_none()
                    .flex()
                    .items_center()
                    .border_r_1()
                    .border_color(grid_border)
                    .overflow_hidden()
                    .when(is_selected, |this| this.bg(selection_bg))
                    .when(!is_selected, |this| {
                        this.hover(move |style| style.bg(cell_hover_bg))
                    })
                    .child(cell_body)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            this.begin_cell_drag(abs_idx, cell_idx);
                        }),
                    )
                    .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
                        if event.pressed_button == Some(MouseButton::Left) {
                            cx.stop_propagation();
                            this.update_cell_drag(abs_idx, cell_display_idx, cell_idx, cx);
                        }
                    }))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseUpEvent, _, cx| {
                            cx.stop_propagation();
                            this.end_cell_drag();
                        }),
                    )
                    .on_click(
                        cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                            let gpui::ClickEvent::Mouse(mouse) = event else {
                                return;
                            };
                            cx.stop_propagation();
                            this.click_added_cell(
                                abs_idx,
                                cell_display_idx,
                                cell_idx,
                                added_idx,
                                event.click_count(),
                                mouse.down.modifiers.shift,
                                mouse.down.modifiers.control,
                                window,
                                cx,
                            );
                        }),
                    )
                    .into_any_element(),
            );
            last_visible_end = end;
        }
        let right_spacer = (x - last_visible_end).max(0.0);

        let row_el = div()
            .id(ElementId::from(SharedString::from(format!(
                "added-row-{added_idx}"
            ))))
            .flex()
            .flex_row()
            .h(px(Self::GRID_ROW_H))
            .border_b_1()
            .border_color(grid_border)
            .when(display_idx % 2 == 1, |this| this.bg(zebra_bg))
            .bg(added_bg)
            .child(div().w(px(left_spacer)).flex_none())
            .children(cells)
            .child(div().w(px(right_spacer)).flex_none())
            .on_click(
                cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                    let gpui::ClickEvent::Mouse(mouse) = event else {
                        return;
                    };
                    let Some(cell_idx) = this.column_at_x(f32::from(mouse.up.position.x)) else {
                        return;
                    };
                    this.click_added_cell(
                        abs_idx,
                        display_idx,
                        cell_idx,
                        added_idx,
                        event.click_count(),
                        mouse.down.modifiers.shift,
                        mouse.down.modifiers.control,
                        window,
                        cx,
                    );
                }),
            );

        row_el.into_any_element()
    }

    fn render_result(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(result) = self.result.as_ref() else {
            return div().into_any_element();
        };
        let sort_columns = self.sort_columns.clone();
        let has_table_context = self.row_ops_enabled();
        let total_rows = result.rows.len();
        // Column names for the header (small; the Table sizes/positions columns).
        let columns = result.columns.clone();

        let status = if self.is_loading {
            format!("Loading… {} rows", self.loaded_rows)
        } else {
            format!(
                "{} row{} ({} ms)",
                total_rows,
                if total_rows == 1 { "" } else { "s" },
                result.execution_time_ms,
            )
        };
        let is_loading = self.is_loading;
        let fetch_target = self.fetch_target;
        let pending_count = self.pending_change_count();
        let transaction_mode = self.transaction_mode;
        let staged_count = self.staged_statements.len();
        let row_ops_enabled = self.row_ops_enabled();
        let has_selected_cell = self.selected_cell.is_some();
        let selected_col_nullable = self
            .selected_cell
            .and_then(|(_, col_idx)| {
                self.column_infos
                    .as_deref()?
                    .get(col_idx)
                    .map(|c| c.is_nullable)
            })
            .unwrap_or(has_selected_cell);
        let selected_col_has_default = self
            .selected_cell
            .and_then(|(_, col_idx)| {
                self.column_infos
                    .as_deref()?
                    .get(col_idx)
                    .map(|c| c.default_value.is_some())
            })
            .unwrap_or(has_selected_cell);
        let result_for_export = self.result.clone();
        let table_for_export = self.table_name.clone();
        let value_editor_open = self.value_editor_open;
        let record_view_open = self.record_view_open;
        let quick_doc_open = self.quick_doc_open;
        let history_open = self.history_open;
        let transposed = self.transposed;
        let chart_open = self.chart_open;
        let pinned = self.pinned;
        let limit_editor = self.limit_editor.clone();
        let copy_format = self.copy_format;
        let weak_this = cx.weak_entity();
        let weak_for_gutter = weak_this.clone();
        let weak_for_header = weak_this.clone();
        let weak_for_copy_menu = weak_this.clone();

        div()
            .flex()
            .flex_col()
            .size_full()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(Label::new(status).size(LabelSize::Small).color(Color::Muted))
                            .when(is_loading, |el| {
                                el.child(loading_spinner("fill-spinner", IconSize::XSmall))
                                .child(
                                    Button::new("stop-fill", "Stop")
                                        .style(ButtonStyle::Subtle)
                                        .tooltip(Tooltip::text("Stop loading more rows"))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.stop_fill(cx);
                                        })),
                                )
                            })
                            .when_some(self.status_message.clone(), |el, message| {
                                el.child(
                                    Label::new(message)
                                        .size(LabelSize::Small)
                                        .color(Color::Warning),
                                )
                            })
                            .when(pending_count > 0, |el| {
                                el.child(
                                    Label::new(format!(
                                        "{pending_count} change{}",
                                        if pending_count == 1 { "" } else { "s" }
                                    ))
                                    .size(LabelSize::Small)
                                    .color(Color::Modified),
                                )
                            }),
                    )
                    .child(
                        // Live FPS watermark — updates each render, so it reflects
                        // the real frame rate while scrolling the grid.
                        Label::new(format!("{} FPS", self.fps))
                            .size(LabelSize::Small)
                            .color(if self.fps >= 50 {
                                Color::Success
                            } else if self.fps >= 25 {
                                Color::Warning
                            } else {
                                Color::Error
                            }),
                    )
                    // Row-limit control: text input + presets dropdown.
                    .when_some(limit_editor, |el, editor| {
                        let weak_for_limit = weak_this.clone();
                        let limit_text_len = Self::limit_display_text(self.fetch_target).len();
                        let limit_input_w = px(8.0 * limit_text_len as f32 + 18.0).max(px(34.));
                        el.child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .rounded_md()
                                .border_1()
                                .border_color(cx.theme().colors().border)
                                .overflow_hidden()
                                .child(
                                    div()
                                        .w(limit_input_w)
                                        .h(px(22.))
                                        .px_1()
                                        .capture_key_down(cx.listener(
                                            |this, event: &KeyDownEvent, window, cx| {
                                                match event.keystroke.key.as_str() {
                                                    "up" if !event.keystroke.modifiers.modified() => {
                                                        this.fetch_target = match this.fetch_target {
                                                            usize::MAX => 1,
                                                            n => n.saturating_add(1),
                                                        };
                                                        this.sync_limit_editor_text(window, cx);
                                                    }
                                                    "down" if !event.keystroke.modifiers.modified() => {
                                                        if this.fetch_target != usize::MAX {
                                                            this.fetch_target = this.fetch_target.saturating_sub(1).max(1);
                                                            this.sync_limit_editor_text(window, cx);
                                                        }
                                                    }
                                                    "enter" if !event.keystroke.modifiers.modified() => {
                                                        this.apply_custom_limit(true, cx);
                                                        this.sync_limit_editor_text(window, cx);
                                                    }
                                                    _ => {}
                                                }
                                            },
                                        ))
                                        .child(editor),
                                )
                                .child(
                                    div()
                                        .w(px(1.))
                                        .h(px(22.))
                                        .bg(cx.theme().colors().border),
                                )
                                .child(
                                    PopoverMenu::new("limit-dropdown")
                                        .menu(move |window, cx| {
                                            let wt = weak_for_limit.clone();
                                            Some(ContextMenu::build(window, cx, move |menu, _, _cx| {
                                                FETCH_TARGET_CHOICES.iter().fold(
                                                    menu,
                                                    |menu, (value, label)| {
                                                        let wt2 = wt.clone();
                                                        let v = *value;
                                                        let is_active = fetch_target == v;
                                                        menu.entry(
                                                            SharedString::from(*label),
                                                            None,
                                                            move |_, cx| {
                                                                wt2.update(cx, |this, cx| {
                                                                    this.set_fetch_target(v, cx);
                                                                })
                                                                .ok();
                                                            },
                                                        )
                                                        .when(is_active, |m| m)
                                                    },
                                                )
                                            }))
                                        })
                                        .trigger(
                                            IconButton::new(
                                                "limit-menu-chevron",
                                                IconName::ChevronDown,
                                            )
                                            .shape(ui::IconButtonShape::Square)
                                            .icon_size(IconSize::XSmall)
                                            .tooltip(Tooltip::text("Row limit presets")),
                                        ),
                                ),
                        )
                    })
                    .child(
                        PopoverMenu::new("copy-format-dropdown")
                            .menu(move |window, cx| {
                                let weak_for_copy = weak_for_copy_menu.clone();
                                Some(ContextMenu::build(window, cx, move |menu, _, _cx| {
                                    COPY_FORMAT_CHOICES.iter().fold(
                                        menu.header("Ctrl+C format"),
                                        |menu, (format, label)| {
                                            let weak = weak_for_copy.clone();
                                            let format = *format;
                                            let is_active = copy_format == format;
                                            menu.entry(SharedString::from(*label), None, move |_, cx| {
                                                weak.update(cx, |this, cx| {
                                                    this.copy_format = format;
                                                    cx.notify();
                                                })
                                                .ok();
                                            })
                                            .when(is_active, |menu| menu)
                                        },
                                    )
                                }))
                            })
                            .anchor(Anchor::TopRight)
                            .attach(Anchor::BottomRight)
                            .trigger_with_tooltip(
                                Button::new(
                                    "copy-format-menu-btn",
                                    format!("Copy: {}", copy_format.label()),
                                )
                                .width(px(148.0))
                                .style(ButtonStyle::Subtle)
                                .label_size(LabelSize::Small),
                                Tooltip::text("Format used by Ctrl+C"),
                            ),
                    )
                    .when(has_selected_cell, |el| {
                        el.child(Divider::vertical())
                        .when(selected_col_nullable, |el| {
                            el.child(
                                Button::new("set-null", "Set NULL")
                                    .style(ButtonStyle::Subtle)
                                    .tooltip(Tooltip::text("Set the selected cell to NULL (Ctrl+Alt+N / Cmd+Alt+N)"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.set_selected_cell_value(CellValue::Null, cx);
                                    })),
                            )
                        })
                        .when(selected_col_has_default, |el| {
                            el.child(
                                Button::new("set-default", "Set DEFAULT")
                                    .style(ButtonStyle::Subtle)
                                    .tooltip(Tooltip::text("Set the selected cell to the column DEFAULT (Ctrl+Alt+D / Cmd+Alt+D)"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.set_selected_cell_value(CellValue::Default, cx);
                                    })),
                            )
                        })
                    })
                    .when(pending_count > 0, |el| {
                        el.child(Divider::vertical())
                        .child(
                            Button::new("submit-edits", "Submit")
                                .style(ButtonStyle::Filled)
                                .tooltip(Tooltip::text("Write pending changes to the database"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.submit_pending_edits(window, cx);
                                })),
                        )
                        .child(
                            Button::new("preview-pending", "Preview")
                                .style(ButtonStyle::Subtle)
                                .tooltip(Tooltip::text("Preview the SQL these changes will run"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.preview_open = !this.preview_open;
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new("revert-edits", "Revert")
                                .style(ButtonStyle::Subtle)
                                .tooltip(Tooltip::text("Discard pending changes"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.revert_pending_edits(cx);
                                })),
                        )
                    })
                    .child(Divider::vertical())
                    .child(
                        Button::new("transaction-mode", transaction_mode.label())
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::Small)
                            .toggle_state(transaction_mode == TransactionMode::Manual)
                            .tooltip(Tooltip::text(
                                "Transaction mode: Auto commits on Submit; Manual stages until Commit",
                            ))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.toggle_transaction_mode(cx);
                            })),
                    )
                    .when(staged_count > 0, |el| {
                        el.child(
                            Button::new("commit-transaction", "Commit")
                                .style(ButtonStyle::Filled)
                                .label_size(LabelSize::Small)
                                .tooltip(Tooltip::text("Run the staged statements as one transaction"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.commit_transaction(window, cx);
                                })),
                        )
                        .child(
                            Button::new("rollback-transaction", "Roll Back")
                                .style(ButtonStyle::Subtle)
                                .label_size(LabelSize::Small)
                                .tooltip(Tooltip::text("Discard the staged statements"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.rollback_transaction(cx);
                                })),
                        )
                    })
                    .child(Divider::vertical())
                    .child(
                        IconButton::new("toggle-local-filters", IconName::Filter)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Toggle column filters (Ctrl+F5)"))
                            .toggle_state(self.local_filter_visible)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.toggle_local_filter_row(window, cx);
                            })),
                    )
                    .child(
                        IconButton::new("toggle-column-list", IconName::ListTree)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Show/hide columns (Ctrl+F12)"))
                            .toggle_state(self.column_list_visible)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.toggle_column_list(cx);
                            })),
                    )
                    .child(Divider::vertical())
                    .child(
                        PopoverMenu::new("view-dropdown")
                            .menu(move |window, cx| {
                                Some(ContextMenu::build(window, cx, move |menu, _, _cx| {
                                    menu
                                        .action_checked("Value Editor", Box::new(ToggleValueEditor), value_editor_open)
                                        .action_checked("Record View", Box::new(ToggleRecordView), record_view_open)
                                        .action_checked("Transpose", Box::new(ToggleTranspose), transposed)
                                        .action_checked("Show Chart", Box::new(ToggleChart), chart_open)
                                        .action_checked("Column Info", Box::new(QuickDoc), quick_doc_open)
                                        .separator()
                                        .action_checked("Query History", Box::new(OpenQueryHistory), history_open)
                                        .separator()
                                        .action("Go to Row", Box::new(GoToRow))
                                        .action("Copy Aggregation", Box::new(CopyAggregation))
                                        .action("Preview Pending Changes", Box::new(PreviewPendingChanges))
                                        .separator()
                                        .action_checked("Pin Tab", Box::new(TogglePinResult), pinned)
                                        .action("Reset View", Box::new(ResetView))
                                }))
                            })
                            .anchor(Anchor::TopRight)
                            .attach(Anchor::BottomRight)
                            .trigger_with_tooltip(
                                Button::new("view-menu-btn", "View")
                                    .style(ButtonStyle::Subtle)
                                    .label_size(LabelSize::Small),
                                Tooltip::text("Panels and inspectors"),
                            ),
                    )
                    .child(Divider::vertical())
                    .child({
                        PopoverMenu::new("export-dropdown")
                            .menu(move |window, cx| {
                                let r = result_for_export.clone();
                                let tbl = table_for_export.clone();
                                let weak_for_paste = weak_this.clone();
                                let weak_for_dialog = weak_this.clone();
                                Some(ContextMenu::build(window, cx, move |menu, _, _cx| {
                                    let r = r.clone();
                                    let tbl = tbl.clone();
                                    let weak_for_dialog = weak_for_dialog.clone();
                                    menu
                                        .entry("Export Data…", None, move |_, cx| {
                                            weak_for_dialog
                                                .update(cx, |this, cx| this.open_export_dialog(cx))
                                                .ok();
                                        })
                                        .separator()
                                        .entry("Copy as CSV", None, {
                                            let r = r.clone();
                                            move |_, cx| {
                                                if let Some(result) = r.as_ref() {
                                                    cx.write_to_clipboard(ClipboardItem::new_string(ResultView::export_csv(result)));
                                                }
                                            }
                                        })
                                        .entry("Copy as TSV", None, {
                                            let r = r.clone();
                                            move |_, cx| {
                                                if let Some(result) = r.as_ref() {
                                                    cx.write_to_clipboard(ClipboardItem::new_string(ResultView::export_tsv(result)));
                                                }
                                            }
                                        })
                                        .entry("Copy as JSON", None, {
                                            let r = r.clone();
                                            move |_, cx| {
                                                if let Some(result) = r.as_ref() {
                                                    cx.write_to_clipboard(ClipboardItem::new_string(ResultView::export_json(result)));
                                                }
                                            }
                                        })
                                        .entry("Copy as Markdown", None, {
                                            let r = r.clone();
                                            move |_, cx| {
                                                if let Some(result) = r.as_ref() {
                                                    cx.write_to_clipboard(ClipboardItem::new_string(ResultView::export_markdown(result)));
                                                }
                                            }
                                        })
                                        .entry("Copy as HTML", None, {
                                            let r = r.clone();
                                            move |_, cx| {
                                                if let Some(result) = r.as_ref() {
                                                    cx.write_to_clipboard(ClipboardItem::new_string(ResultView::export_html(result)));
                                                }
                                            }
                                        })
                                        .when_some(tbl, |menu, table| {
                                            let r_ins = r.clone();
                                            let r_upd = r.clone();
                                            let tbl_upd = table.clone();
                                            menu
                                                .entry("Copy as SQL INSERT", None, {
                                                    move |_, cx| {
                                                        if let Some(result) = r_ins.as_ref() {
                                                            cx.write_to_clipboard(ClipboardItem::new_string(ResultView::export_sql_insert(result, &table)));
                                                        }
                                                    }
                                                })
                                                .entry("Copy as SQL UPDATE", None, {
                                                    move |_, cx| {
                                                        if let Some(result) = r_upd.as_ref() {
                                                            cx.write_to_clipboard(ClipboardItem::new_string(ResultView::export_sql_update(result, &tbl_upd)));
                                                        }
                                                    }
                                                })
                                        })
                                        .separator()
                                        .entry("Paste rows from Clipboard (CSV/TSV)", None, {
                                            let weak = weak_for_paste;
                                            move |_, cx| {
                                                weak.update(cx, |this, cx| {
                                                    let Some(result) = this.result.as_ref() else { return; };
                                                    let col_count = result.columns.len();
                                                    let column_names = result.columns.clone();
                                                    if col_count == 0 { return; }
                                                    let text = cx.read_from_clipboard()
                                                        .and_then(|c| c.text())
                                                        .unwrap_or_default();
                                                    let rows = Self::parse_clipboard_rows(&text, col_count, &column_names);
                                                    if rows.is_empty() { return; }
                                                    this.added_row_anchors.extend(
                                                        std::iter::repeat_n(AddedRowAnchor::End, rows.len()),
                                                    );
                                                    this.added_rows.extend(rows);
                                                    cx.notify();
                                                }).log_err();
                                            }
                                        })
                                        .separator()
                                        .entry("Save as CSV…", None, {
                                            let r = r.clone();
                                            move |_, cx| {
                                                if let Some(result) = r.clone() {
                                                    let home = paths::home_dir().to_path_buf();
                                                    let path_rx = cx.prompt_for_new_path(&home, Some("result.csv"));
                                                    cx.background_spawn(async move {
                                                        if let Some(path) = path_rx.await.log_err().and_then(|r| r.log_err()).flatten() {
                                                            std::fs::write(path, ResultView::export_csv(&result)).log_err();
                                                        }
                                                    }).detach();
                                                }
                                            }
                                        })
                                        .entry("Save as JSON…", None, {
                                            let r = r.clone();
                                            move |_, cx| {
                                                if let Some(result) = r.clone() {
                                                    let home = paths::home_dir().to_path_buf();
                                                    let path_rx = cx.prompt_for_new_path(&home, Some("result.json"));
                                                    cx.background_spawn(async move {
                                                        if let Some(path) = path_rx.await.log_err().and_then(|r| r.log_err()).flatten() {
                                                            std::fs::write(path, ResultView::export_json(&result)).log_err();
                                                        }
                                                    }).detach();
                                                }
                                            }
                                        })
                                        .entry("Save as Excel (XLSX)…", None, {
                                            move |_, cx| {
                                                if let Some(result) = r.clone() {
                                                    let home = paths::home_dir().to_path_buf();
                                                    let path_rx = cx.prompt_for_new_path(&home, Some("result.xlsx"));
                                                    cx.background_spawn(async move {
                                                        if let Some(path) = path_rx.await.log_err().and_then(|r| r.log_err()).flatten() {
                                                            std::fs::write(path, ResultView::export_xlsx(&result)).log_err();
                                                        }
                                                    }).detach();
                                                }
                                            }
                                        })
                                }))
                            })
                            .anchor(Anchor::TopRight)
                            .attach(Anchor::BottomRight)
                            .trigger_with_tooltip(
                                Button::new("export-menu-btn", "Export")
                                    .style(ButtonStyle::Subtle)
                                    .label_size(LabelSize::Small),
                                Tooltip::text("Export or copy data"),
                            )
                    })
            )
            .child(if self.transposed {
                self.render_transposed(cx)
            } else {
                // Use filtered_display_order so per-column filters narrow the list.
                let display_rows = self.display_row_entries();
                let row_count = display_rows.len();
                let loaded_count = self.loaded_row_count();
                let total_width = self.total_width;
                let grid_border = cx.theme().colors().border.opacity(0.46);
                let strong_grid_border = cx.theme().colors().border.opacity(0.82);
                let header_bg = cx
                    .theme()
                    .colors()
                    .editor_subheader_background
                    .blend(cx.theme().colors().text.opacity(0.035));
                let header_hover_bg = header_bg.blend(cx.theme().colors().element_hover);
                let sorted_header_bg = header_bg.blend(cx.theme().colors().element_selected);
                let gutter_bg = cx
                    .theme()
                    .colors()
                    .editor_gutter_background
                    .blend(cx.theme().colors().text.opacity(0.018));
                let gutter_header_bg = gutter_bg.blend(cx.theme().colors().text.opacity(0.035));
                let gutter_hover_bg = cx.theme().colors().element_hover.opacity(0.78);
                let zebra_bg = cx.theme().colors().text.opacity(0.025);
                let cell_hover_bg = cx.theme().colors().element_hover.opacity(0.62);
                // Theme-aware fills for buffered changes. Read once here so render
                // does not touch the theme per cell or per row.
                let modified_bg = cx.theme().status().modified_background;
                let deleted_bg = cx
                    .theme()
                    .status()
                    .deleted_background
                    .blend(cx.theme().status().deleted.opacity(0.16));
                let added_bg = cx.theme().status().created_background;
                // A clearly visible thumb (hover shade) on a distinct track, so the
                // scrollbars read as solid controls rather than faint overlays.
                let thumb_color = cx.theme().colors().scrollbar_thumb_hover_background;
                let track_color = cx.theme().colors().scrollbar_track_background;
                let track_border = cx.theme().colors().scrollbar_track_border;
                let filter_dot_color = cx.theme().colors().text_accent;
                let local_filters = self.local_filters.clone();
                let display_rows_for_body = display_rows.clone();
                let display_rows_for_gutter = display_rows;

                // Horizontal viewport window, shared by header and rows, so only
                // on-screen columns are built per frame.
                let view_left = -f32::from(self.h_scroll.offset().x);
                let view_width = f32::from(self.h_scroll.bounds().size.width);
                let (visible_lo, visible_hi) = if view_width <= 1.0 {
                    (f32::MIN, f32::MAX)
                } else {
                    (view_left - 400.0, view_left + view_width + 400.0)
                };

                // Build per-column type tooltips from column_infos (if loaded).
                let col_type_tooltips: Vec<Option<SharedString>> = {
                    let infos = self.column_infos.as_deref();
                    (0..columns.len())
                        .map(|idx| {
                            infos.and_then(|infos| infos.get(idx)).map(|info| {
                                let nullable = if info.is_nullable { "yes" } else { "no" };
                                let key = info.column_key.as_deref().unwrap_or("—");
                                let default = info.default_value.as_deref().unwrap_or("—");
                                let extra = if info.extra.is_empty() { "—" } else { &info.extra };
                                format!(
                                    "Type: {}\nNullable: {}\nKey: {}\nDefault: {}\nExtra: {}",
                                    info.data_type, nullable, key, default, extra
                                )
                                .into()
                            })
                        })
                        .collect()
                };

                // Header, horizontally virtualized (left/right spacers for the
                // off-screen columns) so a wide header is not rebuilt in full.
                let visible_columns = self.visible_columns.clone();
                let mut hx = 0.0f32;
                let mut header_left = 0.0f32;
                let mut header_last_end = 0.0f32;
                let mut header_cells: Vec<AnyElement> = Vec::new();
                for (display_pos, &col_idx) in visible_columns.iter().enumerate() {
                    let col = columns.get(col_idx).map(|s| s.as_str()).unwrap_or("");
                    let width = self.col_widths.get(display_pos).copied().unwrap_or(px(120.));
                    let start = hx;
                    hx += f32::from(width);
                    let end = hx;
                    if end < visible_lo || start > visible_hi {
                        if header_cells.is_empty() {
                            header_left = end;
                        }
                        continue;
                    }
                    // Find this column's position in the sort list, if any.
                    let sort_pos = sort_columns.iter().position(|sc| sc.col_idx == col_idx);
                    let sort_label = match sort_pos {
                        Some(pos) => {
                            let arrow = if sort_columns[pos].ascending { "↑" } else { "↓" };
                            if sort_columns.len() > 1 {
                                format!(" {}{}", arrow, pos + 1)
                            } else {
                                format!(" {}", arrow)
                            }
                        }
                        None => String::new(),
                    };
                    let is_sorted = sort_pos.is_some();
                    let header_cell_hover_bg = if is_sorted {
                        sorted_header_bg.blend(header_hover_bg)
                    } else {
                        header_hover_bg
                    };
                    let header_cell = {
                        let wt_header = weak_for_header.clone();
                        let header_inner = div()
                            .id(ElementId::from(SharedString::from(format!("col-header-{col_idx}"))))
                            .debug_selector(move || format!("COL_HEADER-{col_idx}"))
                            .px_2()
                            .h(px(Self::GRID_HEADER_H))
                            .w(width)
                            .flex_none()
                            .flex()
                            .items_center()
                            .border_r_1()
                            .border_color(strong_grid_border)
                            .bg(header_bg)
                            .overflow_hidden()
                            .cursor_pointer()
                            .font_weight(gpui::FontWeight::BOLD)
                            .hover(move |style| style.bg(header_cell_hover_bg))
                            .when(is_sorted, move |this| this.bg(sorted_header_bg))
                            .on_click(cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                                let shift = if let gpui::ClickEvent::Mouse(m) = event {
                                    m.down.modifiers.shift
                                } else {
                                    false
                                };
                                if shift {
                                    // Shift+click: add/toggle/replace in multi-sort list.
                                    if let Some(pos) = this.sort_columns.iter().position(|sc| sc.col_idx == col_idx) {
                                        this.sort_columns[pos].ascending = !this.sort_columns[pos].ascending;
                                    } else if this.sort_columns.len() < 3 {
                                        this.sort_columns.push(SortColumn { col_idx, ascending: true });
                                    } else {
                                        // Replace the last priority when already at 3.
                                        let last = this.sort_columns.len() - 1;
                                        this.sort_columns[last] = SortColumn { col_idx, ascending: true };
                                    }
                                } else {
                                    // Normal click: set as sole sort column.
                                    if this.sort_columns.len() == 1 && this.sort_columns[0].col_idx == col_idx {
                                        this.sort_columns[0].ascending = !this.sort_columns[0].ascending;
                                    } else {
                                        this.sort_columns = vec![SortColumn { col_idx, ascending: true }];
                                    }
                                }
                                if this.table_name.is_some() {
                                    this.refresh_table_data(window, cx);
                                } else {
                                    this.recompute_layout();
                                    cx.notify();
                                }
                            }))
                            .when_some(
                                col_type_tooltips.get(col_idx).and_then(|t| t.clone()),
                                |el, tip| el.tooltip(Tooltip::text(tip)),
                            )
                            .child(
                                h_flex()
                                    .gap_1()
                                    .items_center()
                                    .child(
                                        Label::new(format!("{}{}", col, sort_label))
                                            .size(LabelSize::Small)
                                            .color(if is_sorted {
                                                Color::Accent
                                            } else {
                                                Color::Default
                                            }),
                                    )
                                    .when(
                                        local_filters
                                            .get(col_idx)
                                            .is_some_and(|f| !f.is_empty()),
                                        |el| {
                                            el.child(
                                                div()
                                                    .size(px(5.))
                                                    .rounded_full()
                                                    .bg(filter_dot_color),
                                            )
                                        },
                                    ),
                            );
                        right_click_menu(ElementId::from(SharedString::from(format!(
                            "col-header-ctx-{col_idx}"
                        ))))
                        .trigger(move |_, _, _| header_inner)
                        .menu(move |window, cx| {
                            let wt_left = wt_header.clone();
                            let wt_right = wt_header.clone();
                            let wt_hide = wt_header.clone();
                            ContextMenu::build(window, cx, move |menu, _, _| {
                                menu.entry("Move Left", None, move |_, cx| {
                                    wt_left
                                        .update(cx, |this, cx| this.move_column(col_idx, -1, cx))
                                        .ok();
                                })
                                .entry("Move Right", None, move |_, cx| {
                                    wt_right
                                        .update(cx, |this, cx| this.move_column(col_idx, 1, cx))
                                        .ok();
                                })
                                .entry("Hide Column", None, move |_, cx| {
                                    wt_hide
                                        .update(cx, |this, cx| {
                                            this.hidden_columns.insert(col_idx);
                                            this.recompute_layout();
                                            cx.notify();
                                        })
                                        .ok();
                                })
                            })
                        })
                        .into_any_element()
                    };
                    header_cells.push(header_cell);
                    header_last_end = end;
                }
                let header_right = (hx - header_last_end).max(0.0);
                let header = div()
                    .flex()
                    .flex_row()
                    .flex_none()
                    .w(px(total_width))
                    .h(px(Self::GRID_HEADER_H))
                    .border_b_1()
                    .border_color(strong_grid_border)
                    .bg(header_bg)
                    .child(div().w(px(header_left)).flex_none())
                    .children(header_cells)
                    .child(div().w(px(header_right)).flex_none());

                // Virtualized body: bounded, overflow-hidden box so uniform_list
                // builds only the visible rows.
                let body = div().flex_1().min_h_0().overflow_hidden().child(
                    uniform_list(
                        "result-rows",
                        row_count,
                        cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                            #[cfg(test)]
                            RENDERED_ROW_COUNT
                                .fetch_add(range.len(), std::sync::atomic::Ordering::Relaxed);
                            range
                                .map(|display_idx| {
                                    match display_rows_for_body.get(display_idx).copied() {
                                        Some(ResultDisplayRow::Loaded(abs_idx)) => {
                                        this.render_grid_row(
                                            abs_idx,
                                            display_idx,
                                            grid_border,
                                            zebra_bg,
                                            modified_bg,
                                            deleted_bg,
                                            cell_hover_bg,
                                            has_table_context,
                                            cx,
                                        )
                                        }
                                        Some(ResultDisplayRow::Added(added_idx)) => {
                                        this.render_added_row(
                                            added_idx,
                                            display_idx,
                                            grid_border,
                                            zebra_bg,
                                            added_bg,
                                            cell_hover_bg,
                                            has_table_context,
                                            cx,
                                        )
                                        }
                                        None => div().into_any_element(),
                                    }
                                })
                                .collect()
                        }),
                    )
                    .size_full()
                    .with_sizing_behavior(ListSizingBehavior::Auto)
                    .track_scroll(&self.scroll_handle),
                );

                let table = div()
                    .w(px(total_width))
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(header)
                    .when_some(self.render_local_filter_row(cx), |el, row| el.child(row))
                    .child(body);

                let mut h_scroll_div = div()
                    .id("result-grid")
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .h_full()
                    .overflow_x_hidden()
                    .track_scroll(&self.h_scroll)
                    .child(table);
                h_scroll_div.style().restrict_scroll_to_axis = Some(true);

                // Scrollbars live in reserved gutters (the grid shrinks to make
                // room) rather than overlaying the content, with a visible track
                // and a contrasting thumb. Pressing a gutter grabs the thumb and
                // dragging scrolls relative to the grab point; the actual move
                // tracking happens on the drag overlay below.
                let vertical_gutter = div()
                    .id("result-vscroll")
                    .debug_selector(|| "VSCROLL".to_string())
                    .flex_none()
                    .w(px(SCROLLBAR_SIZE))
                    .h_full()
                    .bg(track_color)
                    .border_l_1()
                    .border_color(track_border)
                    .relative()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            this.begin_scroll_drag(true, f32::from(event.position.y));
                            cx.notify();
                        }),
                    )
                    .child(scroll_thumb(&self.scroll_handle, true, thumb_color));
                let horizontal_gutter = div()
                    .id("result-hscroll")
                    .debug_selector(|| "HSCROLL".to_string())
                    .flex_1()
                    .h(px(SCROLLBAR_SIZE))
                    .bg(track_color)
                    .border_t_1()
                    .border_color(track_border)
                    .relative()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            this.begin_scroll_drag(false, f32::from(event.position.x));
                            cx.notify();
                        }),
                    )
                    .child(scroll_thumb(&self.h_scroll, false, thumb_color));

                // While a thumb is grabbed, a full-area overlay captures mouse
                // moves and the release, so the drag keeps tracking even when the
                // cursor leaves the narrow gutter.
                let drag_overlay = self.scroll_drag.map(|drag| {
                    div()
                        .absolute()
                        .inset_0()
                        .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
                            cx.stop_propagation();
                            if event.pressed_button != Some(MouseButton::Left) {
                                this.end_scroll_drag();
                                cx.notify();
                                return;
                            }
                            let pos = if drag.vertical {
                                f32::from(event.position.y)
                            } else {
                                f32::from(event.position.x)
                            };
                            this.update_scroll_drag(pos);
                            cx.notify();
                        }))
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|this, _: &MouseUpEvent, _, cx| {
                                cx.stop_propagation();
                                this.end_scroll_drag();
                                cx.notify();
                            }),
                        )
                });

                // Row number gutter: fixed left column that scrolls vertically
                // in sync with the body but is outside the horizontal scroll.
                let gutter_header = div()
                    .flex_none()
                    .w(px(ROW_GUTTER_WIDTH))
                    .h(px(Self::GRID_HEADER_H))
                    .border_b_1()
                    .border_color(strong_grid_border)
                    .bg(gutter_header_bg)
                    .flex()
                    .justify_center()
                    .items_center()
                    .child(
                        Label::new("Row")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    );
                let show_filter_spacer = self.local_filter_visible && self.result.is_some();
                let gutter_filter_spacer = show_filter_spacer.then(|| {
                    div()
                        .flex_none()
                        .w(px(ROW_GUTTER_WIDTH))
                        .h(px(22.))
                        .border_b_1()
                        .border_color(strong_grid_border)
                        .bg(gutter_bg)
                        .into_any_element()
                });
                let gb = grid_border;
                let gutter_row_bg = gutter_bg;
                let gutter_row_hover_bg = gutter_hover_bg;
                let gutter_selected_bg = cx.theme().colors().element_selected;
                let gutter_deleted_bg = deleted_bg;
                let gutter_selected_bar = cx.theme().colors().text_accent;
                let gutter_unselected_bar = cx.theme().colors().border_transparent;
                let gutter_row_ops = row_ops_enabled;
                let gutter_body = uniform_list(
                    "result-gutter",
                    row_count,
                    cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                        range.map(|display_idx| {
                            let Some(display_row) = display_rows_for_gutter.get(display_idx).copied() else {
                                return div().into_any_element();
                            };
                            let abs_idx = display_row.abs_idx(loaded_count);
                            let is_selected = this.selected_rows.contains(&abs_idx);
                            let is_deleted = this.deleted_rows.contains(&abs_idx);
                            let row_num: SharedString = match display_row {
                                ResultDisplayRow::Loaded(loaded_abs_idx) => this
                                    .filtered_display_order
                                    .iter()
                                    .position(|&row| row == loaded_abs_idx)
                                    .map_or(display_idx + 1, |idx| idx + 1)
                                    .to_string()
                                    .into(),
                                ResultDisplayRow::Added(added_idx) => {
                                    format!("+{}", added_idx + 1).into()
                                }
                            };
                            let gutter_row = div()
                                .id(ElementId::from(SharedString::from(format!("gtr-{display_idx}"))))
                                .debug_selector(move || format!("GUTTER-{display_idx}"))
                                .flex_none()
                                .w(px(ROW_GUTTER_WIDTH))
                                .h(px(Self::GRID_ROW_H))
                                .border_b_1()
                                .border_color(gb)
                                .flex()
                                .items_center()
                                .bg(gutter_row_bg)
                                .cursor_pointer()
                                .when(!is_deleted, move |el| {
                                    el.hover(move |style| style.bg(gutter_row_hover_bg))
                                })
                                .when(is_selected, move |el| el.bg(gutter_selected_bg))
                                .when(is_deleted, move |el| el.bg(gutter_deleted_bg))
                                .child(
                                    div()
                                        .w(px(3.))
                                        .h_full()
                                        .flex_none()
                                        .bg(if is_selected {
                                            gutter_selected_bar
                                        } else {
                                            gutter_unselected_bar
                                        }),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .pr_1()
                                        .flex()
                                        .justify_end()
                                        .child(
                                            Label::new(row_num)
                                                .size(LabelSize::Small)
                                                .color(if is_selected {
                                                    Color::Accent
                                                } else {
                                                    Color::Muted
                                                }),
                                        ),
                                )
                                .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _window, cx| {
                                    let gpui::ClickEvent::Mouse(mouse) = event else { return; };
                                    if mouse.down.modifiers.shift {
                                        let anchor_display_idx = this.last_selected_row.unwrap_or(display_idx);
                                        this.select_row_range(anchor_display_idx, display_idx);
                                        // Don't update anchor — keep extending from original pivot.
                                    } else if mouse.down.modifiers.control {
                                        if this.selected_rows.contains(&abs_idx) {
                                            this.selected_rows.remove(&abs_idx);
                                            this.selected_cell = None;
                                            this.selected_cell_range = None;
                                        } else {
                                            this.select_entire_row(abs_idx, display_idx);
                                        }
                                        this.last_selected_row = Some(display_idx);
                                    } else {
                                        this.select_entire_row(abs_idx, display_idx);
                                    }
                                    cx.notify();
                                }));
                            if gutter_row_ops {
                                let wt = weak_for_gutter.clone();
                                right_click_menu(ElementId::from(SharedString::from(format!("gtr-ctx-{display_idx}"))))
                                    .trigger(move |_, _, _| gutter_row)
                                    .menu(move |window, cx| {
                                        let wt_add = wt.clone();
                                        let wt_del = wt.clone();
                                        let wt_clone = wt.clone();
                                        ContextMenu::build(window, cx, move |menu, _, _| {
                                            menu
                                                .entry("Add Row", None, move |window, cx| {
                                                    wt_add
                                                        .update(cx, |this, cx| {
                                                            this.select_row_for_context_action(abs_idx, 0);
                                                            if let Some(added_idx) =
                                                                this.add_blank_row_after(Some(abs_idx), cx)
                                                            {
                                                                this.begin_added_row_edit(added_idx, window, cx);
                                                            }
                                                        })
                                                        .ok();
                                                })
                                                .entry("Clone Row", None, move |window, cx| {
                                                    wt_clone
                                                        .update(cx, |this, cx| {
                                                            this.select_row_for_context_action(abs_idx, 0);
                                                            if let Some(added_idx) =
                                                                this.clone_row_after(abs_idx, abs_idx, cx)
                                                            {
                                                                this.begin_added_row_edit(added_idx, window, cx);
                                                            }
                                                        })
                                                        .ok();
                                                })
                                                .entry("Delete Row", None, move |_, cx| {
                                                    wt_del
                                                        .update(cx, |this, cx| {
                                                            this.select_row_for_context_action(abs_idx, 0);
                                                            this.toggle_delete_selected_row(cx);
                                                        })
                                                        .ok();
                                                })
                                        })
                                    })
                                    .into_any_element()
                            } else {
                                gutter_row.into_any_element()
                            }
                        })
                        .collect()
                    }),
                )
                .size_full()
                .with_sizing_behavior(ListSizingBehavior::Auto)
                .track_scroll(&self.scroll_handle);
                let gutter_col = div()
                    .flex_none()
                    .w(px(ROW_GUTTER_WIDTH))
                    .h_full()
                    .flex()
                    .flex_col()
                    .border_r_1()
                    .border_color(strong_grid_border)
                    .bg(gutter_bg)
                    .child(gutter_header)
                    .when_some(gutter_filter_spacer, |el, spacer| el.child(spacer))
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .overflow_hidden()
                            .child(gutter_body),
                    );

                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    // Escape clears multi-row selection without navigating away.
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                        if event.keystroke.key.as_str() == "escape"
                            && !event.keystroke.modifiers.modified()
                            && (!this.selected_rows.is_empty()
                                || this.selected_cell_range.is_some())
                        {
                            this.selected_rows.clear();
                            this.selected_cell_range = None;
                            this.last_selected_row = None;
                            cx.notify();
                        }
                    }))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_1()
                            .min_h_0()
                            .child(gutter_col)
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .min_h_0()
                                    .flex()
                                    .flex_col()
                                    .child(
                                        div()
                                            .flex()
                                            .flex_row()
                                            .flex_1()
                                            .min_w_0()
                                            .min_h_0()
                                            .child(h_scroll_div)
                                            .child(vertical_gutter),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .flex_row()
                                            .flex_none()
                                            .child(horizontal_gutter)
                                            .child(
                                                div()
                                                    .flex_none()
                                                    .w(px(SCROLLBAR_SIZE))
                                                    .h(px(SCROLLBAR_SIZE))
                                                    .bg(track_color),
                                            ),
                                    ),
                            ),
                    )
                    .children(drag_overlay)
                    .when_some(self.render_value_editor_popup(cx), |el, popup| el.child(popup))
                    .when_some(self.render_value_editor_resize_overlay(cx), |el, overlay| {
                        el.child(overlay)
                    })
                    .when_some(self.render_enum_popup(cx), |el, popup| el.child(popup))
                    .when_some(self.render_column_list_popup(cx), |el, popup| el.child(popup))
                    .when_some(self.render_query_history_popup(cx), |el, popup| el.child(popup))
                    .into_any_element()
            })
            .into_any_element()
    }

    // Renders the result transposed: the first column lists the original column
    // names and each following column is one record. Bounded to the loaded rows
    // so a large result does not build an unbounded number of columns.
    fn render_transposed(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(result) = self.result.as_ref() else {
            return div().into_any_element();
        };
        let grid_border = cx.theme().colors().border.opacity(0.46);
        let header_bg = cx.theme().colors().editor_subheader_background;
        let name_bg = cx.theme().colors().editor_gutter_background;

        const MAX_RECORDS: usize = 500;
        let record_count = result.rows.len().min(MAX_RECORDS);

        let header = h_flex()
            .flex_none()
            .h(px(Self::GRID_HEADER_H))
            .border_b_1()
            .border_color(grid_border)
            .bg(header_bg)
            .child(
                div()
                    .flex_none()
                    .w(px(180.0))
                    .px_2()
                    .h_full()
                    .flex()
                    .items_center()
                    .border_r_1()
                    .border_color(grid_border)
                    .child(
                        Label::new("Column")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .children((0..record_count).map(|record_idx| {
                div()
                    .flex_none()
                    .w(px(180.0))
                    .px_2()
                    .h_full()
                    .flex()
                    .items_center()
                    .border_r_1()
                    .border_color(grid_border)
                    .child(
                        Label::new(format!("Row {}", record_idx + 1))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .into_any_element()
            }));

        let rows = result
            .columns
            .iter()
            .enumerate()
            .map(|(col_idx, col_name)| {
                h_flex()
                    .flex_none()
                    .h(px(Self::GRID_ROW_H))
                    .border_b_1()
                    .border_color(grid_border)
                    .child(
                        div()
                            .flex_none()
                            .w(px(180.0))
                            .px_2()
                            .h_full()
                            .flex()
                            .items_center()
                            .border_r_1()
                            .border_color(grid_border)
                            .bg(name_bg)
                            .child(
                                Label::new(col_name.clone())
                                    .size(LabelSize::Small)
                                    .color(Color::Default),
                            ),
                    )
                    .children((0..record_count).map(|record_idx| {
                        let (display, color) = render_loaded_value(
                            result
                                .rows
                                .get(record_idx)
                                .and_then(|row| row.get(col_idx))
                                .and_then(|cell| cell.as_deref()),
                        );
                        div()
                            .flex_none()
                            .w(px(180.0))
                            .px_2()
                            .h_full()
                            .flex()
                            .items_center()
                            .border_r_1()
                            .border_color(grid_border)
                            .overflow_hidden()
                            .child(Label::new(display).size(LabelSize::Small).color(color))
                            .into_any_element()
                    }))
                    .into_any_element()
            });

        div()
            .id("result-transpose")
            .debug_selector(|| "TRANSPOSE_VIEW".to_string())
            .flex_1()
            .min_h_0()
            .overflow_scroll()
            .child(div().flex().flex_col().child(header).children(rows))
            .into_any_element()
    }


    fn selected_cell_full_value(&self) -> Option<String> {
        let (abs_idx, col_idx) = self.selected_cell?;
        // Pending edit wins over the loaded value.
        if let Some(cv) = self.pending_cell_value(abs_idx, col_idx) {
            return Some(match cv {
                CellValue::Null => NULL_MARKER.to_string(),
                CellValue::Default => DEFAULT_MARKER.to_string(),
                CellValue::Text(s) => s.clone(),
            });
        }
        self.result
            .as_ref()?
            .rows
            .get(abs_idx)?
            .get(col_idx)?
            .as_deref()
            .map(|s| s.to_string())
            .or_else(|| Some(NULL_MARKER.to_string()))
    }

    // The value editor is writable only for table-backed results, where an edit
    // can be turned into an UPDATE/INSERT. Plain query results stay read-only.
    fn value_editor_is_editable(&self) -> bool {
        self.table_name.is_some() && self.selected_cell.is_some()
    }

    fn sync_value_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.value_editor_open {
            return;
        }
        let value = self.selected_cell_full_value().unwrap_or_default();
        let editable = self.value_editor_is_editable();
        let (editor, created) = match self.value_editor.clone() {
            Some(editor) => (editor, false),
            None => {
                let editor = cx.new(|cx| {
                    let mut editor = Editor::multi_line(window, cx);
                    editor.set_show_gutter(false, cx);
                    editor.disable_expand_excerpt_buttons(cx);
                    editor.set_minimap_visibility(MinimapVisibility::Disabled, window, cx);
                    editor.set_soft_wrap_mode(SoftWrap::EditorWidth, cx);
                    editor.set_show_indent_guides(false, cx);
                    editor.disable_mouse_wheel_zoom();
                    editor
                });
                self.value_editor = Some(editor.clone());
                (editor, true)
            }
        };
        // Reload the text only when the editor opens or the targeted cell
        // changes. While the user is typing in the same cell, leave it alone so
        // a re-render does not discard the in-progress edit.
        let cell_changed = self.value_editor_cell != self.selected_cell;
        if created || cell_changed {
            editor.update(cx, |editor, cx| {
                editor.set_read_only(false);
                editor.set_text(value, window, cx);
            });
            self.value_editor_cell = self.selected_cell;
        }
        editor.update(cx, |editor, _cx| editor.set_read_only(!editable));
    }

    // Writes the value-editor text into the pending buffer for the selected cell
    // and closes the popup. No-op for read-only (plain query) results.
    fn commit_value_editor(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.value_editor_is_editable() {
            return;
        }
        let Some((abs_idx, col_idx)) = self.selected_cell else {
            return;
        };
        let Some(editor) = self.value_editor.clone() else {
            return;
        };
        let raw_text = editor.read(cx).text(cx);
        let new_value = CellValue::from_text(raw_text);

        let added_idx = self
            .result
            .as_ref()
            .map(|result| result.rows.len())
            .filter(|loaded| abs_idx >= *loaded)
            .map(|loaded| abs_idx - loaded);
        if let Some(added_idx) = added_idx {
            if let Some(cell) = self
                .added_rows
                .get_mut(added_idx)
                .and_then(|row| row.get_mut(col_idx))
            {
                *cell = new_value;
            }
        } else {
            self.buffer_loaded_cell_value(abs_idx, col_idx, new_value, cx);
        }
        self.value_editor_open = false;
        self.value_editor_resize_drag = None;
        cx.notify();
    }

    // Reformats the value-editor text as indented JSON when it parses as JSON.
    // Leaves the text unchanged otherwise.
    fn format_value_editor_json(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.value_editor.clone() else {
            return;
        };
        let text = editor.read(cx).text(cx);
        let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&text) else {
            return;
        };
        let Ok(pretty) = serde_json::to_string_pretty(&parsed) else {
            return;
        };
        editor.update(cx, |editor, cx| {
            let was_read_only = editor.read_only(cx);
            editor.set_read_only(false);
            editor.set_text(pretty, window, cx);
            editor.set_read_only(was_read_only);
        });
        cx.notify();
    }

    fn value_editor_text_is_json(&self, cx: &App) -> bool {
        self.value_editor
            .as_ref()
            .map(|editor| editor.read(cx).text(cx))
            .is_some_and(|text| serde_json::from_str::<serde_json::Value>(&text).is_ok())
    }

    // Moves a data column left (delta -1) or right (delta +1) in the display
    // order, skipping over hidden columns so the move is visible to the user.
    fn move_column(&mut self, data_col: usize, delta: isize, cx: &mut Context<Self>) {
        let Some(current) = self.column_order.iter().position(|&c| c == data_col) else {
            return;
        };
        let mut target = current as isize;
        loop {
            target += delta;
            if target < 0 || target as usize >= self.column_order.len() {
                return;
            }
            let candidate = self.column_order[target as usize];
            if !self.hidden_columns.contains(&candidate) {
                break;
            }
        }
        let item = self.column_order.remove(current);
        self.column_order.insert(target as usize, item);
        self.recompute_layout();
        cx.notify();
    }

    fn reset_view(&mut self, cx: &mut Context<Self>) {
        self.hidden_columns.clear();
        self.sort_columns.clear();
        self.local_filters.clear();
        self.local_filter_editors.clear();
        self.transposed = false;
        if let Some(result) = self.result.as_ref() {
            self.column_order = (0..result.columns.len()).collect();
        } else {
            self.column_order.clear();
        }
        self.recompute_layout();
        cx.notify();
    }

    fn toggle_transpose(&mut self, cx: &mut Context<Self>) {
        self.transposed = !self.transposed;
        cx.notify();
    }

    fn open_export_dialog(&mut self, cx: &mut Context<Self>) {
        self.export_dialog_open = !self.export_dialog_open;
        if self.export_dialog_open && self.export_format.needs_table() && self.table_name.is_none()
        {
            self.export_format = ExportChoice::Csv;
        }
        cx.notify();
    }

    // Loads the backing table's DDL so the Add DDL option can prepend it. No-op
    // when there is no table context; failures are logged, not surfaced.
    fn fetch_export_ddl(&mut self, cx: &mut Context<Self>) {
        if self.export_ddl.is_some() {
            return;
        }
        let (Some(store), Some(connection_id), Some(table)) = (
            self.store.clone(),
            self.connection_id,
            self.table_name.clone(),
        ) else {
            return;
        };
        let database = self.database.clone().unwrap_or_default();
        cx.spawn(async move |this, cx| {
            let ddl = store
                .update(cx, |store, cx| {
                    store.get_table_ddl(connection_id, database, table, cx)
                })
                .ok();
            if let Some(task) = ddl {
                if let Some(text) = task.await.log_err() {
                    this.update(cx, |this, cx| {
                        this.export_ddl = Some(text);
                        cx.notify();
                    })
                    .log_err();
                }
            }
        })
        .detach();
    }

    // Assembles the export text honoring the current dialog options.
    fn current_export_text(&self) -> Option<String> {
        let result = self.result.as_ref()?;
        let ddl = if self.export_add_ddl {
            self.export_ddl.as_deref()
        } else {
            None
        };
        Some(Self::build_export_text(
            result,
            self.export_format,
            self.export_headers,
            self.export_transpose,
            self.table_name.as_deref(),
            ddl,
        ))
    }

    fn export_to_clipboard(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = self.current_export_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            self.status_message = Some("Copied result to clipboard".to_string());
            self.export_dialog_open = false;
            cx.notify();
        }
    }

    fn export_to_file(&mut self, cx: &mut Context<Self>) {
        let Some(text) = self.current_export_text() else {
            return;
        };
        let default_name = format!("result.{}", self.export_format.extension());
        let home = paths::home_dir().to_path_buf();
        let path_rx = cx.prompt_for_new_path(&home, Some(&default_name));
        cx.background_spawn(async move {
            if let Some(path) = path_rx
                .await
                .log_err()
                .and_then(|result| result.log_err())
                .flatten()
            {
                std::fs::write(path, text).log_err();
            }
        })
        .detach();
        self.export_dialog_open = false;
        cx.notify();
    }

    fn toggle_chart(&mut self, cx: &mut Context<Self>) {
        self.chart_open = !self.chart_open;
        if self.chart_open && self.chart_value_column.is_none() {
            if let Some(result) = self.result.as_ref() {
                self.chart_value_column = Self::first_numeric_column(result);
            }
        }
        cx.notify();
    }

    // Query history filtered by the search box, newest first, paired with the
    // original index so a click still maps to the right entry.
    fn filtered_history(&self) -> Vec<(usize, String)> {
        let needle = self.history_search.trim().to_lowercase();
        self.query_history
            .iter()
            .enumerate()
            .filter(|(_, sql)| needle.is_empty() || sql.to_lowercase().contains(&needle))
            .map(|(index, sql)| (index, sql.clone()))
            .collect()
    }

    fn render_value_editor_popup(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.value_editor_open {
            return None;
        }
        let value = self.selected_cell_full_value().unwrap_or_default();
        let editor = self.value_editor.clone()?;
        let editable = self.value_editor_is_editable();
        let show_format_json = self.value_editor_text_is_json(cx);
        let (abs_idx, col_idx) = self.selected_cell?;
        let display_idx = self.display_idx_of(abs_idx)?;
        let display_pos = self
            .visible_columns
            .iter()
            .position(|&col| col == col_idx)?;
        let col_left = display_pos
            .checked_sub(1)
            .and_then(|pos| self.column_edges.get(pos).copied())
            .unwrap_or(0.0);
        let col_width = self
            .col_widths
            .get(display_pos)
            .copied()
            .map(f32::from)
            .unwrap_or(160.0);
        let (_, _, _, scroll_y) = axis_metrics(&self.scroll_handle, true);
        let filter_offset = if self.local_filter_visible { 22.0 } else { 0.0 };
        let cell_top =
            Self::GRID_HEADER_H + filter_offset + display_idx as f32 * Self::GRID_ROW_H + scroll_y;
        let grid_width = f32::from(self.h_scroll.bounds().size.width).max(560.0);
        let grid_height = f32::from(self.scroll_handle.viewport().size.height).max(360.0);
        let (auto_width, auto_height) =
            Self::value_editor_auto_size(&value, grid_width, grid_height);
        let (popup_width, popup_height) = self
            .value_editor_size
            .unwrap_or((auto_width.max(col_width), auto_height));
        let popup_width = popup_width.clamp(360.0, (grid_width - 16.0).max(360.0));
        let popup_height = popup_height.clamp(220.0, (grid_height - 16.0).max(220.0));
        let screen_x = col_left + f32::from(self.h_scroll.offset().x);
        let left = screen_x
            .max(4.0)
            .min((grid_width - popup_width - 8.0).max(4.0));
        let top = if cell_top > popup_height + 8.0 {
            cell_top - popup_height - 4.0
        } else {
            cell_top + Self::GRID_ROW_H + 4.0
        }
        .max(4.0);
        let col_name = self
            .selected_cell
            .and_then(|(_, col_idx)| {
                self.result
                    .as_ref()
                    .and_then(|r| r.columns.get(col_idx))
                    .cloned()
            })
            .unwrap_or_default();

        Some(
            v_flex()
                .id("value-editor-popup")
                .debug_selector(|| "VALUE_EDITOR_POPUP".to_string())
                .absolute()
                .left(px(left))
                .top(px(top))
                .w(px(popup_width))
                .h(px(popup_height))
                .elevation_2(cx)
                .occlude()
                .on_scroll_wheel(cx.listener(|_, _: &ScrollWheelEvent, _, cx| {
                    cx.stop_propagation();
                }))
                .child(
                    h_flex()
                        .px_2()
                        .py_1()
                        .border_b_1()
                        .border_color(cx.theme().colors().border_variant)
                        .child(
                            Label::new(if col_name.is_empty() {
                                "Value".to_string()
                            } else {
                                col_name
                            })
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                        )
                        .child(div().flex_1())
                        .when(show_format_json, |row| {
                            row.child(
                                Button::new("value-editor-format-json", "Format JSON")
                                    .style(ButtonStyle::Subtle)
                                    .label_size(LabelSize::Small)
                                    .tooltip(Tooltip::text("Pretty-print the JSON value"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.format_value_editor_json(window, cx);
                                    })),
                            )
                        })
                        .when(editable, |row| {
                            row.child(
                                Button::new("value-editor-save", "Submit")
                                    .style(ButtonStyle::Filled)
                                    .label_size(LabelSize::Small)
                                    .tooltip(Tooltip::text("Apply to pending edits (Ctrl+Enter)"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.commit_value_editor(window, cx);
                                    })),
                            )
                        })
                        .child(
                            IconButton::new("value-editor-copy", IconName::Copy)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text("Copy full value"))
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(value.clone()));
                                })),
                        )
                        .child(
                            IconButton::new("value-editor-close", IconName::Close)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text("Close value panel"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.value_editor_open = false;
                                    this.value_editor_resize_drag = None;
                                    cx.notify();
                                })),
                        ),
                )
                .child(
                    div()
                        .id("value-editor-scroll")
                        .debug_selector(|| "VALUE_EDITOR_SCROLL".to_string())
                        .flex_1()
                        .min_h_0()
                        .overflow_x_scroll()
                        .overflow_y_scroll()
                        .p_2()
                        .on_scroll_wheel(cx.listener(|_, _: &ScrollWheelEvent, _, cx| {
                            cx.stop_propagation();
                        }))
                        // Ctrl+Enter applies the edit; Escape closes the panel.
                        // Enter alone inserts a newline (multi-line values).
                        .when(editable, |container| {
                            container.capture_key_down(cx.listener(
                                |this, event: &KeyDownEvent, window, cx| {
                                    let modifiers = &event.keystroke.modifiers;
                                    match event.keystroke.key.as_str() {
                                        "enter" if modifiers.secondary() => {
                                            this.commit_value_editor(window, cx);
                                        }
                                        "escape" => {
                                            this.value_editor_open = false;
                                            this.value_editor_resize_drag = None;
                                            cx.notify();
                                        }
                                        _ => {}
                                    }
                                },
                            ))
                        })
                        .child(editor),
                )
                .child(
                    div()
                        .id("value-editor-resize")
                        .debug_selector(|| "VALUE_EDITOR_RESIZE".to_string())
                        .absolute()
                        .right_0()
                        .bottom_0()
                        .w(px(18.0))
                        .h(px(18.0))
                        .cursor_nwse_resize()
                        .border_r_2()
                        .border_b_2()
                        .border_color(cx.theme().colors().text_muted)
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                                cx.stop_propagation();
                                this.begin_value_editor_resize(
                                    f32::from(event.position.x),
                                    f32::from(event.position.y),
                                    popup_width,
                                    popup_height,
                                );
                                cx.notify();
                            }),
                        ),
                )
                .into_any_element(),
        )
    }

    fn render_value_editor_resize_overlay(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.value_editor_resize_drag?;

        Some(
            div()
                .absolute()
                .inset_0()
                .cursor_nwse_resize()
                .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                    cx.stop_propagation();
                    if event.pressed_button != Some(MouseButton::Left) {
                        this.end_value_editor_resize();
                        cx.notify();
                        return;
                    }
                    this.update_value_editor_resize(
                        f32::from(event.position.x),
                        f32::from(event.position.y),
                    );
                    cx.notify();
                }))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseUpEvent, _, cx| {
                        cx.stop_propagation();
                        this.end_value_editor_resize();
                        cx.notify();
                    }),
                )
                .into_any_element(),
        )
    }


    fn record_view_display_idx(&self) -> Option<usize> {
        // Clamp to the filtered display order so changing filters never leaves
        // a stale index pointing past the end.
        let max = self.filtered_display_order.len().saturating_sub(1);
        self.record_view_row.map(|r| r.min(max))
    }

    fn render_record_view_panel(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        if !self.record_view_open {
            return None;
        }
        let result = self.result.as_ref()?;
        let display_len = self.filtered_display_order.len();
        if display_len == 0 {
            return None;
        }

        let display_idx = self.record_view_display_idx().unwrap_or(0);
        let abs_idx = *self.filtered_display_order.get(display_idx)?;
        let row = result.rows.get(abs_idx)?;

        let rows: Vec<AnyElement> = result
            .columns
            .iter()
            .enumerate()
            .map(|(col_idx, col_name)| {
                let raw_val = row.get(col_idx).and_then(|v| v.as_deref());
                let (text, color) = render_loaded_value(raw_val);
                h_flex()
                    .id(("rv-row", col_idx))
                    .px_2()
                    .py_px()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .gap_2()
                    .child(
                        div().w(gpui::px(160.0)).flex_none().child(
                            Label::new(col_name.clone())
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                    )
                    .child(Label::new(text).size(LabelSize::Small).color(color))
                    .into_any_element()
            })
            .collect();

        let row_label = format!("Row {} / {}", display_idx + 1, display_len);
        let at_first = display_idx == 0;
        let at_last = display_idx + 1 >= display_len;

        let panel = v_flex()
            .h(gpui::px(200.0))
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().surface_background)
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .gap_2()
                    .child(
                        Label::new("Record View")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1())
                    .child(
                        Label::new(row_label)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        IconButton::new("rv-prev", IconName::ArrowLeft)
                            .icon_size(IconSize::Small)
                            .disabled(at_first)
                            .tooltip(Tooltip::text("Previous row"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.record_view_step(-1, cx);
                            })),
                    )
                    .child(
                        IconButton::new("rv-next", IconName::ArrowRight)
                            .icon_size(IconSize::Small)
                            .disabled(at_last)
                            .tooltip(Tooltip::text("Next row"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.record_view_step(1, cx);
                            })),
                    )
                    .child(
                        IconButton::new("rv-close", IconName::Close)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Close record view"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.record_view_open = false;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .id("record-view-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(rows),
            );

        Some(panel)
    }

    fn render_quick_doc_panel(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        if !self.quick_doc_open {
            return None;
        }
        let infos = self.column_infos.as_deref()?;
        let result = self.result.as_ref()?;
        let col_idx = self.selected_cell.map(|(_, c)| c).unwrap_or(0);
        let col_name = result.columns.get(col_idx)?;
        let info = infos.get(col_idx)?;

        let nullable = if info.is_nullable { "YES" } else { "NO" };
        let key = info.column_key.as_deref().unwrap_or("—");
        let default = info.default_value.as_deref().unwrap_or("—");

        let mut rows: Vec<AnyElement> = vec![
            ("Column", col_name.as_str()),
            ("Type", info.data_type.as_str()),
            ("Nullable", nullable),
            ("Key", key),
            ("Default", default),
        ]
        .into_iter()
        .map(|(label, value)| {
            h_flex()
                .px_2()
                .py_px()
                .border_b_1()
                .border_color(cx.theme().colors().border_variant)
                .gap_2()
                .child(
                    div()
                        .w(gpui::px(80.0))
                        .flex_none()
                        .child(Label::new(label).size(LabelSize::Small).color(Color::Muted)),
                )
                .child(Label::new(value.to_string()).size(LabelSize::Small))
                .into_any_element()
        })
        .collect();

        if !info.extra.is_empty() {
            rows.push(
                h_flex()
                    .px_2()
                    .py_px()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .gap_2()
                    .child(
                        div().w(gpui::px(80.0)).flex_none().child(
                            Label::new("Extra")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                    )
                    .child(Label::new(info.extra.clone()).size(LabelSize::Small))
                    .into_any_element(),
            );
        }

        let panel = v_flex()
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().surface_background)
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .gap_2()
                    .child(
                        Label::new("Column Info")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1())
                    .child(
                        IconButton::new("qdoc-close", IconName::Close)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Close"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.quick_doc_open = false;
                                cx.notify();
                            })),
                    ),
            )
            .children(rows);

        Some(panel)
    }

    // Resolves the display-order index of an absolute row index. Used to keep
    // the record view in sync when the user clicks a cell.
    fn display_idx_of(&self, abs_idx: usize) -> Option<usize> {
        let loaded_count = self.loaded_row_count();
        self.display_row_entries()
            .into_iter()
            .position(|row| row.abs_idx(loaded_count) == abs_idx)
    }

    fn navigate_to_fk_row(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some((abs_idx, col_idx)) = self.selected_cell else {
            return;
        };
        let Some(fk) = self.fk_columns.get(&col_idx).cloned() else {
            return;
        };
        let cell_value = self
            .result
            .as_ref()
            .and_then(|r| r.rows.get(abs_idx))
            .and_then(|row| row.get(col_idx))
            .and_then(|v| v.as_deref());
        let Some(value) = cell_value else {
            return;
        };
        let (Some(store), Some(conn_id), Some(db)) = (
            self.store.clone(),
            self.connection_id,
            self.database.clone(),
        ) else {
            return;
        };
        let escaped = value.replace('\'', "''");
        let sql = format!(
            "SELECT * FROM `{}` WHERE `{}` = '{}'",
            fk.to_table, fk.to_column, escaped
        );
        self.run_sql(store, conn_id, db, sql, cx);
    }

    fn record_view_step(&mut self, delta: i64, cx: &mut Context<Self>) {
        let max = self.filtered_display_order.len().saturating_sub(1);
        let current = self.record_view_display_idx().unwrap_or(0) as i64;
        let next = (current + delta).clamp(0, max as i64) as usize;
        self.record_view_row = Some(next);
        cx.notify();
    }


    fn open_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.find_editor.is_none() {
            let editor = cx.new(|cx| Editor::single_line(window, cx));
            cx.subscribe(&editor, |this, _, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    this.update_find_matches(cx);
                }
            })
            .detach();
            self.find_editor = Some(editor);
        }
        self.find_query = Some(String::new());
        self.find_matches.clear();
        self.find_current = 0;
        if let Some(editor) = &self.find_editor {
            let handle = editor.read(cx).focus_handle(cx);
            window.focus(&handle, cx);
        }
        cx.notify();
    }

    fn close_find(&mut self, cx: &mut Context<Self>) {
        self.find_query = None;
        self.find_matches.clear();
        self.find_current = 0;
        if self.find_filter_rows {
            self.recompute_local_filter_inner();
        }
        cx.notify();
    }

    fn toggle_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.find_query.is_some() {
            self.close_find(cx);
        } else {
            self.open_find(window, cx);
        }
    }

    fn toggle_query_history(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.history_open = !self.history_open;
        if !self.history_open {
            cx.notify();
            return;
        }
        self.history_search.clear();
        if self.history_search_editor.is_none() {
            let editor = cx.new(|cx| Editor::single_line(window, cx));
            cx.subscribe(&editor, |this, editor, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    this.history_search = editor.read(cx).text(cx);
                    cx.notify();
                }
            })
            .detach();
            self.history_search_editor = Some(editor);
        } else if let Some(editor) = &self.history_search_editor {
            editor.update(cx, |editor, cx| editor.set_text("", window, cx));
        }
        if let Some(editor) = &self.history_search_editor {
            let handle = editor.read(cx).focus_handle(cx);
            window.focus(&handle, cx);
        }
        cx.notify();
    }

    fn update_find_matches(&mut self, cx: &mut Context<Self>) {
        let query = self
            .find_editor
            .as_ref()
            .map(|ed| ed.read(cx).text(cx).to_lowercase())
            .unwrap_or_default();

        self.find_query = Some(query.clone());
        self.find_matches.clear();
        self.find_current = 0;

        if query.is_empty() {
            if self.find_filter_rows {
                self.recompute_local_filter_inner();
            }
            cx.notify();
            return;
        }

        if let Some(result) = &self.result {
            for &abs_idx in &self.order {
                if let Some(row) = result.rows.get(abs_idx) {
                    for (col_idx, cell) in row.iter().enumerate() {
                        let text = cell.as_deref().unwrap_or("").to_lowercase();
                        if text.contains(&query) {
                            self.find_matches.push((abs_idx, col_idx));
                        }
                    }
                }
            }
        }
        if self.find_filter_rows {
            self.recompute_local_filter_inner();
        }
        cx.notify();
    }

    fn toggle_find_filter_rows(&mut self, cx: &mut Context<Self>) {
        self.find_filter_rows = !self.find_filter_rows;
        self.recompute_local_filter_inner();
        cx.notify();
    }

    fn find_next(&mut self, cx: &mut Context<Self>) {
        if self.find_matches.is_empty() {
            return;
        }
        self.find_current = (self.find_current + 1) % self.find_matches.len();
        self.scroll_to_find_match(cx);
    }

    fn find_previous(&mut self, cx: &mut Context<Self>) {
        if self.find_matches.is_empty() {
            return;
        }
        self.find_current = self
            .find_current
            .checked_sub(1)
            .unwrap_or(self.find_matches.len() - 1);
        self.scroll_to_find_match(cx);
    }

    fn scroll_to_find_match(&mut self, cx: &mut Context<Self>) {
        let Some(&(abs_idx, _)) = self.find_matches.get(self.find_current) else {
            return;
        };
        // Map abs_idx (content row) to display_idx (position in the sorted order vec).
        if let Some(display_idx) = self.order.iter().position(|&a| a == abs_idx) {
            self.scroll_handle
                .scroll_to_item(display_idx, gpui::ScrollStrategy::Center);
        }
        cx.notify();
    }

    fn render_find_bar(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let _ = self.find_query.as_ref()?;
        let editor = self.find_editor.as_ref()?.clone();

        let total = self.find_matches.len();
        let current_label = if total == 0 {
            "No matches".to_string()
        } else {
            format!("{} / {}", self.find_current + 1, total)
        };

        let bar = h_flex()
            .px_2()
            .py_1()
            .gap_2()
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().surface_background)
            .child(
                IconButton::new("find-prev", IconName::ArrowUp)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Previous match (Shift+Ctrl+G)"))
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.find_previous(cx);
                    })),
            )
            .child(
                IconButton::new("find-next", IconName::ArrowDown)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Next match (Ctrl+G)"))
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.find_next(cx);
                    })),
            )
            .child(div().w(gpui::px(200.0)).child(editor))
            .child(
                Label::new(current_label)
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                Button::new("find-filter-rows", "Filter rows")
                    .style(ButtonStyle::Subtle)
                    .label_size(LabelSize::Small)
                    .toggle_state(self.find_filter_rows)
                    .tooltip(Tooltip::text("Hide rows without a match"))
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.toggle_find_filter_rows(cx);
                    })),
            )
            .child(div().flex_1())
            .child(
                IconButton::new("find-close", IconName::Close)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Close (Escape)"))
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.close_find(cx);
                    })),
            );

        Some(bar)
    }

    // Renders the go-to-row input bar shown above the find bar. Enter jumps to
    // the entered row; Escape closes it.
    fn render_goto_row_bar(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        if !self.goto_row_visible {
            return None;
        }
        let editor = self.goto_row_editor.as_ref()?.clone();
        let total = self.filtered_display_order.len();
        let bar = h_flex()
            .px_2()
            .py_1()
            .gap_2()
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().surface_background)
            .child(
                Label::new("Go to row")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                div()
                    .w(gpui::px(120.0))
                    .capture_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                        match event.keystroke.key.as_str() {
                            "enter" if !event.keystroke.modifiers.modified() => {
                                this.confirm_goto_row(cx);
                            }
                            "escape" if !event.keystroke.modifiers.modified() => {
                                this.close_goto_row(cx);
                            }
                            _ => {}
                        }
                    }))
                    .child(editor),
            )
            .child(
                Label::new(format!("of {total}"))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(div().flex_1())
            .child(
                IconButton::new("goto-row-close", IconName::Close)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Close (Escape)"))
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.close_goto_row(cx);
                    })),
            );
        Some(bar)
    }

    // Renders the pending-changes preview: the SQL that Submit (or Commit) would
    // run, shown read-only so the user can review before writing.
    fn render_pending_preview_popup(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.preview_open {
            return None;
        }
        let body = match self.build_pending_statements(cx) {
            Ok(statements) if statements.is_empty() => "No pending changes.".to_string(),
            Ok(statements) => statements
                .iter()
                .map(|statement| format!("{statement};"))
                .collect::<Vec<_>>()
                .join("\n\n"),
            Err(note) => note,
        };
        let lines: Vec<AnyElement> = body
            .lines()
            .map(|line| {
                Label::new(line.to_string())
                    .size(LabelSize::Small)
                    .buffer_font(cx)
                    .into_any_element()
            })
            .collect();

        Some(
            popup_surface(cx)
                .id("pending-preview-popup")
                .absolute()
                .top_8()
                .right_2()
                .w(px(520.0))
                .max_h(px(400.0))
                .child(
                    h_flex()
                        .px_2()
                        .py_1()
                        .justify_between()
                        .items_center()
                        .border_b_1()
                        .border_color(cx.theme().colors().border)
                        .child(
                            Label::new("Pending changes")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .child(
                            h_flex()
                                .gap_1()
                                .child(
                                    Button::new("preview-submit", "Submit")
                                        .style(ButtonStyle::Filled)
                                        .label_size(LabelSize::Small)
                                        .tooltip(Tooltip::text(
                                            "Write pending changes to the database",
                                        ))
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.preview_open = false;
                                            this.submit_pending_edits(window, cx);
                                        })),
                                )
                                .child(
                                    IconButton::new("preview-close", IconName::Close)
                                        .icon_size(IconSize::Small)
                                        .tooltip(Tooltip::text("Close"))
                                        .on_click(cx.listener(|this, _, _window, cx| {
                                            this.preview_open = false;
                                            cx.notify();
                                        })),
                                ),
                        ),
                )
                .child(
                    div()
                        .id("pending-preview-scroll")
                        .p_2()
                        .max_h(px(340.0))
                        .overflow_y_scroll()
                        .child(v_flex().children(lines)),
                )
                .into_any_element(),
        )
    }


    fn recompute_local_filter(&mut self, cx: &mut Context<Self>) {
        let num_cols = self.result.as_ref().map(|r| r.columns.len()).unwrap_or(0);
        self.local_filters = (0..num_cols)
            .map(|i| {
                self.local_filter_editors
                    .get(i)
                    .map(|ed| ed.read(cx).text(cx).to_lowercase())
                    .unwrap_or_default()
            })
            .collect();
        self.recompute_local_filter_inner();
        cx.notify();
    }

    fn toggle_local_filter_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.local_filter_visible = !self.local_filter_visible;
        if self.local_filter_visible {
            let num_cols = self.result.as_ref().map(|r| r.columns.len()).unwrap_or(0);
            if self.local_filter_editors.len() != num_cols {
                self.local_filter_editors = (0..num_cols)
                    .map(|i| {
                        let editor = cx.new(|cx| {
                            let mut ed = Editor::single_line(window, cx);
                            let placeholder = format!("Filter col {}", i + 1);
                            ed.set_placeholder_text(&placeholder, window, cx);
                            ed
                        });
                        cx.subscribe(&editor, |this, _, event: &EditorEvent, cx| {
                            if matches!(event, EditorEvent::BufferEdited) {
                                this.recompute_local_filter(cx);
                            }
                        })
                        .detach();
                        editor
                    })
                    .collect();
            }
        } else {
            self.local_filter_editors.clear();
            self.local_filters.clear();
            self.filtered_display_order = self.order.clone();
        }
        cx.notify();
    }

    fn render_local_filter_row(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        if !self.local_filter_visible || self.result.is_none() {
            return None;
        }

        let grid_border = cx.theme().colors().border.opacity(0.46);
        let filter_bg = cx
            .theme()
            .colors()
            .editor_subheader_background
            .blend(cx.theme().colors().text.opacity(0.018));
        let mut cells: Vec<AnyElement> = Vec::new();

        for (display_pos, &data_col) in self.visible_columns.iter().enumerate() {
            let width = self
                .col_widths
                .get(display_pos)
                .copied()
                .unwrap_or(px(120.));
            let cell: AnyElement = if let Some(editor) = self.local_filter_editors.get(data_col) {
                div()
                    .w(width)
                    .h(px(22.0))
                    .flex_none()
                    .px_1()
                    .py_px()
                    .border_r_1()
                    .border_color(grid_border)
                    .child(editor.clone())
                    .into_any_element()
            } else {
                div()
                    .w(width)
                    .h(px(22.0))
                    .flex_none()
                    .border_r_1()
                    .border_color(grid_border)
                    .into_any_element()
            };
            cells.push(cell);
        }

        Some(
            h_flex()
                .flex_none()
                .overflow_x_hidden()
                .border_b_1()
                .border_color(grid_border)
                .bg(filter_bg)
                .children(cells),
        )
    }


    fn toggle_column_list(&mut self, cx: &mut Context<Self>) {
        self.column_list_visible = !self.column_list_visible;
        cx.notify();
    }

    fn toggle_column_visibility(&mut self, col_idx: usize, cx: &mut Context<Self>) {
        if self.hidden_columns.contains(&col_idx) {
            self.hidden_columns.remove(&col_idx);
        } else {
            self.hidden_columns.insert(col_idx);
        }
        self.recompute_layout();
        cx.notify();
    }

    fn render_column_list_popup(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.column_list_visible {
            return None;
        }
        let columns = self.result.as_ref()?.columns.clone();
        let mut items: Vec<AnyElement> = Vec::new();

        for (col_idx, col_name) in columns.iter().enumerate() {
            let is_visible = !self.hidden_columns.contains(&col_idx);
            let name = col_name.clone();
            items.push(
                div()
                    .px_2()
                    .py_0p5()
                    .child(
                        Checkbox::new(
                            ElementId::from(SharedString::from(format!("col-vis-{col_idx}"))),
                            is_visible.into(),
                        )
                        .label(name)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.toggle_column_visibility(col_idx, cx);
                        })),
                    )
                    .into_any_element(),
            );
        }

        Some(
            popup_surface(cx)
                .id("column-list-popup")
                .absolute()
                .top_8()
                .right_0()
                .min_w(px(160.0))
                .max_h(px(400.0))
                .overflow_y_scroll()
                .children(items)
                .into_any_element(),
        )
    }

    fn render_export_dialog_popup(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.export_dialog_open {
            return None;
        }
        let has_table = self.table_name.is_some();
        let format = self.export_format;

        let format_chips: Vec<AnyElement> = EXPORT_CHOICES
            .iter()
            .copied()
            .filter(|choice| !choice.needs_table() || has_table)
            .map(|choice| {
                let selected = choice == format;
                Button::new(
                    SharedString::from(format!("export-fmt-{}", choice.label())),
                    choice.label(),
                )
                .style(if selected {
                    ButtonStyle::Filled
                } else {
                    ButtonStyle::Subtle
                })
                .label_size(LabelSize::Small)
                .toggle_state(selected)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.export_format = choice;
                    cx.notify();
                }))
                .into_any_element()
            })
            .collect();

        let headers_enabled = format.honors_headers();
        let add_ddl = self.export_add_ddl;
        let headers_on = self.export_headers;
        let transpose_on = self.export_transpose;

        Some(
            popup_surface(cx)
                .id("export-dialog-popup")
                .debug_selector(|| "EXPORT_DIALOG_POPUP".to_string())
                .absolute()
                .top_8()
                .right_0()
                .w(px(320.0))
                .p_3()
                .child(
                    h_flex()
                        .justify_between()
                        .items_center()
                        .mb_2()
                        .child(Label::new("Export Data").size(LabelSize::Default))
                        .child(
                            IconButton::new("export-dialog-close", IconName::Close)
                                .icon_size(IconSize::Small)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.export_dialog_open = false;
                                    cx.notify();
                                })),
                        ),
                )
                .child(
                    Label::new("Format")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .child(h_flex().flex_wrap().gap_1().mb_2().children(format_chips))
                .child(
                    v_flex()
                        .gap_0p5()
                        .mb_2()
                        .child(
                            Checkbox::new("export-opt-ddl", add_ddl.into())
                                .label("Add DDL (CREATE TABLE)")
                                .disabled(!has_table)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.export_add_ddl = !this.export_add_ddl;
                                    if this.export_add_ddl {
                                        this.fetch_export_ddl(cx);
                                    }
                                    cx.notify();
                                })),
                        )
                        .child(
                            Checkbox::new("export-opt-headers", headers_on.into())
                                .label("Column headers")
                                .disabled(!headers_enabled)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.export_headers = !this.export_headers;
                                    cx.notify();
                                })),
                        )
                        .child(
                            Checkbox::new("export-opt-transpose", transpose_on.into())
                                .label("Transpose")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.export_transpose = !this.export_transpose;
                                    cx.notify();
                                })),
                        ),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("export-clipboard", "Copy to Clipboard")
                                .style(ButtonStyle::Filled)
                                .label_size(LabelSize::Small)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.export_to_clipboard(cx);
                                })),
                        )
                        .child(
                            Button::new("export-file", "Save to File…")
                                .style(ButtonStyle::Subtle)
                                .label_size(LabelSize::Small)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.export_to_file(cx);
                                })),
                        ),
                )
                .into_any_element(),
        )
    }

    fn render_chart_popup(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.chart_open {
            return None;
        }
        let result = self.result.as_ref()?;
        let value_column = self
            .chart_value_column
            .or_else(|| Self::first_numeric_column(result));
        let kind = self.chart_kind;

        let header = h_flex()
            .justify_between()
            .items_center()
            .mb_2()
            .child(Label::new("Chart").size(LabelSize::Default))
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new("chart-kind-bar", "Bar")
                            .style(if kind == ChartKind::Bar {
                                ButtonStyle::Filled
                            } else {
                                ButtonStyle::Subtle
                            })
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.chart_kind = ChartKind::Bar;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("chart-kind-line", "Line")
                            .style(if kind == ChartKind::Line {
                                ButtonStyle::Filled
                            } else {
                                ButtonStyle::Subtle
                            })
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.chart_kind = ChartKind::Line;
                                cx.notify();
                            })),
                    )
                    .child(
                        IconButton::new("chart-close", IconName::Close)
                            .icon_size(IconSize::Small)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.chart_open = false;
                                cx.notify();
                            })),
                    ),
            );

        let geo = Self::detect_lat_lon(result);
        let body = if let Some(value_column) = value_column {
            let series = Self::chart_series(result, self.chart_label_column, value_column);
            self.render_chart_body(&series, kind, cx)
        } else if let Some((lat, lon)) = geo {
            self.render_geo_body(result, lat, lon, cx)
        } else {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    Label::new("No numeric column to chart")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element()
        };

        Some(
            popup_surface(cx)
                .id("chart-popup")
                .debug_selector(|| "CHART_POPUP".to_string())
                .absolute()
                .top_8()
                .left_0()
                .right_0()
                .bottom_8()
                .p_3()
                .flex()
                .flex_col()
                .child(header)
                .child(body)
                .into_any_element(),
        )
    }

    fn render_chart_body(
        &self,
        series: &[(String, f64)],
        kind: ChartKind,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some((min, max)) = Self::series_bounds(series) else {
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    Label::new("No numeric values to chart")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element();
        };
        let span = max - min;
        let bar_color = cx.theme().status().info;
        let grid_color = cx.theme().colors().border;

        match kind {
            ChartKind::Bar => {
                let bars: Vec<AnyElement> = series
                    .iter()
                    .map(|(label, value)| {
                        let fraction = ((value - min) / span).clamp(0.0, 1.0) as f32;
                        v_flex()
                            .flex_1()
                            .min_w(px(6.0))
                            .h_full()
                            .justify_end()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .w(px(14.0))
                                    .h(relative(fraction.max(0.01)))
                                    .rounded_t_sm()
                                    .bg(bar_color),
                            )
                            .child(
                                Label::new(SharedString::from(label.clone()))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .into_any_element()
                    })
                    .collect();
                h_flex()
                    .flex_1()
                    .w_full()
                    .items_end()
                    .gap_1()
                    .border_b_1()
                    .border_color(grid_color)
                    .children(bars)
                    .into_any_element()
            }
            ChartKind::Line => {
                let points: Vec<(f32, f32)> = series
                    .iter()
                    .enumerate()
                    .map(|(index, (_, value))| {
                        let x = if series.len() <= 1 {
                            0.0
                        } else {
                            index as f32 / (series.len() - 1) as f32
                        };
                        let y = ((value - min) / span).clamp(0.0, 1.0) as f32;
                        (x, y)
                    })
                    .collect();
                let line_color = bar_color;
                div()
                    .flex_1()
                    .w_full()
                    .border_b_1()
                    .border_color(grid_color)
                    .child(
                        gpui::canvas(
                            move |_, _, _| {},
                            move |bounds, _, window, _| {
                                if points.len() < 2 {
                                    return;
                                }
                                let origin = bounds.origin;
                                let width = bounds.size.width;
                                let height = bounds.size.height;
                                let mut builder = gpui::PathBuilder::stroke(px(1.5));
                                for (index, (x, y)) in points.iter().enumerate() {
                                    let point = gpui::point(
                                        origin.x + width * *x,
                                        origin.y + height * (1.0 - *y),
                                    );
                                    if index == 0 {
                                        builder.move_to(point);
                                    } else {
                                        builder.line_to(point);
                                    }
                                }
                                if let Ok(path) = builder.build() {
                                    window.paint_path(path, line_color);
                                }
                            },
                        )
                        .size_full(),
                    )
                    .into_any_element()
            }
        }
    }

    fn render_geo_body(
        &self,
        result: &QueryResult,
        lat: usize,
        lon: usize,
        _cx: &mut Context<Self>,
    ) -> AnyElement {
        let points: Vec<(String, String)> = result
            .rows
            .iter()
            .filter_map(|row| {
                let lat_value = row.get(lat)?.clone()?;
                let lon_value = row.get(lon)?.clone()?;
                Some((lat_value, lon_value))
            })
            .collect();
        let rows: Vec<AnyElement> = points
            .iter()
            .take(500)
            .map(|(lat_value, lon_value)| {
                Label::new(SharedString::from(format!("{lat_value}, {lon_value}")))
                    .size(LabelSize::Small)
                    .into_any_element()
            })
            .collect();
        v_flex()
            .id("chart-geo-list")
            .flex_1()
            .gap_1()
            .overflow_y_scroll()
            .child(
                Label::new(SharedString::from(format!("{} geo points", points.len())))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .children(rows)
            .into_any_element()
    }

    // Approximate row/header heights for enum popup positioning (px).
    // These match the py_1 + LabelSize::Small layout used throughout the grid.
    const GRID_HEADER_H: f32 = 28.0;
    const GRID_ROW_H: f32 = 26.0;

    fn render_enum_popup(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let popup = self.enum_popup.as_ref()?;
        let (_, _, _, scroll_y) = axis_metrics(&self.scroll_handle, true);

        // Column left edge in content coordinates.
        let col_x = self
            .column_edges
            .get(popup.col_idx.saturating_sub(1))
            .copied()
            .unwrap_or(0.0);
        // Map to screen coordinates by adding the (negative) h-scroll offset.
        let screen_x = (col_x + f32::from(self.h_scroll.offset().x)).max(0.0);
        // Approximate Y: below the header, then one row per abs_idx, shifted by scroll.
        let screen_y =
            (Self::GRID_HEADER_H + popup.abs_idx as f32 * Self::GRID_ROW_H + scroll_y).max(0.0);

        let values = popup.values.clone();
        let nullable = popup.nullable;
        let popup_abs = popup.abs_idx;
        let popup_col = popup.col_idx;
        let popup_added = popup.added_idx;

        let mut items: Vec<AnyElement> = Vec::new();

        if nullable {
            items.push(
                div()
                    .id("enum-null")
                    .px_2()
                    .py_1()
                    .cursor_pointer()
                    .hover(|el| el.bg(cx.theme().colors().element_hover))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.apply_enum_selection(CellValue::Null, cx);
                    }))
                    .child(
                        Label::new(NULL_MARKER)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .into_any_element(),
            );
        }

        for value in values {
            let display = value.clone();
            let item_id = SharedString::from(format!("enum-val-{}", display));
            items.push(
                div()
                    .id(item_id)
                    .px_2()
                    .py_1()
                    .cursor_pointer()
                    .hover(|el| el.bg(cx.theme().colors().element_hover))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.apply_enum_selection(CellValue::Text(value.clone()), cx);
                    }))
                    .child(Label::new(display).size(LabelSize::Small))
                    .into_any_element(),
            );
        }

        // An invisible full-area backdrop closes the popup on any outside click.
        let backdrop = div().absolute().inset_0().on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _, _, cx| {
                // Dismiss without selecting, unless the click landed on the
                // popup itself (the popup has a higher z-index so it captures
                // its own mouse-down before this backdrop does).
                // Suppress unused-variable warnings:
                let _ = (popup_abs, popup_col, popup_added);
                this.enum_popup = None;
                cx.notify();
            }),
        );

        Some(
            div()
                .absolute()
                .left(gpui::px(screen_x))
                .top(gpui::px(screen_y))
                .child(backdrop)
                .child(
                    div()
                        .absolute()
                        .left(gpui::px(0.0))
                        .top(gpui::px(0.0))
                        .elevation_2(cx)
                        .occlude()
                        .min_w(gpui::px(120.0))
                        .children(items),
                )
                .into_any_element(),
        )
    }
}

// Width/height of the scrollbar gutters.
const SCROLLBAR_SIZE: f32 = 18.0;
const SCROLLBAR_THUMB_MIN: f32 = 36.0;
const SCROLLBAR_THUMB_INSET: f32 = 3.0;

// Maps a window coordinate on a scrollbar gutter to a scroll offset and applies
// it (jump-to-position), used when the empty track is clicked outside the thumb.
fn scroll_axis_to(handle: &impl ui::ScrollableHandle, vertical: bool, window_pos: f32) {
    let viewport = handle.viewport();
    let content = handle.content_size();
    let (origin, viewport_len, content_len) = if vertical {
        (
            f32::from(viewport.origin.y),
            f32::from(viewport.size.height),
            f32::from(content.height),
        )
    } else {
        (
            f32::from(viewport.origin.x),
            f32::from(viewport.size.width),
            f32::from(content.width),
        )
    };
    let Some(new_offset) = gutter_scroll_offset(window_pos, origin, viewport_len, content_len)
    else {
        return;
    };
    let current = handle.offset();
    if vertical {
        handle.set_offset(gpui::point(current.x, px(new_offset)));
    } else {
        handle.set_offset(gpui::point(px(new_offset), current.y));
    }
}

// Computes the (negative) scroll offset for a window coordinate on a gutter.
// Returns `None` when the content fits the viewport (nothing to scroll). The
// fraction is clamped, so positions outside the gutter map to the nearest end.
fn gutter_scroll_offset(
    window_pos: f32,
    origin: f32,
    viewport_len: f32,
    content_len: f32,
) -> Option<f32> {
    let max_offset = content_len - viewport_len;
    if viewport_len <= 0.0 || max_offset <= 0.0 {
        return None;
    }
    let fraction = ((window_pos - origin) / viewport_len).clamp(0.0, 1.0);
    Some(-(fraction * max_offset).clamp(0.0, max_offset))
}

// Reads the origin, length, content length and current offset of a scroll
// handle on one axis, in pixels. The gutter spans the viewport, so its length
// equals the viewport length.
fn axis_metrics(handle: &impl ui::ScrollableHandle, vertical: bool) -> (f32, f32, f32, f32) {
    let viewport = handle.viewport();
    let content = handle.content_size();
    let offset = handle.offset();
    if vertical {
        (
            f32::from(viewport.origin.y),
            f32::from(viewport.size.height),
            f32::from(content.height),
            f32::from(offset.y),
        )
    } else {
        (
            f32::from(viewport.origin.x),
            f32::from(viewport.size.width),
            f32::from(content.width),
            f32::from(offset.x),
        )
    }
}

// Window-coordinate range [start, end] the thumb occupies on the gutter. Mirrors
// the sizing in `scroll_thumb`, so hit-testing matches what is drawn.
fn thumb_range(origin: f32, viewport_len: f32, content_len: f32, offset: f32) -> (f32, f32) {
    if viewport_len <= 0.0 || content_len <= viewport_len {
        return (origin, origin + viewport_len);
    }
    let max_offset = content_len - viewport_len;
    let thumb_len =
        ((viewport_len * viewport_len) / content_len).clamp(SCROLLBAR_THUMB_MIN, viewport_len);
    let travel = (viewport_len - thumb_len).max(0.0);
    let pos_frac = (-offset / max_offset).clamp(0.0, 1.0);
    let start = origin + pos_frac * travel;
    (start, start + thumb_len)
}

// New scroll offset for a relative drag: the content moves in proportion to how
// far the cursor traveled from the grab point. A pixel of thumb travel maps to
// `content/viewport` pixels of content. Returns `None` when nothing scrolls.
fn drag_scroll_offset(
    grab_offset: f32,
    grab_pos: f32,
    current_pos: f32,
    viewport_len: f32,
    content_len: f32,
) -> Option<f32> {
    let max_offset = content_len - viewport_len;
    if viewport_len <= 0.0 || max_offset <= 0.0 {
        return None;
    }
    let delta = current_pos - grab_pos;
    let thumb_len =
        ((viewport_len * viewport_len) / content_len).clamp(SCROLLBAR_THUMB_MIN, viewport_len);
    let travel = (viewport_len - thumb_len).max(1.0);
    let new_offset = grab_offset - delta * (max_offset / travel);
    Some(new_offset.clamp(-max_offset, 0.0))
}

// Builds a scrollbar thumb sized and positioned (as a fraction of its gutter)
// from a scroll handle's viewport/content ratio. The thumb fills the gutter when
// the content fits, so the control is always visible.
fn scroll_thumb(
    handle: &impl ui::ScrollableHandle,
    vertical: bool,
    color: gpui::Hsla,
) -> gpui::Stateful<gpui::Div> {
    let viewport = handle.viewport().size;
    let content = handle.content_size();
    let (viewport_len, content_len, offset) = if vertical {
        (
            f32::from(viewport.height),
            f32::from(content.height),
            f32::from(handle.offset().y),
        )
    } else {
        (
            f32::from(viewport.width),
            f32::from(content.width),
            f32::from(handle.offset().x),
        )
    };
    let (start, end) = thumb_range(0.0, viewport_len, content_len, offset);
    let size_frac = if viewport_len > 0.0 {
        ((end - start) / viewport_len).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let pos_frac = if viewport_len > 0.0 {
        (start / viewport_len).clamp(0.0, 1.0)
    } else {
        0.0
    };

    let thumb = div()
        .id(if vertical {
            "v-scrollbar"
        } else {
            "h-scrollbar"
        })
        .absolute()
        .bg(color)
        .rounded_sm();
    if vertical {
        thumb
            .top(gpui::relative(pos_frac))
            .left(px(SCROLLBAR_THUMB_INSET))
            .w(px(SCROLLBAR_SIZE - SCROLLBAR_THUMB_INSET * 2.0))
            .h(gpui::relative(size_frac))
    } else {
        thumb
            .left(gpui::relative(pos_frac))
            .top(px(SCROLLBAR_THUMB_INSET))
            .h(px(SCROLLBAR_SIZE - SCROLLBAR_THUMB_INSET * 2.0))
            .w(gpui::relative(size_frac))
    }
}

// Quotes an identifier (table/column) with the driver's quote char, doubling any
// embedded quote char so a crafted name cannot break out of the identifier.
fn quote_identifier(quote: char, name: &str) -> String {
    let escaped = name.replace(quote, &format!("{quote}{quote}"));
    format!("{quote}{escaped}{quote}")
}

// Appends `text` to `out` with HTML special-char escaping (<, >, &, ", ').
fn html_escape_into(out: &mut String, text: &str) {
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
}

// Renders a cell value as a SQL literal. NULL stays unquoted; numeric-looking
// values are emitted bare so numeric columns are not compared as strings;
// everything else is a single-quoted string with quotes doubled.
fn sql_literal(value: Option<&str>) -> String {
    match value {
        None => "NULL".to_string(),
        Some(text) => {
            if is_sql_number(text) {
                text.to_string()
            } else {
                format!("'{}'", text.replace('\'', "''"))
            }
        }
    }
}

// Renders a buffered cell value as a SQL literal: `Null` -> NULL, `Default` ->
// the bare DEFAULT keyword (never quoted, so the database applies the column
// default), `Text` -> numeric-bare or quoted string exactly like `sql_literal`.
fn sql_value_literal(value: &CellValue) -> String {
    match value {
        CellValue::Null => "NULL".to_string(),
        CellValue::Default => "DEFAULT".to_string(),
        CellValue::Text(text) => sql_literal(Some(text)),
    }
}

// True when `text` is a plain decimal number (optionally signed, with a single
// fractional part), so it can be emitted as a bare SQL numeric literal. Anything
// else is treated as a string to stay safe.
fn is_sql_number(text: &str) -> bool {
    let body = text.strip_prefix(['+', '-']).unwrap_or(text);
    if body.is_empty() {
        return false;
    }
    let mut seen_dot = false;
    let mut seen_digit = false;
    for ch in body.chars() {
        match ch {
            '0'..='9' => seen_digit = true,
            '.' if !seen_dot => seen_dot = true,
            _ => return false,
        }
    }
    seen_digit
}

// Builds an `UPDATE table SET col = value WHERE <key predicate>` statement. Pure
// and side-effect free so it can be unit tested. The caller must ensure the key
// predicate uniquely identifies the row and that no key value was truncated.
fn build_update_sql(
    quote: char,
    table: &str,
    set_column: &str,
    new_value: &CellValue,
    key_columns: &[(String, Option<String>)],
) -> String {
    let predicate: Vec<String> = key_columns
        .iter()
        .map(|(col, value)| match value {
            Some(_) => format!(
                "{} = {}",
                quote_identifier(quote, col),
                sql_literal(value.as_deref())
            ),
            None => format!("{} IS NULL", quote_identifier(quote, col)),
        })
        .collect();
    format!(
        "-- name: UpdateResultCell :exec\nUPDATE {table} SET {col} = {value} WHERE {predicate}",
        table = quote_identifier(quote, table),
        col = quote_identifier(quote, set_column),
        value = sql_value_literal(new_value),
        predicate = predicate.join(" AND "),
    )
}

// Builds the (column, value) pairs of a PK-based WHERE predicate for one row.
// Pure and testable. Returns Err with a human note when the row cannot be safely
// targeted: a missing key column or a truncated key value (which would match the
// wrong row). The caller must already have rejected an empty primary key.
fn build_key_predicate(
    columns: &[String],
    primary_key_columns: &[String],
    row: &[Option<String>],
) -> Result<Vec<(String, Option<String>)>, String> {
    let mut key_predicate: Vec<(String, Option<String>)> = Vec::new();
    for key in primary_key_columns {
        let Some(value_idx) = columns.iter().position(|col| col == key) else {
            return Err(
                "Edit kept in grid: key column is not in the result; not written.".to_string(),
            );
        };
        let value = row.get(value_idx).and_then(|cell| cell.clone());
        if let Some(text) = value.as_deref()
            && db_client::is_cell_possibly_truncated(text)
        {
            return Err(
                "Edit kept in grid: key value was truncated, so it cannot safely target the row."
                    .to_string(),
            );
        }
        key_predicate.push((key.clone(), value));
    }
    Ok(key_predicate)
}

// Turns the pending-edit buffer into one UPDATE per changed cell. Pure and
// testable. Returns Err with a human note for the first cell that cannot be
// safely targeted (no primary key, a missing key column, or a truncated key
// value), so the caller can surface it and keep the buffer intact. `edits` are
// `((absolute row, column), new value)` and should be pre-sorted for a stable
// statement order.
fn build_pending_updates(
    quote: char,
    table: &str,
    columns: &[String],
    primary_key_columns: &[String],
    rows: &[Vec<Option<String>>],
    edits: &[((usize, usize), CellValue)],
) -> Result<Vec<String>, String> {
    if primary_key_columns.is_empty() {
        return Err(
            "Edit kept in grid: this table has no primary key to target a single row.".to_string(),
        );
    }
    let mut statements = Vec::with_capacity(edits.len());
    for ((abs_idx, col_idx), new_value) in edits {
        let Some(set_column) = columns.get(*col_idx) else {
            return Err("Edit kept in grid: column is not in the result; not written.".to_string());
        };
        let Some(row) = rows.get(*abs_idx) else {
            return Err("Edit kept in grid: row is no longer in the result.".to_string());
        };
        let key_predicate = build_key_predicate(columns, primary_key_columns, row)?;
        statements.push(build_update_sql(
            quote,
            table,
            set_column,
            new_value,
            &key_predicate,
        ));
    }
    Ok(statements)
}

// Builds a `DELETE FROM table WHERE <key predicate>` statement. Pure and side
// effect free. The caller must ensure the key predicate uniquely identifies the
// row and that no key value was truncated.
fn build_delete_sql(quote: char, table: &str, key_columns: &[(String, Option<String>)]) -> String {
    let predicate: Vec<String> = key_columns
        .iter()
        .map(|(col, value)| match value {
            Some(_) => format!(
                "{} = {}",
                quote_identifier(quote, col),
                sql_literal(value.as_deref())
            ),
            None => format!("{} IS NULL", quote_identifier(quote, col)),
        })
        .collect();
    format!(
        "-- name: DeleteResultRow :exec\nDELETE FROM {table} WHERE {predicate}",
        table = quote_identifier(quote, table),
        predicate = predicate.join(" AND "),
    )
}

// Turns the deleted-row set into one DELETE per row. Pure and testable. Returns
// Err for the first row that cannot be safely targeted, mirroring
// `build_pending_updates`. `deleted` are absolute row indices, pre-sorted for a
// stable statement order.
fn build_pending_deletes(
    quote: char,
    table: &str,
    columns: &[String],
    primary_key_columns: &[String],
    rows: &[Vec<Option<String>>],
    deleted: &[usize],
) -> Result<Vec<String>, String> {
    if deleted.is_empty() {
        return Ok(Vec::new());
    }
    if primary_key_columns.is_empty() {
        return Err(
            "Edit kept in grid: this table has no primary key to target a single row.".to_string(),
        );
    }
    let mut statements = Vec::with_capacity(deleted.len());
    for abs_idx in deleted {
        let Some(row) = rows.get(*abs_idx) else {
            return Err("Edit kept in grid: row is no longer in the result.".to_string());
        };
        let key_predicate = build_key_predicate(columns, primary_key_columns, row)?;
        statements.push(build_delete_sql(quote, table, &key_predicate));
    }
    Ok(statements)
}

// Builds an `INSERT INTO table (cols) VALUES (literals)` statement. Pure and
// side effect free. INSERT needs no key, so any added row can be inserted.
fn build_insert_sql(quote: char, table: &str, columns: &[String], values: &[CellValue]) -> String {
    let column_list: Vec<String> = columns
        .iter()
        .map(|col| quote_identifier(quote, col))
        .collect();
    let value_list: Vec<String> = columns
        .iter()
        .enumerate()
        .map(|(idx, _)| sql_value_literal(values.get(idx).unwrap_or(&CellValue::Null)))
        .collect();
    format!(
        "-- name: InsertResultRow :exec\nINSERT INTO {table} ({cols}) VALUES ({values})",
        table = quote_identifier(quote, table),
        cols = column_list.join(", "),
        values = value_list.join(", "),
    )
}

// Concatenates the row statements in the safe execution order: DELETE first, so
// a re-inserted unique key cannot collide with a row about to be removed; then
// UPDATE on the surviving loaded rows; then INSERT of the new rows last. Pure so
// the shipped ordering is the tested ordering.
fn combine_row_statements(
    mut deletes: Vec<String>,
    updates: Vec<String>,
    inserts: Vec<String>,
) -> Vec<String> {
    deletes.extend(updates);
    deletes.extend(inserts);
    deletes
}

// Joins staged statements into one BEGIN/COMMIT block so Manual-mode Commit
// applies them atomically. Each statement keeps its `-- name:` marker; a
// trailing semicolon separates them.
fn wrap_in_transaction(statements: &[String]) -> String {
    let mut out = String::from("BEGIN;\n");
    for statement in statements {
        out.push_str(statement);
        out.push_str(";\n");
    }
    out.push_str("COMMIT;");
    out
}

impl EventEmitter<ResultViewEvent> for ResultView {}

impl Item for ResultView {
    type Event = ResultViewEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.title.clone()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<ui::Icon> {
        let icon = if self.pinned {
            IconName::Pin
        } else {
            IconName::DatabaseZap
        };
        Some(Icon::new(icon))
    }
}

impl Focusable for ResultView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ResultView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.tick_fps();
        // One-time lazy init of the limit editor (requires window, so can't live in new()).
        if self.limit_editor.is_none() {
            let initial = if self.fetch_target == usize::MAX {
                "All".to_string()
            } else {
                self.fetch_target.to_string()
            };
            let ed = cx.new(|cx| {
                let mut e = Editor::single_line(window, cx);
                e.set_placeholder_text("rows", window, cx);
                e.set_text(initial, window, cx);
                e
            });
            cx.subscribe(&ed, |this, _, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::Blurred) {
                    this.apply_custom_limit(false, cx);
                }
            })
            .detach();
            self.limit_editor = Some(ed);
        }
        // Sync limit editor text with fetch_target when the editor is not actively focused.
        if let Some(ed) = self.limit_editor.clone() {
            if !ed.read(cx).is_focused(window) {
                let expected = Self::limit_display_text(self.fetch_target);
                if ed.read(cx).text(cx).trim() != expected.as_str() {
                    ed.update(cx, |e, cx| e.set_text(expected, window, cx));
                }
            }
        }
        self.sync_value_editor(window, cx);
        self.sync_special_view(window, cx);
        let filter_bar = self.render_filter_bar(cx);
        let content = if self.is_loading {
            div()
                .flex()
                .size_full()
                .items_center()
                .justify_center()
                .child(loading_spinner(
                    "loading-spinner",
                    IconSize::Custom(ui::rems_from_px(28.)),
                ))
                .into_any_element()
        } else if let Some(error) = self.error.clone() {
            self.render_error(&error).into_any_element()
        } else if self.result.is_some() {
            if self.active_special() != SpecialResult::None {
                self.render_special_view(cx).into_any_element()
            } else {
                // Borrow the result (do NOT clone it): cloning the whole result
                // set on every scroll frame is a large per-frame cost on big
                // results.
                self.render_result(cx).into_any_element()
            }
        } else {
            self.render_empty_state().into_any_element()
        };

        v_flex()
            .key_context("DbResultView")
            .track_focus(&self.focus_handle)
            .size_full()
            .when_some(self.env_accent, |el, color| {
                // Ambient environment cue: a thin top stripe plus a faint wash so
                // a production connection is recognizable at a glance without
                // hurting cell legibility.
                el.bg(color.opacity(0.05)).child(
                    div()
                        .id("env-accent-bar")
                        .debug_selector(|| "ENV_ACCENT_BAR".to_string())
                        .h(px(2.))
                        .w_full()
                        .flex_none()
                        .bg(color),
                )
            })
            .on_action(cx.listener(|this, _: &SubmitEdits, window, cx| {
                this.submit_pending_edits(window, cx);
            }))
            .on_action(cx.listener(|this, _: &RevertEdits, _window, cx| {
                this.revert_pending_edits(cx);
            }))
            .on_action(cx.listener(|this, _: &AddRow, _window, cx| {
                this.add_blank_row(cx);
            }))
            .on_action(cx.listener(|this, _: &DeleteRow, _window, cx| {
                this.toggle_delete_selected_row(cx);
            }))
            .on_action(cx.listener(|this, _: &CloneRow, _window, cx| {
                this.clone_selected_row(cx);
            }))
            .on_action(cx.listener(|this, _: &SetNull, _window, cx| {
                this.set_selected_cell_value(CellValue::Null, cx);
            }))
            .on_action(cx.listener(|this, _: &SetDefault, _window, cx| {
                this.set_selected_cell_value(CellValue::Default, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleValueEditor, _window, cx| {
                this.value_editor_open = !this.value_editor_open;
                if !this.value_editor_open {
                    this.value_editor_resize_drag = None;
                }
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleFind, window, cx| {
                this.toggle_find(window, cx);
            }))
            .on_action(cx.listener(|this, _: &FindNext, _window, cx| {
                this.find_next(cx);
            }))
            .on_action(cx.listener(|this, _: &FindPrevious, _window, cx| {
                this.find_previous(cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleLocalFilters, window, cx| {
                this.toggle_local_filter_row(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleColumnList, _window, cx| {
                this.toggle_column_list(cx);
            }))
            .on_action(cx.listener(|this, _: &OpenQueryHistory, window, cx| {
                this.toggle_query_history(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleRecordView, _window, cx| {
                this.record_view_open = !this.record_view_open;
                if this.record_view_open && this.record_view_row.is_none() {
                    this.record_view_row = Some(0);
                }
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &RecordViewPrev, _window, cx| {
                this.record_view_step(-1, cx);
            }))
            .on_action(cx.listener(|this, _: &RecordViewNext, _window, cx| {
                this.record_view_step(1, cx);
            }))
            .on_action(cx.listener(|this, _: &QuickDoc, _window, cx| {
                this.quick_doc_open = !this.quick_doc_open;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &NavigateToFkRow, window, cx| {
                this.navigate_to_fk_row(window, cx);
            }))
            .on_action(cx.listener(|this, _: &CopySelection, _window, cx| {
                this.copy_selected_to_clipboard(cx);
            }))
            .on_action(cx.listener(|this, _: &PreviewPendingChanges, _window, cx| {
                this.preview_open = !this.preview_open;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleTransactionMode, _window, cx| {
                this.toggle_transaction_mode(cx);
            }))
            .on_action(cx.listener(|this, _: &CommitTransaction, window, cx| {
                this.commit_transaction(window, cx);
            }))
            .on_action(cx.listener(|this, _: &RollbackTransaction, _window, cx| {
                this.rollback_transaction(cx);
            }))
            .on_action(cx.listener(|this, _: &GoToRow, window, cx| {
                this.toggle_goto_row(window, cx);
            }))
            .on_action(cx.listener(|this, _: &CopyAggregation, _window, cx| {
                this.copy_aggregation_to_clipboard(cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleFindFilterRows, _window, cx| {
                this.toggle_find_filter_rows(cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleTranspose, _window, cx| {
                this.toggle_transpose(cx);
            }))
            .on_action(cx.listener(|this, _: &ResetView, window, cx| {
                this.reset_view(cx);
                if this.table_name.is_some() {
                    this.refresh_table_data(window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &OpenExportDialog, _window, cx| {
                this.open_export_dialog(cx);
            }))
            .on_action(cx.listener(|this, _: &TogglePinResult, _window, cx| {
                this.toggle_pinned(cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleChart, _window, cx| {
                this.toggle_chart(cx);
            }))
            .when_some(filter_bar, |el, bar| el.child(bar))
            .child(div().flex_1().overflow_hidden().child(content))
            .when_some(self.render_record_view_panel(cx), |el, panel| {
                el.child(panel)
            })
            .when_some(self.render_quick_doc_panel(cx), |el, panel| el.child(panel))
            .when_some(self.render_pending_preview_popup(cx), |el, popup| {
                el.child(popup)
            })
            .when_some(self.render_goto_row_bar(cx), |el, bar| el.child(bar))
            .when_some(self.render_find_bar(cx), |el, bar| el.child(bar))
            .when_some(self.render_export_dialog_popup(cx), |el, popup| {
                el.child(popup)
            })
            .when_some(self.render_chart_popup(cx), |el, popup| el.child(popup))
            .when_some(self.render_status_bar(cx), |el, bar| el.child(bar))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_cell_bounds_huge_values_and_preserves_formatting() {
        // Short values pass through unchanged.
        assert_eq!(display_cell("hello"), "hello");

        assert_eq!(display_cell("a\nb\tc\rd"), "a\nb\tc\nd");
        assert_eq!(display_cell("a\r\nb"), "a\nb");
        assert!(cell_value_needs_expanded_editor("a\nb"));
        assert!(cell_value_needs_expanded_editor("a\tb"));

        // A value longer than the cap is truncated and gets an ellipsis. The
        // result must never exceed the cap (+1 for the ellipsis), no matter how
        // large the input — this is what prevents the giant-label freeze.
        let huge = "x".repeat(5_000_000);
        let shown = display_cell(&huge);
        assert_eq!(shown.chars().count(), MAX_CELL_DISPLAY_CHARS + 1);
        assert!(shown.ends_with('…'));
        assert!(cell_value_needs_expanded_editor(&huge));

        // A value exactly at the cap is not marked as truncated.
        let exact = "y".repeat(MAX_CELL_DISPLAY_CHARS);
        assert_eq!(display_cell(&exact), exact);
        assert!(!cell_value_needs_expanded_editor(&exact));
    }

    #[test]
    fn column_aggregates_do_not_embed_text_values() {
        let result = QueryResult {
            columns: vec!["Create Table".to_string()],
            rows: vec![vec![Some(
                "CREATE TABLE `splits` (\n  `split_id` int NOT NULL\n)".to_string(),
            )]],
            rows_affected: 0,
            execution_time_ms: 0,
        };

        assert_eq!(
            ResultView::compute_column_aggregates(&result, 0, &[0]),
            "COUNT 1 | NULLS 0"
        );
    }

    #[gpui::test]
    fn set_query_result_resets_row_limit_to_default(cx: &mut gpui::TestAppContext) {
        init_result_view_test(cx);
        let store = cx.update(|cx| cx.new(DatabaseStore::new));
        let connection_id = uuid::Uuid::new_v4();
        let result = sample_table_result();
        let window = cx.add_window(|_window, cx| {
            let mut view = ResultView::new("test", cx);
            view.fetch_target = 599;
            view.set_query_result(
                store.downgrade(),
                connection_id,
                "public".to_string(),
                "select * from users".to_string(),
                result,
                cx,
            );
            view
        });
        window
            .update(cx, |view, _window, _cx| {
                assert_eq!(view.fetch_target, DEFAULT_FETCH_TARGET);
            })
            .unwrap();
    }

    #[test]
    fn sql_literal_quotes_strings_and_passes_numbers_and_null() {
        // NULL is emitted bare, never quoted.
        assert_eq!(sql_literal(None), "NULL");

        // Plain integers and decimals are bare numeric literals.
        assert_eq!(sql_literal(Some("42")), "42");
        assert_eq!(sql_literal(Some("-3.14")), "-3.14");

        // Anything non-numeric is a single-quoted string with quotes doubled, so
        // a value containing a quote cannot break out of the literal.
        assert_eq!(sql_literal(Some("hello")), "'hello'");
        assert_eq!(sql_literal(Some("O'Brien")), "'O''Brien'");
        assert_eq!(sql_literal(Some("a' OR '1'='1")), "'a'' OR ''1''=''1'");

        // Leading-zero or version-like values are treated as strings, not numbers.
        assert_eq!(sql_literal(Some("1.2.3")), "'1.2.3'");
        assert_eq!(sql_literal(Some("")), "''");
    }

    #[test]
    fn quote_identifier_doubles_embedded_quote_char() {
        assert_eq!(quote_identifier('`', "users"), "`users`");
        assert_eq!(quote_identifier('"', "users"), "\"users\"");
        // A backtick inside a MySQL identifier is doubled.
        assert_eq!(quote_identifier('`', "we`ird"), "`we``ird`");
    }

    #[test]
    fn build_update_sql_uses_key_predicate_and_handles_null() {
        let sql = build_update_sql(
            '`',
            "users",
            "name",
            &CellValue::Text("O'Brien".to_string()),
            &[("id".to_string(), Some("5".to_string()))],
        );
        assert_eq!(
            sql,
            "-- name: UpdateResultCell :exec\nUPDATE `users` SET `name` = 'O''Brien' WHERE `id` = 5"
        );

        // Setting a cell to NULL and a composite key, one part of which is NULL.
        let sql = build_update_sql(
            '"',
            "t",
            "val",
            &CellValue::Null,
            &[
                ("a".to_string(), Some("1".to_string())),
                ("b".to_string(), None),
            ],
        );
        assert_eq!(
            sql,
            "-- name: UpdateResultCell :exec\nUPDATE \"t\" SET \"val\" = NULL WHERE \"a\" = 1 AND \"b\" IS NULL"
        );
    }

    #[test]
    fn build_pending_updates_emits_one_update_per_cell_in_order() {
        let columns = vec!["id".to_string(), "name".to_string()];
        let rows = vec![
            vec![Some("1".to_string()), Some("a".to_string())],
            vec![Some("2".to_string()), Some("b".to_string())],
        ];
        let primary_key_columns = vec!["id".to_string()];
        // Deliberately out of order; the caller sorts, but verify the output
        // order follows the input so the function itself is deterministic.
        let edits = vec![
            ((0usize, 1usize), CellValue::Text("alice".to_string())),
            ((1usize, 1usize), CellValue::Null),
        ];

        let statements =
            build_pending_updates('`', "users", &columns, &primary_key_columns, &rows, &edits)
                .expect("statements build for a keyed table");

        assert_eq!(
            statements,
            vec![
                build_update_sql(
                    '`',
                    "users",
                    "name",
                    &CellValue::Text("alice".to_string()),
                    &[("id".to_string(), Some("1".to_string()))]
                ),
                build_update_sql(
                    '`',
                    "users",
                    "name",
                    &CellValue::Null,
                    &[("id".to_string(), Some("2".to_string()))]
                ),
            ]
        );
    }

    #[test]
    fn build_pending_updates_rejects_empty_primary_key() {
        let columns = vec!["id".to_string(), "name".to_string()];
        let rows = vec![vec![Some("1".to_string()), Some("a".to_string())]];
        let edits = vec![((0usize, 1usize), CellValue::Text("x".to_string()))];

        let err = build_pending_updates('`', "users", &columns, &[], &rows, &edits)
            .expect_err("no primary key must be rejected");
        assert!(err.contains("no primary key"));
    }

    #[test]
    fn build_pending_updates_rejects_missing_key_column() {
        let columns = vec!["name".to_string()];
        let rows = vec![vec![Some("a".to_string())]];
        let primary_key_columns = vec!["id".to_string()];
        let edits = vec![((0usize, 0usize), CellValue::Text("x".to_string()))];

        let err =
            build_pending_updates('`', "users", &columns, &primary_key_columns, &rows, &edits)
                .expect_err("a key column missing from the result must be rejected");
        assert!(err.contains("key column is not in the result"));
    }

    #[test]
    fn build_pending_updates_rejects_truncated_key() {
        // A truncated key value (ends with the ellipsis marker and is at least the
        // cap in length) cannot safely target a single row.
        let truncated = format!("{}…", "x".repeat(db_client::MAX_CELL_BYTES));
        assert!(db_client::is_cell_possibly_truncated(&truncated));

        let columns = vec!["id".to_string(), "name".to_string()];
        let rows = vec![vec![Some(truncated), Some("a".to_string())]];
        let primary_key_columns = vec!["id".to_string()];
        let edits = vec![((0usize, 1usize), CellValue::Text("x".to_string()))];

        let err =
            build_pending_updates('`', "users", &columns, &primary_key_columns, &rows, &edits)
                .expect_err("a truncated key value must be rejected");
        assert!(err.contains("truncated"));
    }

    #[test]
    fn build_insert_sql_quotes_columns_and_emits_value_literals() {
        let columns = vec!["id".to_string(), "name".to_string()];
        let sql = build_insert_sql(
            '`',
            "users",
            &columns,
            &[
                CellValue::Text("1".to_string()),
                CellValue::Text("O'Brien".to_string()),
            ],
        );
        assert_eq!(
            sql,
            "-- name: InsertResultRow :exec\nINSERT INTO `users` (`id`, `name`) VALUES (1, 'O''Brien')"
        );

        // A NULL value is emitted bare; quoting uses the double-quote driver char.
        let sql = build_insert_sql(
            '"',
            "t",
            &["a".to_string(), "b".to_string()],
            &[CellValue::Text("42".to_string()), CellValue::Null],
        );
        assert_eq!(
            sql,
            "-- name: InsertResultRow :exec\nINSERT INTO \"t\" (\"a\", \"b\") VALUES (42, NULL)"
        );
    }

    #[test]
    fn sql_value_literal_handles_null_default_and_text() {
        assert_eq!(sql_value_literal(&CellValue::Null), "NULL");
        assert_eq!(sql_value_literal(&CellValue::Default), "DEFAULT");
        assert_eq!(sql_value_literal(&CellValue::Text("42".to_string())), "42");
        assert_eq!(
            sql_value_literal(&CellValue::Text("O'Brien".to_string())),
            "'O''Brien'"
        );
        // DEFAULT is the bare keyword, never a quoted string.
        assert!(!sql_value_literal(&CellValue::Default).contains('\''));
    }

    #[test]
    fn build_update_sql_emits_default_keyword_unquoted() {
        let sql = build_update_sql(
            '`',
            "users",
            "status",
            &CellValue::Default,
            &[("id".to_string(), Some("5".to_string()))],
        );
        assert_eq!(
            sql,
            "-- name: UpdateResultCell :exec\nUPDATE `users` SET `status` = DEFAULT WHERE `id` = 5"
        );
        assert!(sql.contains("SET `status` = DEFAULT"));
        assert!(!sql.contains("'DEFAULT'"));
    }

    #[test]
    fn build_insert_sql_emits_default_keyword() {
        let sql = build_insert_sql(
            '`',
            "users",
            &["id".to_string(), "status".to_string()],
            &[CellValue::Default, CellValue::Text("active".to_string())],
        );
        assert_eq!(
            sql,
            "-- name: InsertResultRow :exec\nINSERT INTO `users` (`id`, `status`) VALUES (DEFAULT, 'active')"
        );
        assert!(!sql.contains("'DEFAULT'"));
    }

    #[test]
    fn build_delete_sql_uses_key_predicate_and_handles_null() {
        let sql = build_delete_sql('`', "users", &[("id".to_string(), Some("5".to_string()))]);
        assert_eq!(
            sql,
            "-- name: DeleteResultRow :exec\nDELETE FROM `users` WHERE `id` = 5"
        );

        // Composite key, one part of which is NULL.
        let sql = build_delete_sql(
            '`',
            "users",
            &[
                ("a".to_string(), Some("1".to_string())),
                ("b".to_string(), None),
            ],
        );
        assert_eq!(
            sql,
            "-- name: DeleteResultRow :exec\nDELETE FROM `users` WHERE `a` = 1 AND `b` IS NULL"
        );
    }

    #[test]
    fn build_key_predicate_builds_pairs_and_rejects_bad_keys() {
        let columns = vec!["id".to_string(), "name".to_string()];

        // Happy path: the key column value is read by position.
        let row = vec![Some("7".to_string()), Some("a".to_string())];
        let predicate = build_key_predicate(&columns, &["id".to_string()], &row)
            .expect("a present key column builds a predicate");
        assert_eq!(predicate, vec![("id".to_string(), Some("7".to_string()))]);

        // A key column missing from the result is rejected.
        let err = build_key_predicate(&["name".to_string()], &["id".to_string()], &row)
            .expect_err("a missing key column must be rejected");
        assert!(err.contains("key column is not in the result"));

        // A truncated key value is rejected.
        let truncated = format!("{}…", "x".repeat(db_client::MAX_CELL_BYTES));
        let row = vec![Some(truncated), Some("a".to_string())];
        let err = build_key_predicate(&columns, &["id".to_string()], &row)
            .expect_err("a truncated key value must be rejected");
        assert!(err.contains("truncated"));
    }

    #[test]
    fn build_pending_deletes_emits_one_delete_per_row_in_order() {
        let columns = vec!["id".to_string(), "name".to_string()];
        let rows = vec![
            vec![Some("1".to_string()), Some("a".to_string())],
            vec![Some("2".to_string()), Some("b".to_string())],
            vec![Some("3".to_string()), Some("c".to_string())],
        ];
        let primary_key_columns = vec!["id".to_string()];
        let deleted = vec![0usize, 2usize];

        let statements = build_pending_deletes(
            '`',
            "users",
            &columns,
            &primary_key_columns,
            &rows,
            &deleted,
        )
        .expect("deletes build for a keyed table");
        assert_eq!(
            statements,
            vec![
                build_delete_sql('`', "users", &[("id".to_string(), Some("1".to_string()))]),
                build_delete_sql('`', "users", &[("id".to_string(), Some("3".to_string()))]),
            ]
        );

        // No deletions yields no statements, even without a primary key.
        assert!(
            build_pending_deletes('`', "users", &columns, &[], &rows, &[])
                .expect("empty deletions need no key")
                .is_empty()
        );

        // A deletion with no primary key is rejected.
        let err = build_pending_deletes('`', "users", &columns, &[], &rows, &deleted)
            .expect_err("no primary key must be rejected");
        assert!(err.contains("no primary key"));
    }

    #[test]
    fn combine_row_statements_orders_delete_update_insert() {
        let deletes = vec!["d1".to_string(), "d2".to_string()];
        let updates = vec!["u1".to_string()];
        let inserts = vec!["i1".to_string(), "i2".to_string()];
        assert_eq!(
            combine_row_statements(deletes, updates, inserts),
            vec!["d1", "d2", "u1", "i1", "i2"]
        );

        // Empty groups drop out without disturbing the order.
        assert_eq!(
            combine_row_statements(vec![], vec!["u1".to_string()], vec!["i1".to_string()]),
            vec!["u1", "i1"]
        );
    }

    #[test]
    fn gutter_scroll_offset_maps_position_to_clamped_offset() {
        // No scroll when the content fits the viewport.
        assert_eq!(gutter_scroll_offset(50.0, 0.0, 100.0, 100.0), None);
        assert_eq!(gutter_scroll_offset(50.0, 0.0, 100.0, 80.0), None);

        // A click at the gutter start scrolls to the top/left (offset 0).
        assert_eq!(gutter_scroll_offset(0.0, 0.0, 100.0, 300.0), Some(0.0));

        // A click at the gutter end scrolls fully (offset = -(content-viewport)).
        assert_eq!(gutter_scroll_offset(100.0, 0.0, 100.0, 300.0), Some(-200.0));

        // The midpoint scrolls halfway.
        assert_eq!(gutter_scroll_offset(50.0, 0.0, 100.0, 300.0), Some(-100.0));

        // Positions outside the gutter clamp to the nearest end.
        assert_eq!(gutter_scroll_offset(-20.0, 0.0, 100.0, 300.0), Some(0.0));
        assert_eq!(gutter_scroll_offset(500.0, 0.0, 100.0, 300.0), Some(-200.0));

        // The viewport origin is subtracted before mapping.
        assert_eq!(gutter_scroll_offset(60.0, 10.0, 100.0, 300.0), Some(-100.0));
    }

    #[test]
    fn drag_scroll_offset_is_relative_to_grab_point() {
        // No scroll when content fits.
        assert_eq!(drag_scroll_offset(0.0, 50.0, 80.0, 100.0, 100.0), None);

        // Grabbing mid-thumb and not moving keeps the offset unchanged, no matter
        // where on the thumb the grab landed (this is the relative-drag fix).
        assert_eq!(
            drag_scroll_offset(-50.0, 30.0, 30.0, 100.0, 300.0),
            Some(-50.0)
        );

        // Dragging down by 10px maps through the thumb's actual travel. With a
        // 36px minimum thumb in a 100px gutter, 10px of thumb movement maps to
        // 31.25px of content movement.
        assert_eq!(
            drag_scroll_offset(-50.0, 30.0, 40.0, 100.0, 300.0),
            Some(-81.25)
        );

        // Dragging up reduces the offset toward 0.
        assert_eq!(
            drag_scroll_offset(-50.0, 30.0, 20.0, 100.0, 300.0),
            Some(-18.75)
        );

        // Offset is clamped to the scrollable range at both ends.
        assert_eq!(
            drag_scroll_offset(-50.0, 30.0, 0.0, 100.0, 300.0),
            Some(0.0)
        );
        assert_eq!(
            drag_scroll_offset(-50.0, 30.0, 200.0, 100.0, 300.0),
            Some(-200.0)
        );
    }

    #[test]
    fn thumb_range_matches_drawn_thumb() {
        // Content fits: thumb fills the whole gutter.
        assert_eq!(thumb_range(0.0, 100.0, 100.0, 0.0), (0.0, 100.0));

        // 3× content, scrolled to top: thumb respects the minimum size.
        let (start, end) = thumb_range(0.0, 100.0, 300.0, 0.0);
        assert!((start - 0.0).abs() < 1e-3);
        assert!((end - SCROLLBAR_THUMB_MIN).abs() < 1e-2);

        // Scrolled to the bottom: thumb sits against the gutter end.
        let (start, end) = thumb_range(0.0, 100.0, 300.0, -200.0);
        assert!((end - 100.0).abs() < 1e-2);
        assert!((start - (100.0 - SCROLLBAR_THUMB_MIN)).abs() < 1e-2);

        // The origin offsets the whole range into window coordinates.
        let (start, _) = thumb_range(10.0, 100.0, 300.0, 0.0);
        assert!((start - 10.0).abs() < 1e-3);
    }

    // Autonomous perf guard: a large result must virtualize — only the visible
    // window of rows is built per frame, not the whole result. If this fails,
    // scrolling is O(total_rows) per frame (the cause of single-digit FPS).
    #[gpui::test]
    fn table_virtualizes_large_result(cx: &mut gpui::TestAppContext) {
        use std::sync::atomic::Ordering;
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });

        let total = 5000usize;
        let cols: Vec<String> = (0..15).map(|i| format!("col{i}")).collect();
        let rows: Vec<Vec<Option<String>>> = (0..total)
            .map(|r| (0..15).map(|c| Some(format!("r{r}c{c}"))).collect())
            .collect();
        let result = QueryResult {
            columns: cols,
            rows,
            rows_affected: total as u64,
            execution_time_ms: 1,
        };

        let window = cx.add_window(|_window, cx| ResultView::new("perf", cx));
        let cx = &mut gpui::VisualTestContext::from_window(window.into(), cx);
        window
            .update(cx, |view, _window, cx| view.set_result(result, cx))
            .unwrap();

        RENDERED_ROW_COUNT.store(0, Ordering::Relaxed);
        cx.run_until_parked();
        window.update(cx, |_, window, _| window.refresh()).unwrap();
        cx.run_until_parked();

        let built = RENDERED_ROW_COUNT.load(Ordering::Relaxed);
        eprintln!("VIRTUALIZATION CHECK: built {built} rows out of {total}");
        assert!(built > 0, "no rows were rendered — the draw did not run");
        assert!(
            built < 1000,
            "table is NOT virtualizing: built {built} of {total} rows per draw (scroll is O(n))"
        );

        // Time repeated full redraws (build + layout) as an FPS proxy. GPU paint
        // is not exercised in tests, but element construction + flexbox layout is
        // the CPU cost that dominates the frame, so this tracks the real bottleneck.
        let frames = 120;
        let start = std::time::Instant::now();
        for _ in 0..frames {
            window.update(cx, |_, window, _| window.refresh()).unwrap();
            cx.run_until_parked();
        }
        let elapsed = start.elapsed();
        let per_frame_ms = elapsed.as_secs_f64() * 1000.0 / frames as f64;
        eprintln!(
            "FRAME TIME: {per_frame_ms:.2} ms/frame over {frames} frames (~{:.0} build-FPS)",
            1000.0 / per_frame_ms
        );
    }

    #[test]
    fn column_editor_kind_recognizes_bool_types() {
        assert!(matches!(
            column_editor_kind("tinyint(1)"),
            CellEditorKind::Boolean
        ));
        assert!(matches!(
            column_editor_kind("BOOL"),
            CellEditorKind::Boolean
        ));
        assert!(matches!(
            column_editor_kind("boolean"),
            CellEditorKind::Boolean
        ));
        assert!(!matches!(
            column_editor_kind("tinyint(4)"),
            CellEditorKind::Boolean
        ));
        assert!(!matches!(
            column_editor_kind("int"),
            CellEditorKind::Boolean
        ));
    }

    #[test]
    fn column_editor_kind_recognizes_timestamp_as_datetime() {
        assert!(matches!(
            column_editor_kind("timestamp"),
            CellEditorKind::DateTime
        ));
        assert!(matches!(
            column_editor_kind("timestamp(6)"),
            CellEditorKind::DateTime
        ));
        assert!(matches!(
            column_editor_kind("datetime"),
            CellEditorKind::DateTime
        ));
    }

    #[test]
    fn parse_enum_values_extracts_quoted_variants() {
        let values = parse_enum_values("enum('a','b','c')");
        assert_eq!(values, vec!["a", "b", "c"]);
        let values2 = parse_enum_values("ENUM('yes','no')");
        assert_eq!(values2, vec!["yes", "no"]);
        let empty = parse_enum_values("varchar(255)");
        assert!(empty.is_empty());
    }

    #[test]
    fn is_truthy_bool_handles_canonical_values() {
        assert!(is_truthy_bool("1"));
        assert!(is_truthy_bool("true"));
        assert!(is_truthy_bool("TRUE"));
        assert!(is_truthy_bool("yes"));
        assert!(!is_truthy_bool("0"));
        assert!(!is_truthy_bool("false"));
        assert!(!is_truthy_bool(""));
    }

    #[test]
    fn is_valid_date_accepts_iso_and_rejects_malformed() {
        assert!(is_valid_date("2024-01-15"));
        assert!(is_valid_date("2000-12-31"));
        assert!(is_valid_date(""));
        assert!(!is_valid_date("2024-1-5"));
        assert!(!is_valid_date("24-01-15"));
        assert!(!is_valid_date("2024-13-01"));
        assert!(!is_valid_date("2024-00-01"));
        assert!(!is_valid_date("not-a-date"));
        assert!(!is_valid_date("2024/01/15"));
    }

    #[test]
    fn is_valid_datetime_accepts_iso_and_rejects_malformed() {
        assert!(is_valid_datetime("2024-01-15 10:30:00"));
        assert!(is_valid_datetime("2024-12-31 23:59:59"));
        assert!(is_valid_datetime("2024-01-15 10:30:00.123"));
        assert!(is_valid_datetime(""));
        assert!(!is_valid_datetime("2024-01-15"));
        assert!(!is_valid_datetime("2024-01-15 25:00:00"));
        assert!(!is_valid_datetime("2024-01-15 10:60:00"));
        assert!(!is_valid_datetime("2024-13-01 10:00:00"));
        assert!(!is_valid_datetime("not a datetime"));
    }

    #[test]
    fn find_matches_locate_substring_across_cells() {
        let rows: Vec<Vec<Option<String>>> = vec![
            vec![Some("hello world".to_string()), Some("foo".to_string())],
            vec![Some("bar".to_string()), Some("hello".to_string())],
            vec![None, Some("baz".to_string())],
        ];
        let query = "hello";
        let order: Vec<usize> = (0..rows.len()).collect();
        let mut matches: Vec<(usize, usize)> = Vec::new();
        for &abs_idx in &order {
            if let Some(row) = rows.get(abs_idx) {
                for (col_idx, cell) in row.iter().enumerate() {
                    let text = cell.as_deref().unwrap_or("").to_lowercase();
                    if text.contains(query) {
                        matches.push((abs_idx, col_idx));
                    }
                }
            }
        }
        assert_eq!(matches.len(), 2);
        assert!(matches.contains(&(0, 0)));
        assert!(matches.contains(&(1, 1)));
    }

    #[test]
    fn find_navigation_cycles_forward_and_backward() {
        let match_count = 3usize;
        let mut current = 0usize;

        current = (current + 1) % match_count;
        assert_eq!(current, 1);
        current = (current + 1) % match_count;
        assert_eq!(current, 2);
        current = (current + 1) % match_count;
        assert_eq!(current, 0, "forward should wrap around");

        current = current.checked_sub(1).unwrap_or(match_count - 1);
        assert_eq!(current, 2, "backward should wrap around");
        current = current.checked_sub(1).unwrap_or(match_count - 1);
        assert_eq!(current, 1);
    }

    #[test]
    fn parse_clipboard_rows_parses_csv_and_skips_header() {
        let cols = vec!["id".to_string(), "name".to_string()];
        let csv = "id,name\n1,Alice\n2,Bob\n";
        let rows = ResultView::parse_clipboard_rows(csv, 2, &cols);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], CellValue::Text("1".to_string()));
        assert_eq!(rows[0][1], CellValue::Text("Alice".to_string()));
        assert_eq!(rows[1][0], CellValue::Text("2".to_string()));
    }

    #[test]
    fn parse_clipboard_rows_parses_tsv() {
        let cols = vec!["a".to_string(), "b".to_string()];
        let tsv = "a\tb\n1\t2\n";
        let rows = ResultView::parse_clipboard_rows(tsv, 2, &cols);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], CellValue::Text("1".to_string()));
        assert_eq!(rows[0][1], CellValue::Text("2".to_string()));
    }

    #[test]
    fn parse_clipboard_rows_handles_empty_field_as_null() {
        let cols = vec!["a".to_string(), "b".to_string()];
        let csv = "1,\n";
        let rows = ResultView::parse_clipboard_rows(csv, 2, &cols);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], CellValue::Text("1".to_string()));
        assert_eq!(rows[0][1], CellValue::Null);
    }

    #[test]
    fn parse_clipboard_rows_pads_short_rows() {
        let cols = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let csv = "1,2\n";
        let rows = ResultView::parse_clipboard_rows(csv, 3, &cols);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].len(), 3);
        assert_eq!(rows[0][2], CellValue::Null);
    }

    #[test]
    fn parse_clipboard_rows_handles_quoted_csv_fields() {
        let cols = vec!["a".to_string(), "b".to_string()];
        let csv = "\"hello, world\",\"say \"\"hi\"\"\"\n";
        let rows = ResultView::parse_clipboard_rows(csv, 2, &cols);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], CellValue::Text("hello, world".to_string()));
        assert_eq!(rows[0][1], CellValue::Text("say \"hi\"".to_string()));
    }

    #[test]
    fn export_html_escapes_special_chars_and_wraps_null() {
        let result = QueryResult {
            columns: vec!["a".to_string(), "b".to_string()],
            rows: vec![
                vec![Some("<b>hello</b>".to_string()), None],
                vec![Some("a & b".to_string()), Some("x\"y".to_string())],
            ],
            rows_affected: 0,
            execution_time_ms: 0,
        };
        let html = ResultView::export_html(&result);
        assert!(html.contains("<th>a</th>"));
        assert!(html.contains("<th>b</th>"));
        assert!(html.contains("&lt;b&gt;hello&lt;/b&gt;"));
        assert!(html.contains("<em>NULL</em>"));
        assert!(html.contains("a &amp; b"));
        assert!(html.contains("x&quot;y"));
    }

    #[test]
    fn export_sql_update_uses_first_column_as_pk() {
        let result = QueryResult {
            columns: vec!["id".to_string(), "name".to_string(), "val".to_string()],
            rows: vec![
                vec![Some("1".to_string()), Some("alice".to_string()), None],
                vec![
                    Some("2".to_string()),
                    Some("bob".to_string()),
                    Some("42".to_string()),
                ],
            ],
            rows_affected: 0,
            execution_time_ms: 0,
        };
        let sql = ResultView::export_sql_update(&result, "users");
        assert!(sql.contains("UPDATE users SET name = 'alice', val = NULL WHERE id = 1;"));
        assert!(sql.contains("UPDATE users SET name = 'bob', val = 42 WHERE id = 2;"));
    }

    fn export_fixture() -> QueryResult {
        QueryResult {
            columns: vec!["id".to_string(), "name".to_string()],
            rows: vec![
                vec![Some("1".to_string()), Some("Alice".to_string())],
                vec![Some("2".to_string()), Some("Bob".to_string())],
            ],
            rows_affected: 0,
            execution_time_ms: 0,
        }
    }

    #[test]
    fn build_export_text_prepends_ddl_when_present() {
        let result = export_fixture();
        let text = ResultView::build_export_text(
            &result,
            ExportChoice::Csv,
            true,
            false,
            Some("t"),
            Some("CREATE TABLE t (id INT);"),
        );
        assert!(text.starts_with("CREATE TABLE t (id INT);"));
        assert!(text.contains("id,name"));
        assert!(text.contains("1,Alice"));

        // No DDL prefix when not requested.
        let plain =
            ResultView::build_export_text(&result, ExportChoice::Csv, true, false, Some("t"), None);
        assert!(!plain.contains("CREATE TABLE"));
    }

    #[test]
    fn build_export_text_drops_header_when_headers_disabled() {
        let result = export_fixture();
        let with_headers =
            ResultView::build_export_text(&result, ExportChoice::Csv, true, false, None, None);
        assert!(with_headers.starts_with("id,name"));

        let without_headers =
            ResultView::build_export_text(&result, ExportChoice::Csv, false, false, None, None);
        assert!(!without_headers.starts_with("id,name"));
        assert!(without_headers.starts_with("1,Alice"));
    }

    #[test]
    fn transpose_result_swaps_rows_and_columns() {
        let result = export_fixture();
        let transposed = ResultView::transpose_result(&result);
        // One label column plus one column per original row.
        assert_eq!(transposed.columns, vec!["column", "1", "2"]);
        // One row per original column.
        assert_eq!(transposed.rows.len(), 2);
        assert_eq!(
            transposed.rows[0],
            vec![
                Some("id".to_string()),
                Some("1".to_string()),
                Some("2".to_string())
            ]
        );
        assert_eq!(
            transposed.rows[1],
            vec![
                Some("name".to_string()),
                Some("Alice".to_string()),
                Some("Bob".to_string())
            ]
        );
    }

    #[test]
    fn chart_series_extracts_numeric_pairs_and_skips_non_numeric() {
        let result = QueryResult {
            columns: vec!["label".to_string(), "value".to_string()],
            rows: vec![
                vec![Some("a".to_string()), Some("10".to_string())],
                vec![Some("b".to_string()), None],
                vec![Some("c".to_string()), Some("not-a-number".to_string())],
                vec![Some("d".to_string()), Some("4.5".to_string())],
            ],
            rows_affected: 0,
            execution_time_ms: 0,
        };
        let series = ResultView::chart_series(&result, Some(0), 1);
        assert_eq!(
            series,
            vec![("a".to_string(), 10.0), ("d".to_string(), 4.5)]
        );

        // Without a label column the row number is used.
        let by_index = ResultView::chart_series(&result, None, 1);
        assert_eq!(by_index[0].0, "1");
        assert_eq!(by_index[1].0, "4");
    }

    #[test]
    fn series_bounds_pins_baseline_to_zero() {
        let series = vec![("a".to_string(), 3.0), ("b".to_string(), 8.0)];
        assert_eq!(ResultView::series_bounds(&series), Some((0.0, 8.0)));

        // Negative values lower the minimum below zero.
        let negative = vec![("a".to_string(), -2.0), ("b".to_string(), 5.0)];
        assert_eq!(ResultView::series_bounds(&negative), Some((-2.0, 5.0)));

        assert_eq!(ResultView::series_bounds(&[]), None);
    }

    #[test]
    fn first_numeric_column_finds_numeric() {
        let result = export_fixture();
        // Column 0 (id) is numeric; column 1 (name) is not.
        assert_eq!(ResultView::first_numeric_column(&result), Some(0));
    }

    #[gpui::test]
    fn toggle_chart_opens_and_picks_numeric_column(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let window = cx.add_window(|_window, cx| {
            let mut view = ResultView::new("test", cx);
            view.set_result(export_fixture(), cx);
            view
        });
        window
            .update(cx, |view, _window, cx| {
                assert!(!view.chart_open);
                view.toggle_chart(cx);
                assert!(view.chart_open);
                assert_eq!(view.chart_value_column, Some(0));
                view.toggle_chart(cx);
                assert!(!view.chart_open);
            })
            .unwrap();
    }

    #[gpui::test]
    fn open_export_dialog_toggles_state(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let window = cx.add_window(|_window, cx| {
            let mut view = ResultView::new("test", cx);
            view.set_result(export_fixture(), cx);
            view
        });
        window
            .update(cx, |view, _window, cx| {
                assert!(!view.export_dialog_open);
                view.open_export_dialog(cx);
                assert!(view.export_dialog_open);
                view.open_export_dialog(cx);
                assert!(!view.export_dialog_open);
            })
            .unwrap();
    }

    #[gpui::test]
    fn recompute_local_filter_inner_filters_rows_by_column(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let result = QueryResult {
            columns: vec!["name".to_string(), "city".to_string()],
            rows: vec![
                vec![Some("Alice".to_string()), Some("Berlin".to_string())],
                vec![Some("Bob".to_string()), Some("Paris".to_string())],
                vec![Some("Alice".to_string()), Some("Paris".to_string())],
            ],
            rows_affected: 0,
            execution_time_ms: 0,
        };
        let window = cx.add_window(|_window, cx| {
            let mut view = ResultView::new("test", cx);
            view.set_result(result, cx);
            view
        });
        window
            .update(cx, |view, _window, _cx| {
                view.visible_columns = vec![0, 1];

                // Filter by name = "alice" (case-insensitive).
                view.local_filters = vec!["alice".to_string(), String::new()];
                view.recompute_local_filter_inner();
                assert_eq!(view.filtered_display_order, vec![0, 2]);

                // Filter by both columns.
                view.local_filters = vec!["alice".to_string(), "paris".to_string()];
                view.recompute_local_filter_inner();
                assert_eq!(view.filtered_display_order, vec![2]);

                // No match.
                view.local_filters = vec!["alice".to_string(), "tokyo".to_string()];
                view.recompute_local_filter_inner();
                assert!(view.filtered_display_order.is_empty());

                // Empty filters → full order.
                view.local_filters = vec![String::new(), String::new()];
                view.recompute_local_filter_inner();
                assert_eq!(view.filtered_display_order, vec![0, 1, 2]);
            })
            .unwrap();
    }

    #[gpui::test]
    fn recompute_layout_hides_columns_and_updates_visible_columns(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let result = QueryResult {
            columns: vec!["a".to_string(), "b".to_string(), "c".to_string()],
            rows: vec![vec![
                Some("1".to_string()),
                Some("2".to_string()),
                Some("3".to_string()),
            ]],
            rows_affected: 0,
            execution_time_ms: 0,
        };
        let window = cx.add_window(|_window, cx| {
            let mut view = ResultView::new("test", cx);
            view.set_result(result, cx);
            view
        });
        window
            .update(cx, |view, _window, _cx| {
                // All columns visible initially.
                assert_eq!(view.visible_columns, vec![0, 1, 2]);

                // Hide column 1.
                view.hidden_columns.insert(1);
                view.recompute_layout();
                assert_eq!(view.visible_columns, vec![0, 2]);
                assert_eq!(view.col_widths.len(), 2);

                // Restore.
                view.hidden_columns.remove(&1);
                view.recompute_layout();
                assert_eq!(view.visible_columns, vec![0, 1, 2]);
            })
            .unwrap();
    }

    fn init_result_view_test(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
    }

    fn sample_table_result() -> QueryResult {
        QueryResult {
            columns: vec!["id".to_string(), "name".to_string()],
            rows: vec![
                vec![Some("1".to_string()), Some("Alice".to_string())],
                vec![Some("2".to_string()), Some("Bob".to_string())],
                vec![Some("3".to_string()), Some("Claire".to_string())],
            ],
            rows_affected: 0,
            execution_time_ms: 0,
        }
    }

    fn wide_table_result() -> QueryResult {
        let columns: Vec<String> = (0..18).map(|index| format!("column_{index}")).collect();
        let rows = (0..6)
            .map(|row| {
                (0..columns.len())
                    .map(|col| Some(format!("row_{row}_column_{col}_wide_value")))
                    .collect()
            })
            .collect();
        QueryResult {
            columns,
            rows,
            rows_affected: 0,
            execution_time_ms: 0,
        }
    }

    fn draw_result_view(_window: gpui::WindowHandle<ResultView>, cx: &mut gpui::VisualTestContext) {
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        cx.run_until_parked();
    }

    struct ResultViewFrame {
        view: Entity<ResultView>,
    }

    impl Render for ResultViewFrame {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div().w(px(700.)).h(px(500.)).child(self.view.clone())
        }
    }

    fn draw_result_view_frame(
        _window: gpui::WindowHandle<ResultViewFrame>,
        cx: &mut gpui::VisualTestContext,
    ) {
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        cx.run_until_parked();
    }

    fn debug_center(
        cx: &mut gpui::VisualTestContext,
        selector: &'static str,
    ) -> gpui::Point<Pixels> {
        cx.debug_bounds(selector)
            .unwrap_or_else(|| panic!("expected debug bounds for {selector}"))
            .center()
    }

    fn table_backed_result_window(
        cx: &mut gpui::TestAppContext,
    ) -> (
        gpui::WindowHandle<ResultView>,
        Entity<ResultView>,
        gpui::VisualTestContext,
    ) {
        init_result_view_test(cx);
        let store = cx.update(|cx| cx.new(DatabaseStore::new));
        let connection_id = uuid::Uuid::new_v4();
        let window = cx.add_window({
            let store = store.downgrade();
            move |window, cx| {
                let mut view = ResultView::new("users", cx).with_table_context(
                    store,
                    connection_id,
                    "public".to_string(),
                    "users".to_string(),
                    window,
                    cx,
                );
                view.set_result(sample_table_result(), cx);
                view
            }
        });
        let view = window.root(cx).unwrap();
        let mut cx = gpui::VisualTestContext::from_window(window.into(), cx);
        draw_result_view(window, &mut cx);
        (window, view, cx)
    }

    fn framed_plain_result_window(
        cx: &mut gpui::TestAppContext,
        result: QueryResult,
    ) -> (
        gpui::WindowHandle<ResultViewFrame>,
        Entity<ResultView>,
        gpui::VisualTestContext,
    ) {
        init_result_view_test(cx);
        let window = cx.add_window(move |_window, cx| {
            let view = cx.new(|cx| {
                let mut view = ResultView::new("query", cx);
                view.set_result(result, cx);
                view
            });
            ResultViewFrame { view }
        });
        let view = window
            .read_with(cx, |frame, _cx| frame.view.clone())
            .expect("test window should contain a result view");
        let mut cx = gpui::VisualTestContext::from_window(window.into(), cx);
        draw_result_view_frame(window, &mut cx);
        (window, view, cx)
    }

    fn plain_result_window(
        cx: &mut gpui::TestAppContext,
        result: QueryResult,
    ) -> (
        gpui::WindowHandle<ResultView>,
        Entity<ResultView>,
        gpui::VisualTestContext,
    ) {
        init_result_view_test(cx);
        let window = cx.add_window(move |_window, cx| {
            let mut view = ResultView::new("query", cx);
            view.set_result(result, cx);
            view
        });
        let view = window.root(cx).unwrap();
        let mut cx = gpui::VisualTestContext::from_window(window.into(), cx);
        draw_result_view(window, &mut cx);
        (window, view, cx)
    }

    #[gpui::test]
    fn env_accent_bar_shown_only_when_color_set(cx: &mut gpui::TestAppContext) {
        init_result_view_test(cx);
        let window = cx.add_window(|_window, cx| {
            let mut view =
                ResultView::new("query", cx).with_env_color(Some(gpui::rgb(0xf85149).into()));
            view.set_result(sample_table_result(), cx);
            view
        });
        let mut cx = gpui::VisualTestContext::from_window(window.into(), cx);
        draw_result_view(window, &mut cx);
        assert!(
            cx.debug_bounds("ENV_ACCENT_BAR").is_some(),
            "env accent bar must render when a color is set"
        );
    }

    #[gpui::test]
    fn env_accent_bar_absent_without_color(cx: &mut gpui::TestAppContext) {
        init_result_view_test(cx);
        let window = cx.add_window(|_window, cx| {
            let mut view = ResultView::new("query", cx);
            view.set_result(sample_table_result(), cx);
            view
        });
        let mut cx = gpui::VisualTestContext::from_window(window.into(), cx);
        draw_result_view(window, &mut cx);
        assert!(
            cx.debug_bounds("ENV_ACCENT_BAR").is_none(),
            "env accent bar must not render without a color"
        );
    }

    #[test]
    fn detect_special_result_classifies_ddl_and_explain() {
        use super::{SpecialResult, detect_special_result};
        let create_columns = vec!["Table".to_string(), "Create Table".to_string()];
        assert_eq!(
            detect_special_result(Some("SHOW CREATE TABLE t"), &[]),
            SpecialResult::Ddl
        );
        assert_eq!(
            detect_special_result(Some("  show create database d"), &[]),
            SpecialResult::Ddl
        );
        assert_eq!(
            detect_special_result(Some("EXPLAIN SELECT 1"), &[]),
            SpecialResult::ExplainPlan
        );
        assert_eq!(
            detect_special_result(Some("EXPLAIN ANALYZE SELECT 1"), &[]),
            SpecialResult::ExplainPlan
        );
        assert_eq!(
            detect_special_result(Some("EXPLAIN FORMAT=TREE SELECT 1"), &[]),
            SpecialResult::ExplainPlan
        );
        assert_eq!(
            detect_special_result(Some("EXPLAIN QUERY PLAN SELECT 1"), &[]),
            SpecialResult::ExplainPlan
        );
        assert_eq!(
            detect_special_result(Some("-- a comment\nEXPLAIN SELECT 1"), &[]),
            SpecialResult::ExplainPlan
        );
        assert_eq!(
            detect_special_result(Some("SELECT * FROM t"), &["id".to_string()]),
            SpecialResult::None
        );
        // Column-based detection when the originating query is unknown.
        assert_eq!(
            detect_special_result(None, &create_columns),
            SpecialResult::Ddl
        );
    }

    #[test]
    fn ddl_text_from_result_picks_create_column_then_last() {
        use super::ddl_text_from_result;
        let with_create = QueryResult {
            columns: vec!["Table".to_string(), "Create Table".to_string()],
            rows: vec![vec![
                Some("t".to_string()),
                Some("CREATE TABLE t (id INT)".to_string()),
            ]],
            rows_affected: 0,
            execution_time_ms: 0,
        };
        assert_eq!(
            ddl_text_from_result(&with_create).as_deref(),
            Some("CREATE TABLE t (id INT)")
        );
        let without_create = QueryResult {
            columns: vec!["a".to_string(), "b".to_string()],
            rows: vec![vec![Some("x".to_string()), Some("y".to_string())]],
            rows_affected: 0,
            execution_time_ms: 0,
        };
        assert_eq!(ddl_text_from_result(&without_create).as_deref(), Some("y"));
    }

    #[gpui::test]
    fn ddl_result_renders_formatted_view(cx: &mut gpui::TestAppContext) {
        let result = QueryResult {
            columns: vec!["Table".to_string(), "Create Table".to_string()],
            rows: vec![vec![
                Some("t".to_string()),
                Some("CREATE TABLE t (\n  id INT\n)".to_string()),
            ]],
            rows_affected: 0,
            execution_time_ms: 0,
        };
        let (_window, _view, mut cx) = plain_result_window(cx, result);
        assert!(
            cx.debug_bounds("DDL_RESULT_VIEW").is_some(),
            "a SHOW CREATE result must render the formatted DDL view by default"
        );
    }

    #[gpui::test]
    fn explain_result_renders_plan_tree(cx: &mut gpui::TestAppContext) {
        init_result_view_test(cx);
        let window = cx.add_window(|_window, cx| {
            let mut view = ResultView::new("query", cx);
            view.base_sql = Some("EXPLAIN SELECT * FROM t".to_string());
            let result = QueryResult {
                columns: vec!["QUERY PLAN".to_string()],
                rows: vec![
                    vec![Some("Seq Scan on t".to_string())],
                    vec![Some("  Filter: (id > 1)".to_string())],
                ],
                rows_affected: 0,
                execution_time_ms: 0,
            };
            view.set_result(result, cx);
            view
        });
        let mut cx = gpui::VisualTestContext::from_window(window.into(), cx);
        draw_result_view(window, &mut cx);
        assert!(
            cx.debug_bounds("EXPLAIN_PLAN_VIEW").is_some(),
            "an EXPLAIN result must render the plan tree by default"
        );
    }

    #[gpui::test]
    fn plain_result_has_no_special_view(cx: &mut gpui::TestAppContext) {
        let (_window, _view, mut cx) = plain_result_window(cx, sample_table_result());
        assert!(cx.debug_bounds("DDL_RESULT_VIEW").is_none());
        assert!(cx.debug_bounds("EXPLAIN_PLAN_VIEW").is_none());
    }

    #[gpui::test]
    fn visual_double_click_cell_starts_editing(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) = table_backed_result_window(cx);
        let cell_center = debug_center(&mut cx, "CELL-0-1");

        cx.simulate_event(gpui::MouseDownEvent {
            position: cell_center,
            button: gpui::MouseButton::Left,
            modifiers: gpui::Modifiers::none(),
            click_count: 2,
            first_mouse: false,
        });
        cx.simulate_event(gpui::MouseUpEvent {
            position: cell_center,
            button: gpui::MouseButton::Left,
            modifiers: gpui::Modifiers::none(),
            click_count: 2,
        });
        draw_result_view(window, &mut cx);

        view.update(&mut cx, |view, _cx| {
            assert_eq!(view.selected_cell, Some((0, 1)));
            assert!(
                view.cell_edit
                    .as_ref()
                    .is_some_and(|edit| edit.abs_idx == 0 && edit.col_idx == 1),
                "double-click should open the inline cell editor"
            );
        });
    }

    #[gpui::test]
    fn double_click_keeps_cell_text_with_caret_at_end(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) = table_backed_result_window(cx);
        let cell_center = debug_center(&mut cx, "CELL-0-1");

        cx.simulate_event(gpui::MouseDownEvent {
            position: cell_center,
            button: gpui::MouseButton::Left,
            modifiers: gpui::Modifiers::none(),
            click_count: 2,
            first_mouse: false,
        });
        cx.simulate_event(gpui::MouseUpEvent {
            position: cell_center,
            button: gpui::MouseButton::Left,
            modifiers: gpui::Modifiers::none(),
            click_count: 2,
        });
        draw_result_view(window, &mut cx);

        view.update(&mut cx, |view, cx| {
            let text = view
                .cell_edit
                .as_ref()
                .map(|edit| edit.editor.read(cx).text(cx))
                .unwrap_or_default();
            assert_eq!(
                text, "Alice",
                "double-click must keep the cell's value, not wipe it"
            );
            assert!(
                !view.value_editor_open,
                "starting an inline edit must close any auto-opened value editor popup"
            );
        });
    }

    #[gpui::test]
    fn visual_double_click_plain_result_cell_starts_editing(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) = plain_result_window(cx, sample_table_result());
        let cell_center = debug_center(&mut cx, "CELL-0-1");

        cx.simulate_event(gpui::MouseDownEvent {
            position: cell_center,
            button: gpui::MouseButton::Left,
            modifiers: gpui::Modifiers::none(),
            click_count: 2,
            first_mouse: false,
        });
        cx.simulate_event(gpui::MouseUpEvent {
            position: cell_center,
            button: gpui::MouseButton::Left,
            modifiers: gpui::Modifiers::none(),
            click_count: 2,
        });
        draw_result_view(window, &mut cx);

        view.update(&mut cx, |view, _cx| {
            assert_eq!(view.selected_cell, Some((0, 1)));
            assert!(
                view.cell_edit
                    .as_ref()
                    .is_some_and(|edit| edit.abs_idx == 0 && edit.col_idx == 1),
                "double-click should open the inline editor even without table context"
            );
        });
    }

    #[gpui::test]
    fn visual_multiline_cell_click_opens_value_editor_popup(cx: &mut gpui::TestAppContext) {
        let ddl = "line one\n  nested line two\n  nested line three\nfinal line";
        let result = QueryResult {
            columns: vec!["definition".to_string()],
            rows: vec![vec![Some(ddl.to_string())]],
            rows_affected: 0,
            execution_time_ms: 0,
        };
        let (window, view, mut cx) = plain_result_window(cx, result);
        let cell_center = debug_center(&mut cx, "CELL-0-0");

        cx.simulate_click(cell_center, gpui::Modifiers::none());
        draw_result_view(window, &mut cx);

        assert!(
            cx.debug_bounds("VALUE_EDITOR_POPUP").is_some(),
            "multiline cell selection should open the value editor popup automatically"
        );
        view.update(&mut cx, |view, cx| {
            assert!(view.value_editor_open);
            assert_eq!(view.selected_cell, Some((0, 0)));
            let text = view
                .value_editor
                .as_ref()
                .map(|editor| editor.read(cx).text(cx))
                .unwrap_or_default();
            assert!(text.contains("line one"));
        });
    }

    #[gpui::test]
    fn visual_value_editor_popup_loads_selected_cell_text(cx: &mut gpui::TestAppContext) {
        let ddl = "line one\n  nested line two\n  nested line three\nfinal line";
        let result = QueryResult {
            columns: vec!["definition".to_string()],
            rows: vec![vec![Some(ddl.to_string())]],
            rows_affected: 0,
            execution_time_ms: 0,
        };
        let (window, view, mut cx) = plain_result_window(cx, result);
        let cell_center = debug_center(&mut cx, "CELL-0-0");

        cx.simulate_click(cell_center, gpui::Modifiers::none());
        view.update(&mut cx, |view, cx| {
            view.value_editor_open = true;
            cx.notify();
        });
        draw_result_view(window, &mut cx);

        assert!(
            cx.debug_bounds("VALUE_EDITOR_POPUP").is_some(),
            "explicit value editor open should show the selected cell text"
        );
        view.update(&mut cx, |view, cx| {
            let text = view
                .value_editor
                .as_ref()
                .map(|editor| editor.read(cx).text(cx))
                .unwrap_or_default();
            assert!(text.contains("line one"));
            assert!(text.contains("\n  nested line two"));
        });
    }

    #[gpui::test]
    fn visual_value_editor_popup_can_be_resized(cx: &mut gpui::TestAppContext) {
        let ddl = "line one\nline two\nline three";
        let result = QueryResult {
            columns: vec!["DDL".to_string(), "note".to_string()],
            rows: vec![vec![Some(ddl.to_string()), Some("x".to_string())]],
            rows_affected: 0,
            execution_time_ms: 0,
        };
        let (window, view, mut cx) = plain_result_window(cx, result);
        let cell_center = debug_center(&mut cx, "CELL-0-0");

        cx.simulate_click(cell_center, gpui::Modifiers::none());
        view.update(&mut cx, |view, cx| {
            view.value_editor_open = true;
            cx.notify();
        });
        draw_result_view(window, &mut cx);

        let before = cx
            .debug_bounds("VALUE_EDITOR_POPUP")
            .expect("value editor popup bounds");
        let resize = debug_center(&mut cx, "VALUE_EDITOR_RESIZE");
        let end = gpui::point(resize.x + px(80.0), resize.y + px(50.0));

        cx.simulate_mouse_down(resize, gpui::MouseButton::Left, gpui::Modifiers::none());
        draw_result_view(window, &mut cx);
        cx.simulate_mouse_move(end, Some(gpui::MouseButton::Left), gpui::Modifiers::none());
        cx.simulate_mouse_up(end, gpui::MouseButton::Left, gpui::Modifiers::none());
        draw_result_view(window, &mut cx);

        let after = cx
            .debug_bounds("VALUE_EDITOR_POPUP")
            .expect("resized value editor popup bounds");
        assert!(after.size.width > before.size.width);
        assert!(after.size.height > before.size.height);
        view.update(&mut cx, |view, _cx| {
            assert!(view.value_editor_resize_drag.is_none());
        });
    }

    #[gpui::test]
    fn visual_second_click_selected_cell_starts_editing(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) = table_backed_result_window(cx);
        let cell_center = debug_center(&mut cx, "CELL-0-1");

        cx.simulate_click(cell_center, gpui::Modifiers::none());
        draw_result_view(window, &mut cx);
        cx.simulate_click(cell_center, gpui::Modifiers::none());
        draw_result_view(window, &mut cx);

        view.update(&mut cx, |view, _cx| {
            assert_eq!(view.selected_cell, Some((0, 1)));
            assert!(
                view.cell_edit
                    .as_ref()
                    .is_some_and(|edit| edit.abs_idx == 0 && edit.col_idx == 1),
                "second click on an already-selected cell should open the inline editor"
            );
        });
    }

    #[gpui::test]
    fn visual_shift_click_cells_selects_range(cx: &mut gpui::TestAppContext) {
        let (_window, view, mut cx) = table_backed_result_window(cx);
        let first_cell = debug_center(&mut cx, "CELL-0-0");
        let last_cell = debug_center(&mut cx, "CELL-2-1");

        cx.simulate_click(first_cell, gpui::Modifiers::none());
        cx.simulate_click(last_cell, gpui::Modifiers::shift());

        view.update(&mut cx, |view, _cx| {
            assert_eq!(view.selected_cell, Some((2, 1)));
            assert!(
                view.selected_rows.is_empty(),
                "cell range selection must not also select whole rows"
            );
            assert!(view.selected_cell_range_contains(0, 0, 0));
            assert!(view.selected_cell_range_contains(1, 1, 0));
            assert!(view.selected_cell_range_contains(2, 2, 1));
            assert!(!view.selected_cell_range_contains(2, 2, 2));
        });
    }

    #[gpui::test]
    fn visual_drag_cells_selects_matrix(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) = table_backed_result_window(cx);
        let first_cell = debug_center(&mut cx, "CELL-0-0");
        let last_cell = debug_center(&mut cx, "CELL-2-1");

        cx.simulate_mouse_down(first_cell, gpui::MouseButton::Left, gpui::Modifiers::none());
        cx.simulate_mouse_move(
            last_cell,
            Some(gpui::MouseButton::Left),
            gpui::Modifiers::none(),
        );
        cx.simulate_mouse_up(last_cell, gpui::MouseButton::Left, gpui::Modifiers::none());
        draw_result_view(window, &mut cx);

        view.update(&mut cx, |view, _cx| {
            assert_eq!(view.selected_cell, Some((2, 1)));
            assert!(
                view.selected_rows.is_empty(),
                "dragged cell range selection must not also select whole rows"
            );
            assert!(view.selected_cell_range_contains(0, 0, 0));
            assert!(view.selected_cell_range_contains(1, 1, 0));
            assert!(view.selected_cell_range_contains(2, 2, 1));
            assert!(!view.selected_cell_range_contains(2, 2, 2));
        });
    }

    #[gpui::test]
    fn visual_row_actions_are_menu_only(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) = table_backed_result_window(cx);
        assert!(cx.debug_bounds("EDIT_ROW-0").is_none());
        assert!(cx.debug_bounds("DELETE_ROW-0").is_none());

        open_gutter_menu_and_click(window, &mut cx, "GUTTER-1", "MENU_ITEM-Delete Row");

        view.update(&mut cx, |view, _cx| {
            assert!(
                view.deleted_rows.contains(&1),
                "row delete menu item should only mark the row for pending delete"
            );
            assert!(view.selected_rows.contains(&1));
            assert_eq!(view.selected_rows.len(), 1);
        });
    }

    #[gpui::test]
    fn visual_horizontal_scrollbar_drag_changes_offset(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) = framed_plain_result_window(cx, wide_table_result());
        let gutter = cx
            .debug_bounds("HSCROLL")
            .expect("horizontal scrollbar bounds");
        let start = gutter.center();
        let end = gpui::point(gutter.origin.x + gutter.size.width - px(2.), start.y);

        cx.simulate_mouse_down(start, gpui::MouseButton::Left, gpui::Modifiers::none());
        draw_result_view_frame(window, &mut cx);
        cx.simulate_mouse_move(end, Some(gpui::MouseButton::Left), gpui::Modifiers::none());
        cx.simulate_mouse_up(end, gpui::MouseButton::Left, gpui::Modifiers::none());
        draw_result_view_frame(window, &mut cx);

        view.update(&mut cx, |view, _cx| {
            let offset = f32::from(view.h_scroll.offset().x);
            let bounds_width = f32::from(view.h_scroll.bounds().size.width);
            assert!(
                offset < 0.0,
                "dragging the horizontal scrollbar should move the horizontal offset; offset={offset}, bounds_width={bounds_width}, total_width={}",
                view.total_width
            );
            assert!(view.scroll_drag.is_none());
        });
    }

    #[gpui::test]
    fn visual_scrollbar_drag_stops_when_left_button_is_released(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) = framed_plain_result_window(cx, wide_table_result());
        let gutter = cx
            .debug_bounds("HSCROLL")
            .expect("horizontal scrollbar bounds");
        let start = gutter.center();
        let move_after_release = gpui::point(gutter.origin.x + gutter.size.width - px(2.), start.y);

        cx.simulate_mouse_down(start, gpui::MouseButton::Left, gpui::Modifiers::none());
        draw_result_view_frame(window, &mut cx);

        let offset_after_press = view.update(&mut cx, |view, _cx| {
            assert!(
                view.scroll_drag.is_some(),
                "mouse down on the scrollbar should start a drag"
            );
            f32::from(view.h_scroll.offset().x)
        });

        cx.simulate_mouse_move(move_after_release, None, gpui::Modifiers::none());
        draw_result_view_frame(window, &mut cx);

        view.update(&mut cx, |view, _cx| {
            assert!(
                view.scroll_drag.is_none(),
                "the scrollbar drag should stop as soon as GPUI reports no pressed left button"
            );
            assert_eq!(f32::from(view.h_scroll.offset().x), offset_after_press);
        });
    }

    #[gpui::test]
    fn visual_gutter_click_selects_entire_row(cx: &mut gpui::TestAppContext) {
        let (_window, view, mut cx) = table_backed_result_window(cx);
        let row_number = debug_center(&mut cx, "GUTTER-1");

        cx.simulate_click(row_number, gpui::Modifiers::none());

        view.update(&mut cx, |view, _cx| {
            assert!(view.selected_rows.contains(&1));
            assert_eq!(view.selected_rows.len(), 1);
            assert_eq!(view.selected_cell, None);
            assert!(view.selected_cell_range_contains(1, 1, 0));
            assert!(view.selected_cell_range_contains(1, 1, 1));
            assert!(!view.selected_cell_range_contains(0, 0, 0));
        });
    }

    fn open_gutter_menu_and_click(
        window: gpui::WindowHandle<ResultView>,
        cx: &mut gpui::VisualTestContext,
        gutter_selector: &'static str,
        menu_selector: &'static str,
    ) {
        let gutter_center = debug_center(cx, gutter_selector);
        cx.simulate_event(gpui::MouseDownEvent {
            position: gutter_center,
            button: gpui::MouseButton::Right,
            modifiers: gpui::Modifiers::none(),
            click_count: 1,
            first_mouse: false,
        });
        draw_result_view(window, cx);
        let menu_center = debug_center(cx, menu_selector);
        cx.simulate_click(menu_center, gpui::Modifiers::none());
        draw_result_view(window, cx);
    }

    #[gpui::test]
    fn visual_gutter_context_menu_adds_clones_and_deletes_rows(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) = table_backed_result_window(cx);

        open_gutter_menu_and_click(window, &mut cx, "GUTTER-1", "MENU_ITEM-Add Row");
        view.update(&mut cx, |view, _cx| {
            assert_eq!(
                view.added_rows.len(),
                1,
                "Add Row should create one pending row"
            );
            assert_eq!(
                view.display_row_entries(),
                vec![
                    ResultDisplayRow::Loaded(0),
                    ResultDisplayRow::Loaded(1),
                    ResultDisplayRow::Added(0),
                    ResultDisplayRow::Loaded(2),
                ],
                "Add Row should place the pending row directly after the clicked row"
            );
            assert!(
                view.cell_edit.as_ref().is_some_and(|edit| {
                    edit.abs_idx == view.loaded_row_count()
                        && edit.col_idx == 0
                        && matches!(edit.target, CellEditTarget::Added(0))
                }),
                "Add Row should immediately open an editor in the new pending row"
            );
        });

        window
            .update(&mut cx, |view, window, cx| {
                let editor = view
                    .cell_edit
                    .as_ref()
                    .expect("pending row editor")
                    .editor
                    .clone();
                editor.update(cx, |editor, cx| {
                    editor.set_text("10", window, cx);
                });
                assert!(view.commit_cell_edit(window, cx));
                assert_eq!(
                    view.added_rows.first().and_then(|row| row.first()),
                    Some(&CellValue::Text("10".to_string()))
                );
            })
            .unwrap();

        open_gutter_menu_and_click(window, &mut cx, "GUTTER-1", "MENU_ITEM-Clone Row");
        view.update(&mut cx, |view, _cx| {
            assert_eq!(
                view.added_rows.len(),
                2,
                "Clone Row should create another pending row"
            );
            assert_eq!(
                view.added_rows.last(),
                Some(&vec![
                    CellValue::Text("2".to_string()),
                    CellValue::Text("Bob".to_string())
                ])
            );
            assert_eq!(
                view.display_row_entries(),
                vec![
                    ResultDisplayRow::Loaded(0),
                    ResultDisplayRow::Loaded(1),
                    ResultDisplayRow::Added(0),
                    ResultDisplayRow::Added(1),
                    ResultDisplayRow::Loaded(2),
                ],
                "Clone Row should place the cloned row under the clicked row's pending group"
            );
        });

        open_gutter_menu_and_click(window, &mut cx, "GUTTER-4", "MENU_ITEM-Delete Row");
        view.update(&mut cx, |view, _cx| {
            assert!(
                view.deleted_rows.contains(&2),
                "Delete Row should mark the right-clicked loaded row"
            );
        });
    }

    #[gpui::test]
    fn visual_copy_selection_uses_selected_copy_format(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) = table_backed_result_window(cx);
        let row_number = debug_center(&mut cx, "GUTTER-1");

        cx.simulate_click(row_number, gpui::Modifiers::none());
        draw_result_view(window, &mut cx);

        view.update(&mut cx, |view, cx| {
            view.copy_format = CopyFormat::Csv;
            view.copy_selected_to_clipboard(cx);
        });

        let copied = cx
            .read_from_clipboard()
            .and_then(|clipboard| clipboard.text())
            .expect("copy selection should write text to clipboard");
        assert_eq!(copied, "id,name\n2,Bob\n");
    }

    #[gpui::test]
    fn copy_selection_uses_insert_copy_format(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) = table_backed_result_window(cx);
        let row_number = debug_center(&mut cx, "GUTTER-1");

        cx.simulate_click(row_number, gpui::Modifiers::none());
        draw_result_view(window, &mut cx);

        view.update(&mut cx, |view, cx| {
            view.copy_format = CopyFormat::Insert;
            view.copy_selected_to_clipboard(cx);
        });

        let copied = cx
            .read_from_clipboard()
            .and_then(|clipboard| clipboard.text())
            .expect("copy selection should write INSERT text to clipboard");
        assert_eq!(
            copied,
            "INSERT INTO `users` (`id`, `name`) VALUES ('2', 'Bob');\n"
        );
    }

    #[gpui::test]
    fn copy_selection_uses_multi_insert_copy_format(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) = table_backed_result_window(cx);
        let first_row = debug_center(&mut cx, "GUTTER-0");
        let second_row = debug_center(&mut cx, "GUTTER-1");

        cx.simulate_click(first_row, gpui::Modifiers::none());
        cx.simulate_click(second_row, gpui::Modifiers::shift());
        draw_result_view(window, &mut cx);

        view.update(&mut cx, |view, cx| {
            view.copy_format = CopyFormat::MultiInsert;
            view.copy_selected_to_clipboard(cx);
        });

        let copied = cx
            .read_from_clipboard()
            .and_then(|clipboard| clipboard.text())
            .expect("copy selection should write multi INSERT text to clipboard");
        assert_eq!(
            copied,
            "INSERT INTO `users` (`id`, `name`) VALUES\n  ('1', 'Alice'),\n  ('2', 'Bob');\n"
        );
    }

    #[gpui::test]
    fn context_row_action_selection_overrides_previous_selection(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let result = QueryResult {
            columns: vec!["id".to_string(), "name".to_string()],
            rows: vec![
                vec![Some("1".to_string()), Some("Alice".to_string())],
                vec![Some("2".to_string()), Some("Bob".to_string())],
            ],
            rows_affected: 0,
            execution_time_ms: 0,
        };
        let window = cx.add_window(|_window, cx| {
            let mut view = ResultView::new("test", cx);
            view.set_result(result, cx);
            view
        });
        window
            .update(cx, |view, _window, _cx| {
                view.selected_rows.insert(0);
                view.selected_cell_range = Some(((0, 0), (1, 1)));

                view.select_row_for_context_action(1, 1);

                assert_eq!(
                    view.selected_rows.iter().copied().collect::<Vec<_>>(),
                    vec![1]
                );
                assert_eq!(view.selected_cell, Some((1, 1)));
                assert_eq!(view.last_selected_row, Some(1));
                assert!(view.selected_cell_range.is_none());
            })
            .unwrap();
    }

    #[gpui::test]
    fn cell_range_selection_uses_visible_display_order(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let result = QueryResult {
            columns: vec!["id".to_string(), "name".to_string()],
            rows: vec![
                vec![Some("1".to_string()), Some("Alice".to_string())],
                vec![Some("2".to_string()), Some("Bob".to_string())],
                vec![Some("3".to_string()), Some("Claire".to_string())],
            ],
            rows_affected: 0,
            execution_time_ms: 0,
        };
        let window = cx.add_window(|_window, cx| {
            let mut view = ResultView::new("test", cx);
            view.set_result(result, cx);
            view
        });
        window
            .update(cx, |view, _window, _cx| {
                view.filtered_display_order = vec![2, 0];
                view.added_rows.push(vec![CellValue::Null, CellValue::Null]);

                view.select_cell_from_click(2, 0, 0, false, false);
                view.select_cell_from_click(3, 2, 1, true, false);

                assert!(view.selected_cell_range_contains(2, 0, 0));
                assert!(view.selected_cell_range_contains(0, 1, 1));
                assert!(view.selected_cell_range_contains(3, 2, 1));
                assert!(!view.selected_cell_range_contains(1, 3, 1));
            })
            .unwrap();
    }

    #[gpui::test]
    fn row_range_selection_includes_added_rows(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let result = QueryResult {
            columns: vec!["id".to_string()],
            rows: vec![vec![Some("1".to_string())], vec![Some("2".to_string())]],
            rows_affected: 0,
            execution_time_ms: 0,
        };
        let window = cx.add_window(|_window, cx| {
            let mut view = ResultView::new("test", cx);
            view.set_result(result, cx);
            view
        });
        window
            .update(cx, |view, _window, _cx| {
                view.filtered_display_order = vec![1];
                view.added_rows.push(vec![CellValue::Null]);

                view.select_row_range(0, 1);

                assert!(view.selected_rows.contains(&1));
                assert!(view.selected_rows.contains(&2));
                assert_eq!(view.selected_rows.len(), 2);
            })
            .unwrap();
    }

    #[gpui::test]
    fn build_pending_statements_emits_delete_update_insert_in_order(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) = table_backed_result_window(cx);
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, cx| {
            view.primary_key_columns = Some(vec!["id".to_string()]);
            view.pending_edits.insert(
                (0, 1),
                PendingEdit {
                    original: Some("Alice".to_string()),
                    new_value: CellValue::Text("Alicia".to_string()),
                },
            );
            view.deleted_rows.insert(2);
            view.added_rows.push(vec![
                CellValue::Text("9".to_string()),
                CellValue::Text("Zed".to_string()),
            ]);

            let statements = view
                .build_pending_statements(cx)
                .expect("statements should build with a primary key");
            assert_eq!(statements.len(), 3);
            assert!(
                statements[0].contains("DELETE FROM `users` WHERE `id` = 3"),
                "first statement should be the DELETE: {}",
                statements[0]
            );
            assert!(
                statements[1].contains("UPDATE `users` SET `name` = 'Alicia' WHERE `id` = 1"),
                "second statement should be the UPDATE: {}",
                statements[1]
            );
            assert!(
                statements[2].contains("INSERT INTO `users` (`id`, `name`) VALUES (9, 'Zed')"),
                "third statement should be the INSERT: {}",
                statements[2]
            );
        });
    }

    #[gpui::test]
    fn build_pending_statements_without_primary_key_reports_error(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) = table_backed_result_window(cx);
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, cx| {
            view.primary_key_columns = Some(Vec::new());
            view.pending_edits.insert(
                (0, 1),
                PendingEdit {
                    original: Some("Alice".to_string()),
                    new_value: CellValue::Text("Alicia".to_string()),
                },
            );
            assert!(view.build_pending_statements(cx).is_err());
        });
    }

    #[gpui::test]
    fn transaction_mode_toggle_switches_between_auto_and_manual(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) = table_backed_result_window(cx);
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, cx| {
            assert_eq!(view.transaction_mode, TransactionMode::Auto);
            view.toggle_transaction_mode(cx);
            assert_eq!(view.transaction_mode, TransactionMode::Manual);
            view.toggle_transaction_mode(cx);
            assert_eq!(view.transaction_mode, TransactionMode::Auto);
        });
    }

    #[gpui::test]
    fn manual_submit_stages_statements_without_executing(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) = table_backed_result_window(cx);
        draw_result_view(window, &mut cx);
        view.update_in(&mut cx, |view, window, cx| {
            view.transaction_mode = TransactionMode::Manual;
            view.primary_key_columns = Some(vec!["id".to_string()]);
            view.pending_edits.insert(
                (0, 1),
                PendingEdit {
                    original: Some("Alice".to_string()),
                    new_value: CellValue::Text("Alicia".to_string()),
                },
            );
            view.submit_pending_edits(window, cx);

            // Manual Submit stages the SQL but keeps the buffered edit and does
            // not clear it, so the grid still shows the pending change.
            assert_eq!(view.staged_statements.len(), 1);
            assert!(view.pending_edits.contains_key(&(0, 1)));
        });
    }

    #[gpui::test]
    fn rollback_clears_staged_and_pending_changes(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) = table_backed_result_window(cx);
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, cx| {
            view.staged_statements.push("BEGIN".to_string());
            view.pending_edits.insert(
                (0, 1),
                PendingEdit {
                    original: Some("Alice".to_string()),
                    new_value: CellValue::Text("Alicia".to_string()),
                },
            );
            view.rollback_transaction(cx);
            assert!(view.staged_statements.is_empty());
            assert!(view.pending_edits.is_empty());
        });
    }

    #[gpui::test]
    fn goto_row_selects_and_focuses_target_row(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) = table_backed_result_window(cx);
        draw_result_view(window, &mut cx);
        view.update_in(&mut cx, |view, window, cx| {
            view.open_goto_row(window, cx);
        });
        view.update_in(&mut cx, |view, window, cx| {
            if let Some(editor) = view.goto_row_editor.clone() {
                editor.update(cx, |editor, cx| editor.set_text("2", window, cx));
            }
        });
        view.update(&mut cx, |view, cx| {
            view.confirm_goto_row(cx);
            // Display row 2 is the second row (Bob), absolute index 1.
            assert!(view.selected_rows.contains(&1));
            assert!(!view.goto_row_visible);
        });
    }

    #[gpui::test]
    fn copy_aggregation_writes_column_summary_to_clipboard(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) = table_backed_result_window(cx);
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, cx| {
            view.selected_cell = Some((0, 0));
            view.copy_aggregation_to_clipboard(cx);
        });
        let copied = cx
            .read_from_clipboard()
            .and_then(|clipboard| clipboard.text())
            .expect("copy aggregation should write text to clipboard");
        assert!(
            copied.starts_with("id"),
            "summary should name the column: {copied}"
        );
        assert!(
            copied.contains("COUNT 3"),
            "summary should include the count: {copied}"
        );
        assert!(
            copied.contains("SUM 6"),
            "summary should include the sum: {copied}"
        );
    }

    #[gpui::test]
    fn find_filter_rows_hides_non_matching_rows(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) = table_backed_result_window(cx);
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, _cx| {
            view.find_filter_rows = true;
            view.find_query = Some("bob".to_string());
            view.recompute_local_filter_inner();
            assert_eq!(view.filtered_display_order, vec![1]);
        });
    }

    #[gpui::test]
    fn quick_filter_by_cell_narrows_visible_rows(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) = table_backed_result_window(cx);
        draw_result_view(window, &mut cx);
        view.update_in(&mut cx, |view, window, cx| {
            view.apply_quick_filter(1, "Bob".to_string(), false, window, cx);
        });
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, _cx| {
            assert!(view.local_filter_visible);
            assert_eq!(view.filtered_display_order, vec![1]);
        });
    }

    #[gpui::test]
    fn quick_filter_exclude_removes_matching_rows(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) = table_backed_result_window(cx);
        draw_result_view(window, &mut cx);
        view.update_in(&mut cx, |view, window, cx| {
            view.apply_quick_filter(1, "Bob".to_string(), true, window, cx);
        });
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, _cx| {
            assert_eq!(view.filtered_display_order.len(), 2);
            assert!(!view.filtered_display_order.contains(&1));
        });
    }

    #[gpui::test]
    fn value_editor_writes_edit_into_pending_buffer(cx: &mut gpui::TestAppContext) {
        let (_window, view, mut cx) = table_backed_result_window(cx);
        view.update_in(&mut cx, |view, window, cx| {
            view.selected_cell = Some((0, 1));
            view.value_editor_open = true;
            view.sync_value_editor(window, cx);
            let editor = view.value_editor.clone().expect("value editor created");
            editor.update(cx, |editor, cx| editor.set_text("Updated", window, cx));
            view.commit_value_editor(window, cx);
        });
        view.update(&mut cx, |view, _cx| {
            assert!(!view.value_editor_open);
            let edit = view
                .pending_edits
                .get(&(0, 1))
                .expect("pending edit buffered");
            assert_eq!(edit.new_value, CellValue::Text("Updated".to_string()));
        });
    }

    #[gpui::test]
    fn value_editor_pretty_prints_json(cx: &mut gpui::TestAppContext) {
        let (_window, view, mut cx) = table_backed_result_window(cx);
        view.update_in(&mut cx, |view, window, cx| {
            view.selected_cell = Some((0, 1));
            view.value_editor_open = true;
            view.sync_value_editor(window, cx);
            let editor = view.value_editor.clone().expect("value editor created");
            editor.update(cx, |editor, cx| {
                editor.set_text(r#"{"a":1,"b":2}"#, window, cx)
            });
            assert!(view.value_editor_text_is_json(cx));
            view.format_value_editor_json(window, cx);
            let text = view
                .value_editor
                .as_ref()
                .expect("value editor present")
                .read(cx)
                .text(cx);
            assert!(text.contains('\n'), "expected pretty-printed JSON: {text}");
        });
    }

    #[gpui::test]
    fn toggle_transpose_switches_view(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) = table_backed_result_window(cx);
        view.update(&mut cx, |view, cx| {
            assert!(!view.transposed);
            view.toggle_transpose(cx);
            assert!(view.transposed);
        });
        draw_result_view(window, &mut cx);
        assert!(
            cx.debug_bounds("TRANSPOSE_VIEW").is_some(),
            "transposed view should render"
        );
    }

    #[gpui::test]
    fn move_column_reorders_visible_columns(cx: &mut gpui::TestAppContext) {
        let (_window, view, mut cx) = table_backed_result_window(cx);
        view.update(&mut cx, |view, cx| {
            assert_eq!(view.visible_columns, vec![0, 1]);
            view.move_column(0, 1, cx);
            assert_eq!(view.column_order, vec![1, 0]);
            assert_eq!(view.visible_columns, vec![1, 0]);
        });
    }

    #[gpui::test]
    fn reset_view_restores_defaults(cx: &mut gpui::TestAppContext) {
        let (_window, view, mut cx) = table_backed_result_window(cx);
        view.update(&mut cx, |view, cx| {
            view.hidden_columns.insert(1);
            view.sort_columns = vec![SortColumn {
                col_idx: 0,
                ascending: false,
            }];
            view.local_filters = vec!["x".to_string(), String::new()];
            view.transposed = true;
            view.move_column(0, 1, cx);
            view.reset_view(cx);
            assert!(view.hidden_columns.is_empty());
            assert!(view.sort_columns.is_empty());
            assert!(view.local_filters.is_empty());
            assert!(!view.transposed);
            assert_eq!(view.column_order, vec![0, 1]);
            assert_eq!(view.visible_columns, vec![0, 1]);
        });
    }

    #[gpui::test]
    fn history_search_filters_entries(cx: &mut gpui::TestAppContext) {
        let (_window, view, mut cx) = table_backed_result_window(cx);
        view.update(&mut cx, |view, _cx| {
            view.query_history = vec![
                "select * from users".to_string(),
                "select * from orders".to_string(),
                "update users set x = 1".to_string(),
            ];
            assert_eq!(view.filtered_history().len(), 3);

            view.history_search = "users".to_string();
            let filtered = view.filtered_history();
            assert_eq!(filtered.len(), 2);
            assert!(filtered.iter().all(|(_, sql)| sql.contains("users")));

            // The original index is preserved so a click maps to the right entry.
            view.history_search = "orders".to_string();
            assert_eq!(
                view.filtered_history(),
                vec![(1, "select * from orders".to_string())]
            );
        });
    }
}
