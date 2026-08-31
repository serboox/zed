use crate::store::DatabaseStore;
use db_client::{
    ConnectionId, DatabaseDriver,
    schema::{ColumnInfo, FkInfo, QueryResult, QueryTiming},
};
use editor::{CompletionContext, CompletionProvider, Editor, EditorEvent, MinimapVisibility};
use gpui::{
    Anchor, AnyElement, App, ClipboardItem, Context, ElementId, Entity, EventEmitter, FocusHandle,
    Focusable, FontWeight, IntoElement, KeyDownEvent, ListSizingBehavior, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Render, ScrollWheelEvent, SharedString,
    StatefulInteractiveElement, Subscription, Task, TextStyleRefinement, UniformListScrollHandle,
    WeakEntity, Window, actions, uniform_list,
};
use language::language_settings::SoftWrap;
use language::{Buffer, CodeLabel, ToOffset};
use project::{Completion, CompletionDisplayOptions, CompletionResponse, CompletionSource};
use std::io::Write;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use ui::{
    Button, ButtonCommon, ButtonStyle, Checkbox, Chip, Color, CommonAnimationExt, ContextMenu,
    CopyButton, Divider, Icon, IconButton, IconName, IconSize, Label, LabelSize, PopoverMenu,
    ScrollableHandle, Tooltip, cyberpunk, prelude::*, right_click_menu,
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
        /// Sets the selected cell to an explicit empty string.
        SetEmptyValue,
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
        /// Toggles heatmap tinting of numeric cells by their value's position in the column's range.
        ToggleHeatmap,
        /// Resets column order, hidden columns, sort, filters, and transpose to defaults.
        ResetView,
        /// Opens the Export Data dialog (format, headers, DDL, transpose, destination).
        OpenExportDialog,
        /// Opens or closes the chart view for the current result.
        ToggleChart,
        /// Pins this result tab so the next query opens a new tab instead of reusing it.
        TogglePinResult,
        /// Moves the selected cell to the first column of its row.
        SelectRowStart,
        /// Moves the selected cell to the last column of its row.
        SelectRowEnd,
        /// Moves the selected cell to the first cell of the grid.
        SelectFirstCell,
        /// Moves the selected cell to the last cell of the grid.
        SelectLastCell,
        /// Moves the selected cell up by one page of rows.
        SelectPageUp,
        /// Moves the selected cell down by one page of rows.
        SelectPageDown,
    ]
);

// PageUp/PageDown move the selection by this many rows. Not derived from the
// viewport's actual visible row count (UniformListScrollHandle does not expose
// it precomputed) — a fixed jump is simpler and matches common grid editors'
// "big enough to be useful" page-navigation feel.
const PAGE_ROW_JUMP: usize = 20;

// Projects a lat/lon pair to a normalized (x, y) fraction in [0, 1] using an
// equirectangular projection, for plotting on the geo-viewer's offline scatter
// map. Returns None for non-finite or out-of-range coordinates so callers can
// skip them instead of plotting nonsense off the visible canvas.
fn project_lat_lon(lat: f64, lon: f64) -> Option<(f32, f32)> {
    if !lat.is_finite()
        || !lon.is_finite()
        || !(-90.0..=90.0).contains(&lat)
        || !(-180.0..=180.0).contains(&lon)
    {
        return None;
    }
    let x = ((lon + 180.0) / 360.0) as f32;
    let y = ((90.0 - lat) / 180.0) as f32;
    Some((x, y))
}

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

// `anyhow::Error`'s `Display` (used by `.to_string()`) prints only the
// outermost context message, discarding the wrapped driver error underneath
// (e.g. the actual message sqlx/scylla got back from the database). `Debug`
// prints the full "Caused by: 0: ... 1: ..." chain instead, which is what a
// user needs to diagnose a real failure rather than a generic
// "Query execution failed".
pub(crate) fn format_query_error(err: &anyhow::Error) -> String {
    format!("{err:?}")
}

/// The pieces of a failed query the results panel presents separately: the one
/// sentence the database itself said, the machine identifiers that belong
/// beside it, and the verbatim driver output kept as secondary detail.
struct QueryErrorParts {
    /// The deepest cause in the chain with every framing layer peeled off.
    headline: String,
    /// The driver's own numeric code, e.g. MySQL's 1146.
    vendor_code: Option<String>,
    /// The five-character standard error class, e.g. 42S02.
    sqlstate: Option<String>,
    /// The unmodified text, present only when it says more than `headline`
    /// already does, so the panel never shows the same sentence twice.
    detail: Option<String>,
}

const SQLSTATE_LEN: usize = 5;

/// The failure sentence is bounded so a pathological multi-kilobyte message
/// cannot push the driver detail off the panel; the rest scrolls in place.
const ERROR_MESSAGE_MAX_LINES: usize = 8;

// sqlx wraps the database's own words in its own framing before anyhow adds the
// "Query execution failed" context on top. Neither layer tells a reader
// anything the message below it does not.
const DRIVER_NOISE_PREFIXES: [&str; 2] = [
    "error returned from database: ",
    "error communicating with database: ",
];

fn parse_query_error(text: &str) -> QueryErrorParts {
    let verbatim = text.trim();
    let causes = caused_by_entries(text);
    let deepest = causes.last().copied().unwrap_or(verbatim);
    let stripped = strip_driver_framing(deepest);
    let headline = if stripped.is_empty() {
        verbatim.to_string()
    } else {
        stripped
    };
    let (vendor_code, sqlstate) = extract_error_codes(text);
    // A single-line original adds nothing next to the headline: the only thing
    // stripped from it is the code pair, which the header shows as chips.
    let detail =
        (verbatim.lines().count() > 1 && verbatim != headline).then(|| verbatim.to_string());
    QueryErrorParts {
        headline,
        vendor_code,
        sqlstate,
        detail,
    }
}

// The lines anyhow's `Debug` prints under "Caused by:", outermost first, with
// the `N: ` numbering it adds for chains longer than one link removed.
fn caused_by_entries(text: &str) -> Vec<&str> {
    let mut lines = text.lines();
    if !lines.any(|line| line.trim() == "Caused by:") {
        return Vec::new();
    }
    lines
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(strip_chain_index)
        .collect()
}

// anyhow numbers the links of a chain from zero, so an index is one or two
// digits. Longer digit runs are left alone: a driver message can legitimately
// open with its own four-digit error number, and eating that would hide it.
const MAX_CHAIN_INDEX_DIGITS: usize = 2;

fn strip_chain_index(line: &str) -> &str {
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 || digits > MAX_CHAIN_INDEX_DIGITS {
        return line;
    }
    line.get(digits..)
        .and_then(|rest| rest.strip_prefix(": "))
        .unwrap_or(line)
}

// MySQL and MariaDB drivers report `<code> (<sqlstate>): <message>`; Postgres
// and SQLite report neither. The pair is searched for anywhere in the chain so
// it is found whether the driver put it in the outermost message or in a nested
// cause. Both the digits in front of the parentheses and the colon after them
// are required, because a message can legitimately contain a five-character
// parenthesised word.
fn extract_error_codes(text: &str) -> (Option<String>, Option<String>) {
    for (open, _) in text.char_indices().filter(|(_, ch)| *ch == '(') {
        let Some(rest) = text.get(open + 1..) else {
            continue;
        };
        let sqlstate: String = rest
            .chars()
            .take(SQLSTATE_LEN)
            .take_while(|ch| ch.is_ascii_digit() || ch.is_ascii_uppercase())
            .collect();
        if sqlstate.len() != SQLSTATE_LEN {
            continue;
        }
        if !rest
            .get(SQLSTATE_LEN..)
            .is_some_and(|tail| tail.starts_with("):"))
        {
            continue;
        }
        let Some(before) = text.get(..open) else {
            continue;
        };
        let digits: Vec<char> = before
            .trim_end()
            .chars()
            .rev()
            .take_while(char::is_ascii_digit)
            .collect();
        if digits.is_empty() {
            continue;
        }
        return (Some(digits.into_iter().rev().collect()), Some(sqlstate));
    }
    (None, None)
}

// Peels the layers the driver stack puts in front of the database's own words:
// sqlx's wrapper text and the `<code> (<sqlstate>): ` prefix, which the panel
// shows as chips instead of repeating inside the sentence.
fn strip_driver_framing(message: &str) -> String {
    let mut message = message.trim();
    loop {
        let before = message;
        for prefix in DRIVER_NOISE_PREFIXES {
            if let Some(rest) = message.strip_prefix(prefix) {
                message = rest.trim_start();
            }
        }
        if let Some(rest) = strip_code_prefix(message) {
            message = rest.trim_start();
        }
        if message == before {
            return message.to_string();
        }
    }
}

fn strip_code_prefix(message: &str) -> Option<&str> {
    let digits = message.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let rest = message.get(digits..)?.trim_start().strip_prefix('(')?;
    let sqlstate: String = rest
        .chars()
        .take(SQLSTATE_LEN)
        .take_while(|ch| ch.is_ascii_digit() || ch.is_ascii_uppercase())
        .collect();
    if sqlstate.len() != SQLSTATE_LEN {
        return None;
    }
    rest.get(SQLSTATE_LEN..)?.strip_prefix("):")
}

// A lively rotating loading indicator. A short rotation period reads as active
// work; kept in one place so every spinner in the grid looks the same.
fn loading_spinner(id: impl Into<ElementId>, size: IconSize) -> impl IntoElement {
    Icon::new(IconName::ArrowCircle)
        .size(size)
        .color(Color::Hint)
        .with_keyed_rotate_animation(id, 1)
}

/// One visual segment of the query-timing breakdown bar: a measured phase's
/// duration plus the fraction of the bar's width it should occupy. Kept
/// separate from rendering so the width math is unit-testable without a
/// GPUI window.
struct QueryTimingSegment {
    label: &'static str,
    ms: u64,
    fraction: f32,
}

/// Splits a measured `QueryTiming` into the segments the breakdown bar draws,
/// in phase order. Only includes `streaming_ms` when the provider actually
/// measured it (writes and empty reads leave it unset) -- a phase that was
/// never measured must never appear, not even as a zero-width segment.
/// Returns `None` when the total is zero, since a bar with nothing to show
/// as a fraction would be meaningless.
fn query_timing_segments(timing: &QueryTiming) -> Option<Vec<QueryTimingSegment>> {
    let total = timing.total_ms();
    if total == 0 {
        return None;
    }
    let fraction = |ms: u64| ms as f32 / total as f32;
    let mut segments = vec![
        QueryTimingSegment {
            label: "Waiting for connection",
            ms: timing.pool_wait_ms,
            fraction: fraction(timing.pool_wait_ms),
        },
        QueryTimingSegment {
            label: "Executing",
            ms: timing.execute_ms,
            fraction: fraction(timing.execute_ms),
        },
    ];
    if let Some(streaming_ms) = timing.streaming_ms {
        segments.push(QueryTimingSegment {
            label: "Streaming rows",
            ms: streaming_ms,
            fraction: fraction(streaming_ms),
        });
    }
    Some(segments)
}

/// Tooltip text naming every measured phase, e.g.
/// "Waiting for connection: 2ms · Executing: 8ms · Streaming rows: 15ms".
fn format_query_timing_tooltip(segments: &[QueryTimingSegment]) -> String {
    segments
        .iter()
        .map(|segment| format!("{}: {}ms", segment.label, segment.ms))
        .collect::<Vec<_>>()
        .join(" · ")
}

/// Color for the Nth segment of the breakdown bar, cycling if there were ever
/// more segments than colors. Muted-to-accent progression reads as "waiting"
/// (passive) through "streaming" (active), matching phase order.
fn query_timing_segment_color(index: usize) -> Color {
    const COLORS: [Color; 3] = [Color::Muted, Color::Accent, Color::Success];
    COLORS[index % COLORS.len()]
}

/// The small stacked-bar affordance next to a query's elapsed time, when the
/// provider measured a phase breakdown. Absent entirely (not a plain bar with
/// one segment) when `result.timing` is `None`, so a provider that only ever
/// measures the total never implies a breakdown that does not exist.
fn render_query_timing_bar(timing: &QueryTiming, cx: &App) -> Option<AnyElement> {
    let segments = query_timing_segments(timing)?;
    let tooltip = format_query_timing_tooltip(&segments);
    Some(
        div()
            .id("query-timing-bar")
            .flex()
            .flex_row()
            .h(px(8.))
            .w(px(28.))
            .rounded_sm()
            .overflow_hidden()
            .children(segments.iter().enumerate().map(|(index, segment)| {
                div()
                    .h_full()
                    .w(relative(segment.fraction))
                    .bg(query_timing_segment_color(index).color(cx))
            }))
            .tooltip(Tooltip::text(tooltip))
            .into_any_element(),
    )
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

// Parses the leading "YYYY-MM-DD" of a date or datetime string. Tolerant of a
// trailing time portion (datetime values keep it after a space); returns None
// for anything shorter or not matching the calendar-date grammar, so a blank
// or malformed cell falls back to the popup's caller-supplied default instead
// of panicking or silently misparsing.
fn parse_date_prefix(text: &str) -> Option<time::Date> {
    let date_part = text.get(0..10)?;
    let format = time::macros::format_description!("[year]-[month]-[day]");
    time::Date::parse(date_part, &format).ok()
}

// Formats a date as "YYYY-MM-DD", matching the format the grid already uses
// for DATE columns and the date portion of DATETIME columns.
fn format_date_ymd(date: time::Date) -> String {
    let format = time::macros::format_description!("[year]-[month]-[day]");
    date.format(&format).unwrap_or_default()
}

// Distinct, already-loaded values of one column, offered as completions while
// editing a cell of that column. Built entirely from `result.rows` already in
// memory (no query), case-sensitive, capped so a large result set can't turn
// every keystroke into an unbounded scan.
fn distinct_column_values(result: &QueryResult, col_idx: usize) -> Vec<String> {
    const MAX_VALUES: usize = 50;
    let mut seen = std::collections::HashSet::new();
    let mut values = Vec::new();
    for row in &result.rows {
        let Some(text) = row.get(col_idx).and_then(|cell| cell.as_deref()) else {
            continue;
        };
        if text.is_empty() {
            continue;
        }
        if seen.insert(text.to_string()) {
            values.push(text.to_string());
            if values.len() >= MAX_VALUES {
                break;
            }
        }
    }
    values.sort();
    values
}

struct CellValueCompletionProvider {
    values: Vec<String>,
}

impl CompletionProvider for CellValueCompletionProvider {
    fn completions(
        &self,
        buffer: &Entity<Buffer>,
        buffer_position: language::Anchor,
        _trigger: CompletionContext,
        _window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> Task<anyhow::Result<Vec<CompletionResponse>>> {
        let snapshot = buffer.read(cx).snapshot();
        let offset = buffer_position.to_offset(&snapshot);
        let query: String = snapshot
            .text_for_range(0..offset)
            .collect::<String>()
            .to_lowercase();
        let replace_range = snapshot.anchor_before(0)..snapshot.anchor_after(offset);
        let completions = self
            .values
            .iter()
            .filter(|value| value.to_lowercase().starts_with(&query))
            .map(|value| Completion {
                replace_range: replace_range.clone(),
                new_text: value.clone(),
                label: CodeLabel::plain(value.clone(), None),
                documentation: None,
                source: CompletionSource::Custom,
                icon_path: None,
                icon_color: None,
                match_start: None,
                snippet_deduplication_key: None,
                insert_text_mode: None,
                confirm: None,
                group: None,
            })
            .collect();
        Task::ready(Ok(vec![CompletionResponse {
            completions,
            display_options: CompletionDisplayOptions::default(),
            is_incomplete: false,
        }]))
    }

    fn is_completion_trigger(
        &self,
        _buffer: &Entity<Buffer>,
        _position: language::Anchor,
        _text: &str,
        _trigger_in_words: bool,
        _cx: &mut Context<Editor>,
    ) -> bool {
        true
    }
}

fn is_truthy_bool(s: &str) -> bool {
    matches!(s.to_lowercase().as_str(), "1" | "true" | "t" | "yes" | "on")
}

fn bool_cell_display(value: &CellValue) -> (String, Color) {
    match value {
        CellValue::Null => (NULL_MARKER.to_string(), Color::Muted),
        CellValue::Default => (DEFAULT_MARKER.to_string(), Color::Muted),
        CellValue::Text(s) => {
            if is_truthy_bool(s) {
                ("true".to_string(), Color::Default)
            } else {
                ("false".to_string(), Color::Default)
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

// Decides whether a column is numeric for display (right-alignment). A known SQL
// type is authoritative; without type metadata the loaded values decide — every
// non-empty value must parse as a number and at least one value must be present.
fn column_is_numeric<'a>(
    data_type: Option<&str>,
    values: impl Iterator<Item = Option<&'a str>>,
) -> bool {
    if let Some(data_type) = data_type {
        return matches!(column_editor_kind(data_type), CellEditorKind::Numeric);
    }
    column_values_look_numeric(values)
}

fn column_values_look_numeric<'a>(values: impl Iterator<Item = Option<&'a str>>) -> bool {
    let mut saw_value = false;
    for value in values {
        match value {
            None => {}
            Some("") => {}
            Some(text) => {
                if text.parse::<f64>().is_err() {
                    return false;
                }
                saw_value = true;
            }
        }
    }
    saw_value
}

// Maps a value's position within [min, max] to 0.0-1.0 for heatmap tinting.
// When the column has no spread (min == max), every cell gets the same neutral
// mid-ramp tint rather than dividing by zero.
fn heatmap_ratio(value: f64, min: f64, max: f64) -> f32 {
    if max <= min {
        return 0.5;
    }
    (((value - min) / (max - min)) as f32).clamp(0.0, 1.0)
}

// Only a column whose declared type says boolean is shown as one. Values are
// never asked: a `mediumint` holding nothing but zeros looks exactly like a
// boolean, and answering "false" where the row holds 0 states something the
// database never said. Without a type, the number is shown as it is.
fn column_is_boolean(data_type: Option<&str>) -> bool {
    data_type
        .is_some_and(|data_type| matches!(column_editor_kind(data_type), CellEditorKind::Boolean))
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

#[derive(Clone, Copy, Debug, PartialEq)]
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

// The two ways to look at a MongoDB document result: a flat grid (documents
// projected into columns) or the documents themselves, pretty-printed. Only
// meaningful when `QueryResult::raw_documents` is populated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MongoResultView {
    Table,
    Documents,
}

// Picks the view a fresh Mongo result should open in, from its shape alone:
// a document missing a field that its siblings have produces a `None` cell
// once projected into columns (see `documents_to_query_result` in
// mongo_provider.rs), so any `None` cell means the documents aren't all
// shaped alike and read better as documents than as a table with holes.
fn default_mongo_view_for_result(result: &QueryResult) -> MongoResultView {
    let ragged = result
        .rows
        .iter()
        .any(|row| row.iter().any(|cell| cell.is_none()));
    if ragged {
        MongoResultView::Documents
    } else {
        MongoResultView::Table
    }
}

// Joins each document's pretty-printed text (already mongosh-shell-style, see
// `bson_document_pretty_text` in mongo_provider.rs) into one block, in row
// order, with a blank line between documents the way a shell session prints
// consecutive results.
fn mongo_documents_display_text(result: &QueryResult) -> String {
    match result.raw_documents.as_deref() {
        Some(documents) if !documents.is_empty() => documents.join("\n\n"),
        _ => String::new(),
    }
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

/// Whether the statement is one that comes back with rows, judged by the word it
/// starts with.
fn statement_returns_rows(sql: &str) -> bool {
    const RETURNING: [&str; 9] = [
        "SELECT", "WITH", "SHOW", "EXPLAIN", "VALUES", "TABLE", "DESCRIBE", "DESC", "PRAGMA",
    ];
    let start = sql_effective_start(sql).to_ascii_uppercase();
    RETURNING.iter().any(|word| start.starts_with(word))
}

/// Whether to report what a statement did instead of drawing a grid for it -- a
/// `CREATE`, an `ALTER`, a `GRANT`.
///
/// Having no columns is not enough on its own: a provider that learns the column
/// names from the first row has none to report when a query matched nothing, and
/// a `SELECT` answered with "statement completed" hides the very rows the reader
/// went looking for. So the statement must also be one that was never going to
/// return any. Anything unrecognised, or a result with no statement behind it,
/// keeps the grid.
fn returns_no_result_set(result: &QueryResult, sql: Option<&str>) -> bool {
    result.columns.is_empty() && sql.is_some_and(|sql| !statement_returns_rows(sql))
}

/// What to say about such a statement, as a headline and a line beneath it.
///
/// Reporting it as "0 rows" is what makes it look like a failure: a reader takes
/// that as a query that found nothing, when the truth is there was never
/// anything to find and the statement did what it was asked.
fn statement_outcome(result: &QueryResult) -> (String, String) {
    let elapsed = format!("{} ms", result.execution_time_ms);
    match result.rows_affected {
        0 => (
            "Statement completed".to_string(),
            format!("It returns no rows · {elapsed}"),
        ),
        1 => ("1 row affected".to_string(), elapsed),
        affected => (format!("{affected} rows affected"), elapsed),
    }
}

fn detect_special_result(sql: Option<&str>, columns: &[String]) -> SpecialResult {
    if let Some(sql) = sql {
        let upper = sql_effective_start(sql).to_ascii_uppercase();
        if upper.starts_with("EXPLAIN") {
            // A tree-shaped plan (EXPLAIN FORMAT=TREE, EXPLAIN ANALYZE,
            // PostgreSQL's text plan, ...) comes back as a single column of
            // indented lines. A plain tabular EXPLAIN (MySQL's default) returns
            // many columns; flattening those cells into a plan tree yields a
            // meaningless one-value-per-line list, so show it as a normal grid.
            // `columns` is only empty before a result exists.
            if columns.len() > 1 {
                return SpecialResult::None;
            }
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

// An open calendar popup for a DATE/DATETIME cell editor. It writes into the
// same free-text cell editor the text-typing path uses, so typing a date
// manually keeps working unchanged; the calendar is a convenience on top, not
// a replacement editor.
struct DatePopup {
    abs_idx: usize,
    col_idx: usize,
    is_datetime: bool,
    // The calendar page currently shown; independent of any date already
    // picked, so browsing months doesn't require a valid selection yet.
    display_year: i32,
    display_month: time::Month,
}

pub enum ResultViewEvent {
    ResultChanged,
}

// The prepared failure state: the parsed pieces plus the read-only editors that
// hold them. Text lives in editors rather than labels because a label cannot be
// selected, and reading part of an error out of the panel is the whole point.
struct QueryErrorView {
    // The exact text these editors were built for; they are rebuilt whenever a
    // different failure arrives.
    source: String,
    parts: QueryErrorParts,
    message: Entity<Editor>,
    detail: Option<Entity<Editor>>,
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
    // Per data-column flag: true for numeric columns, which render right-aligned
    // like a spreadsheet. Indexed by result column index, recomputed with layout.
    numeric_columns: Vec<bool>,
    // Per data-column flag: true for boolean-looking columns, used only as a
    // provisional hint before real column type metadata (describe_table) has
    // loaded -- column_kind_at always prefers the real type once known. Without
    // this, a boolean column renders raw "1"/"0" text and then visibly flips to
    // the check/dash icon once metadata arrives.
    boolean_columns: Vec<bool>,
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
    // An open calendar popup for a DATE/DATETIME cell, if any. At most one at a time.
    date_popup: Option<DatePopup>,
    // When true, a read-only value editor popup is shown near the selected cell,
    // displaying the full content of the selected cell.
    value_editor_open: bool,
    value_editor: Option<Entity<Editor>>,
    value_editor_size: Option<(f32, f32)>,
    value_editor_resize_drag: Option<ValueEditorResizeDrag>,
    /// A column's edge under the pointer, while it is being dragged.
    column_resize: Option<ColumnResizeDrag>,
    /// The widths the reader set by hand, by column name. Kept by name rather
    /// than by position so that running the query again, or hiding a column
    /// beside it, leaves their work alone. An address that does not fit in the
    /// width a sample of the rows suggests is the whole reason this exists.
    widths_by_hand: std::collections::HashMap<String, gpui::Pixels>,
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
    // Undo/redo stacks over the edit buffer (pending edits, deletes, added rows).
    // Each entry is the full buffer state before an editing operation; this is
    // independent of the database and is cleared on Submit/Revert.
    edit_undo_stack: Vec<EditSnapshot>,
    edit_redo_stack: Vec<EditSnapshot>,
    // While true, per-cell writes skip recording their own undo entry. Set by a
    // multi-cell operation (paste, cut, fill) that records a single entry up front
    // so the whole operation undoes at once instead of one cell at a time.
    suppress_edit_undo: bool,
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
    // When true, numeric cells paint a background tint scaled by their value's
    // position in the column's min/max range (spreadsheet-style heatmap).
    heatmap_enabled: bool,
    // Per data-column (min, max) over every loaded row, for numeric columns only
    // (None for non-numeric columns). Recomputed with layout, like numeric_columns.
    heatmap_ranges: Vec<Option<(f64, f64)>>,
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
    // The user's Table/Documents choice for a MongoDB document result (see
    // `raw_documents` on `QueryResult`). `None` picks the default from the
    // result's shape instead of a fixed value; set on toggle, reset with the
    // rest of the special-view state whenever a new result arrives.
    mongo_view_override: Option<MongoResultView>,
    // Lazily-built read-only editor showing the current result's documents
    // as pretty-printed BSON/JSON, one per row, in the Documents view.
    mongo_documents_view: Option<Entity<Editor>>,
    // The generation `mongo_documents_view` was built for, mirroring
    // `special_built_for`.
    mongo_documents_built_for: Option<u64>,
    // Bumped by `begin_request` every time a new query is dispatched into this
    // view. A caller that awaits a query's result compares the token it got
    // back against `is_current_request` before applying that result, so an
    // older, slower request that resolves after a newer one was dispatched
    // loses the race instead of overwriting the newer result.
    active_request: u64,
    // Lazily-built editors backing the failure state, kept in sync with `error`.
    error_view: Option<QueryErrorView>,
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
#[derive(Clone)]
struct PendingEdit {
    original: Option<String>,
    new_value: CellValue,
}

// A full copy of the edit buffer captured before an editing operation, so the
// operation can be undone/redone without touching the database.
#[derive(Clone)]
struct EditSnapshot {
    pending_edits: std::collections::HashMap<(usize, usize), PendingEdit>,
    deleted_rows: std::collections::HashSet<usize>,
    added_rows: Vec<Vec<CellValue>>,
    added_row_anchors: Vec<AddedRowAnchor>,
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

/// A column's edge being dragged. The width is worked out from where the drag
/// began rather than from each step, so a fast drag cannot drift.
#[derive(Clone, Copy)]
struct ColumnResizeDrag {
    display_pos: usize,
    // The data column that sat at `display_pos` when the edge was grabbed.
    // Columns can be reordered, hidden or replaced while a button is held, and
    // the position alone would then point at somebody else's column.
    data_col: usize,
    grab_x: f32,
    started_at: f32,
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
            numeric_columns: Vec::new(),
            boolean_columns: Vec::new(),
            selected_cell: None,
            selected_cell_range: None,
            cell_drag_anchor: None,
            suppress_next_cell_click: false,
            selected_rows: std::collections::HashSet::new(),
            last_selected_row: None,
            scroll_drag: None,
            cell_edit: None,
            status_message: None,
            primary_key_columns: None,
            column_infos: None,
            fk_columns: std::collections::HashMap::new(),
            enum_popup: None,
            date_popup: None,
            value_editor_open: false,
            value_editor: None,
            value_editor_size: None,
            value_editor_resize_drag: None,
            column_resize: None,
            widths_by_hand: std::collections::HashMap::new(),
            find_query: None,
            find_matches: Vec::new(),
            find_current: 0,
            find_editor: None,
            pending_edits: std::collections::HashMap::new(),
            deleted_rows: std::collections::HashSet::new(),
            added_rows: Vec::new(),
            added_row_anchors: Vec::new(),
            edit_undo_stack: Vec::new(),
            edit_redo_stack: Vec::new(),
            suppress_edit_undo: false,
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
            heatmap_enabled: false,
            heatmap_ranges: Vec::new(),
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
            mongo_view_override: None,
            mongo_documents_view: None,
            mongo_documents_built_for: None,
            active_request: 0,
            error_view: None,
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

    /// The narrowest and widest a column may be dragged to. Narrow enough to
    /// push a column out of the way, wide enough for a long address.
    // 40 pixels holds neither a value nor a header, so a column dragged that
    // narrow costs space and shows nothing. 65 is the minimum JetBrains keeps.
    const MIN_COLUMN_W: f32 = 65.0;
    const MAX_COLUMN_W: f32 = 1600.0;

    fn begin_column_resize(&mut self, display_pos: usize, grab_x: f32) {
        let Some(&data_col) = self.visible_columns.get(display_pos) else {
            return;
        };
        let started_at = self
            .col_widths
            .get(display_pos)
            .copied()
            .map(f32::from)
            .unwrap_or(120.0);
        self.column_resize = Some(ColumnResizeDrag {
            display_pos,
            data_col,
            grab_x,
            started_at,
        });
    }

    fn update_column_resize(&mut self, x: f32) {
        let Some(drag) = self.column_resize else {
            return;
        };
        if self.visible_columns.get(drag.display_pos) != Some(&drag.data_col) {
            self.end_column_resize();
            return;
        }
        let width =
            (drag.started_at + x - drag.grab_x).clamp(Self::MIN_COLUMN_W, Self::MAX_COLUMN_W);
        if let Some(slot) = self.col_widths.get_mut(drag.display_pos) {
            *slot = px(width);
        }
        if let Some(name) = self.column_name_at(drag.display_pos) {
            self.widths_by_hand.insert(name, px(width));
        }
        // The columns are drawn from a running total of their widths, and what
        // is on screen is decided from the same numbers, so both have to be
        // worked out again for the drag to be seen at all.
        self.recompute_column_edges();
    }

    fn end_column_resize(&mut self) {
        self.column_resize = None;
    }

    /// Back to the width the rows themselves suggest.
    const MOST_REMEMBERED_WIDTHS: usize = 128;

    fn forget_widths_for_columns_that_left(&mut self) {
        if self.widths_by_hand.len() <= Self::MOST_REMEMBERED_WIDTHS {
            return;
        }
        let Some(result) = self.result.as_ref() else {
            self.widths_by_hand.clear();
            return;
        };
        let still_here: std::collections::HashSet<String> = (0..result.columns.len())
            .filter_map(|data_col| Self::width_key(&result.columns, data_col))
            .collect();
        self.widths_by_hand
            .retain(|name, _| still_here.contains(name));
    }

    fn fit_column_to_its_rows(&mut self, display_pos: usize) {
        if let Some(name) = self.column_name_at(display_pos) {
            self.widths_by_hand.remove(&name);
        }
        self.recompute_layout();
    }

    /// The name of the column at a display position, which is how a width the
    /// reader set is remembered.
    fn column_name_at(&self, display_pos: usize) -> Option<String> {
        let data_col = *self.visible_columns.get(display_pos)?;
        let result = self.result.as_ref()?;
        Self::width_key(&result.columns, data_col)
    }

    /// The key a hand-set width is remembered under: the column's name, plus
    /// how many columns of that same name came before it. A join can hand us
    /// two `id` columns, and they are resized separately.
    fn width_key(columns: &[String], data_col: usize) -> Option<String> {
        let name = columns.get(data_col)?;
        let ordinal = columns[..data_col].iter().filter(|it| *it == name).count();
        Some(format!("{name}#{ordinal}"))
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

    // Recomputes the cached row order (with sort applied) and per-column widths.
    // Called only when the result or sort changes — never per scroll frame — so
    // scrolling stays cheap.
    fn recompute_layout(&mut self) {
        let Some(result) = self.result.as_ref() else {
            self.order.clear();
            self.col_widths.clear();
            self.column_edges.clear();
            self.numeric_columns.clear();
            self.heatmap_ranges.clear();
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
                    // Numeric-aware comparison: if both sides parse as a number, compare
                    // numerically ("9" < "10"); otherwise fall back to string comparison
                    // (mixed/non-numeric columns), matching how aggregates already treat
                    // numeric values in `compute_column_aggregates`.
                    let ord = match (a_val.parse::<f64>(), b_val.parse::<f64>()) {
                        (Ok(a_num), Ok(b_num)) => a_num
                            .partial_cmp(&b_num)
                            .unwrap_or(std::cmp::Ordering::Equal),
                        _ => a_val.cmp(b_val),
                    };
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
                match Self::width_key(&result.columns, data_col)
                    .and_then(|key| self.widths_by_hand.get(&key))
                {
                    Some(&by_hand) => by_hand,
                    None => px((widest as f32 * 7.5 + 28.0).clamp(80.0, 360.0)),
                }
            })
            .collect();

        // Numeric flag per data column (for right-alignment), from type metadata
        // when available, otherwise inferred from the sampled values.
        let column_infos = self.column_infos.as_ref();
        self.numeric_columns = (0..total_cols)
            .map(|data_col| {
                let data_type = column_infos.and_then(|infos| {
                    let col_name = result.columns.get(data_col)?;
                    infos
                        .iter()
                        .find(|info| &info.name == col_name)
                        .map(|info| info.data_type.as_str())
                });
                let values = result.rows[..sample]
                    .iter()
                    .map(move |row| row.get(data_col).and_then(|cell| cell.as_deref()));
                column_is_numeric(data_type, values)
            })
            .collect();

        // Min/max over every loaded row for numeric columns, driving the heatmap
        // tint. Scanned in full (not just the width-sampling window above) so the
        // range reflects every row the grid can paint, not just the first sample.
        self.heatmap_ranges = (0..total_cols)
            .map(|data_col| {
                if !self.numeric_columns.get(data_col).copied().unwrap_or(false) {
                    return None;
                }
                result
                    .rows
                    .iter()
                    .fold(None, |range: Option<(f64, f64)>, row| {
                        let value = row
                            .get(data_col)
                            .and_then(|cell| cell.as_deref())
                            .and_then(|text| text.parse::<f64>().ok());
                        match (range, value) {
                            (None, Some(v)) => Some((v, v)),
                            (Some((lo, hi)), Some(v)) => Some((lo.min(v), hi.max(v))),
                            (range, None) => range,
                        }
                    })
            })
            .collect();

        // Provisional boolean flag per data column, same type-first/sample-fallback
        // shape as numeric_columns above -- lets column_kind_at render the check/dash
        // icon immediately instead of flashing raw "1"/"0" until describe_table loads.
        self.boolean_columns = (0..total_cols)
            .map(|data_col| {
                let data_type = column_infos.and_then(|infos| {
                    let col_name = result.columns.get(data_col)?;
                    infos
                        .iter()
                        .find(|info| &info.name == col_name)
                        .map(|info| info.data_type.as_str())
                });
                column_is_boolean(data_type)
            })
            .collect();

        self.recompute_column_edges();

        self.recompute_local_filter_inner();
    }

    /// Cumulative right edge of each visible column (content coords) for
    /// click-to-column hit testing, plus the total content width. Worked out
    /// again whenever a width changes, which is what makes a column dragged
    /// wider move the ones after it.
    fn recompute_column_edges(&mut self) {
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

    fn active_cell_row(&self) -> Option<usize> {
        self.selected_cell.map(|(row, _)| row)
    }

    fn active_cell_column(&self) -> Option<usize> {
        self.selected_cell.map(|(_, col)| col)
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
        // Ctrl/Cmd-click on a cell (as opposed to the row gutter, which does
        // support additive multi-row selection) has no dedicated behavior:
        // the selection model only tracks a single contiguous cell range, not
        // a discontiguous set, so it falls back to a plain single-cell select.
        _control: bool,
    ) {
        if shift {
            if let Some(anchor) = self.selected_cell {
                self.selected_rows.clear();
                self.selected_cell_range = Some((anchor, (abs_idx, cell_idx)));
            } else {
                let anchor_display_idx = self.last_selected_row.unwrap_or(display_idx);
                self.select_row_range(anchor_display_idx, display_idx);
            }
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

    // Selects the cell at the given position in `filtered_display_order` (not
    // `display_row_entries`, so this deliberately matches `move_active_cell`'s
    // plain-arrow navigation and, like it, does not visit staged "added" rows)
    // and scrolls the row into view. Shared by the Home/End/first/last/page
    // keyboard-navigation actions below.
    fn select_display_cell(&mut self, display_idx: usize, col_pos: usize, cx: &mut Context<Self>) {
        let Some(&abs_idx) = self.filtered_display_order.get(display_idx) else {
            return;
        };
        let Some(&col_idx) = self.visible_columns.get(col_pos) else {
            return;
        };
        self.selected_cell = Some((abs_idx, col_idx));
        self.selected_cell_range = None;
        self.selected_rows.clear();
        self.scroll_handle
            .scroll_to_item(display_idx, gpui::ScrollStrategy::Center);
        cx.notify();
    }

    fn current_display_idx(&self) -> usize {
        self.selected_cell
            .and_then(|(abs_idx, _)| {
                self.filtered_display_order
                    .iter()
                    .position(|&a| a == abs_idx)
            })
            .unwrap_or(0)
    }

    fn current_col_pos(&self) -> usize {
        self.selected_cell
            .and_then(|(_, col_idx)| self.visible_columns.iter().position(|&c| c == col_idx))
            .unwrap_or(0)
    }

    fn select_row_start(&mut self, cx: &mut Context<Self>) {
        self.select_display_cell(self.current_display_idx(), 0, cx);
    }

    fn select_row_end(&mut self, cx: &mut Context<Self>) {
        if self.visible_columns.is_empty() {
            return;
        }
        self.select_display_cell(
            self.current_display_idx(),
            self.visible_columns.len() - 1,
            cx,
        );
    }

    fn select_first_cell(&mut self, cx: &mut Context<Self>) {
        self.select_display_cell(0, 0, cx);
    }

    fn select_last_cell(&mut self, cx: &mut Context<Self>) {
        let row_count = self.filtered_display_order.len();
        if row_count == 0 || self.visible_columns.is_empty() {
            return;
        }
        self.select_display_cell(row_count - 1, self.visible_columns.len() - 1, cx);
    }

    fn move_page(&mut self, delta_row: isize, cx: &mut Context<Self>) {
        let row_count = self.filtered_display_order.len();
        if row_count == 0 || self.visible_columns.is_empty() {
            return;
        }
        let new_display_idx = self
            .current_display_idx()
            .saturating_add_signed(delta_row)
            .min(row_count - 1);
        self.select_display_cell(new_display_idx, self.current_col_pos(), cx);
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

    // Commits any edit in progress on a DIFFERENT cell before a new cell is
    // selected. This does not depend on the editor's own focus/blur events —
    // those are not reliably emitted for every focus-transition path here, so
    // clicking a different cell while editing could otherwise leave the
    // in-progress edit open and uncommitted indefinitely. Returns `false` when
    // the commit was rejected (invalid value for the column type), in which
    // case the caller must abort the click and keep the invalid cell in edit
    // mode, matching `commit_and_move`'s existing validation-blocking behavior.
    fn commit_other_cell_edit(
        &mut self,
        abs_idx: usize,
        cell_idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        match self.cell_edit.as_ref() {
            Some(edit) if edit.abs_idx != abs_idx || edit.col_idx != cell_idx => {
                self.commit_cell_edit(window, cx)
            }
            _ => true,
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
        if !self.commit_other_cell_edit(abs_idx, cell_idx, window, cx) {
            cx.notify();
            return;
        }
        window.focus(&self.focus_handle, cx);
        self.select_cell_from_click(abs_idx, display_idx, cell_idx, shift, control);
        if click_count >= 2 && !matches!(self.column_kind_at(cell_idx), CellEditorKind::Boolean) {
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
        if !self.commit_other_cell_edit(abs_idx, cell_idx, window, cx) {
            cx.notify();
            return;
        }
        window.focus(&self.focus_handle, cx);
        self.select_cell_from_click(abs_idx, display_idx, cell_idx, shift, control);
        if click_count >= 2 && !matches!(self.column_kind_at(cell_idx), CellEditorKind::Boolean) {
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

    // Whether a cell should paint the range-selection tint: any selected cell
    // (lone active cell or part of a multi-cell range) EXCEPT the active cell
    // itself, which stays background-colored -- its own border is enough to mark
    // it (the range fills, the active corner does not). Shared by both grid
    // render paths so a test can exercise the exact same decision the renderer
    // makes, not a re-derived copy of the formula.
    fn cell_receives_selection_tint(
        &self,
        abs_idx: usize,
        display_idx: usize,
        cell_idx: usize,
    ) -> bool {
        let is_active = self.selected_cell == Some((abs_idx, cell_idx));
        let is_selected =
            is_active || self.selected_cell_range_contains(abs_idx, display_idx, cell_idx);
        is_selected && !is_active
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
        self.date_popup = None;
        self.sort_columns.clear();
    }

    pub fn clear_table_context(&mut self) {
        self.table_name = None;
        self.filter_editor = None;
        self.primary_key_columns = None;
        self.column_infos = None;
        self.fk_columns.clear();
        self.enum_popup = None;
        self.date_popup = None;
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
                    Err(err) => this.set_error(format_query_error(&err), cx),
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

    /// Claims this view for a new in-flight request and returns a token
    /// identifying it. A caller dispatching a query should call this once, up
    /// front, and pass the returned token to `is_current_request` before
    /// applying the query's result -- see the field doc on `active_request`.
    pub fn begin_request(&mut self) -> u64 {
        self.active_request = self.active_request.wrapping_add(1);
        self.active_request
    }

    /// Whether `token` (from a prior `begin_request` call) is still this
    /// view's most recent request, i.e. no newer request has superseded it.
    pub fn is_current_request(&self, token: u64) -> bool {
        self.active_request == token
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
        self.clear_edit_history();
        self.cell_drag_anchor = None;
        self.suppress_next_cell_click = false;
        self.selected_cell = None;
        self.selected_cell_range = None;
        self.selected_rows.clear();
        self.last_selected_row = None;
        self.value_editor_open = false;
        self.value_editor_resize_drag = None;
        self.column_resize = None;
        self.forget_widths_for_columns_that_left();
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
        self.mongo_view_override = None;
        self.mongo_documents_view = None;
        self.mongo_documents_built_for = None;
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
        if special == SpecialResult::None || self.special_built_for == Some(self.result_generation)
        {
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
                let is_analyze = self
                    .base_sql
                    .as_deref()
                    .map(crate::explain_plan::sql_requests_analyze)
                    .unwrap_or(false);
                let query_context = self
                    .store
                    .as_ref()
                    .and_then(|store| store.upgrade())
                    .zip(self.connection_id)
                    .map(|(store, connection_id)| {
                        let driver = store
                            .read(cx)
                            .connections()
                            .iter()
                            .find(|c| c.config.id == connection_id)
                            .map(|c| c.config.driver)
                            .unwrap_or(DatabaseDriver::MySQL);
                        crate::explain_plan::ExplainQueryContext {
                            store,
                            connection_id,
                            database: self.database.clone().unwrap_or_default(),
                            driver,
                            sql: self.base_sql.clone().unwrap_or_default(),
                        }
                    });
                let view = cx.new(|cx| {
                    crate::explain_plan::ExplainPlanView::new(
                        roots,
                        query_context,
                        is_analyze,
                        window,
                        cx,
                    )
                });
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
                    .child(Label::new(title).size(LabelSize::Small).color(Color::Muted))
                    .child(div().flex_1())
                    .child(
                        Button::new("special-show-as-table", "Show as Table")
                            .style(cyberpunk::Rank::Quiet.style())
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

    // The view to show for the current result if it's Mongo document-shaped,
    // or None for every other result (nothing to toggle).
    fn active_mongo_view(&self) -> Option<MongoResultView> {
        let result = self.result.as_ref()?;
        result.raw_documents.as_ref()?;
        Some(
            self.mongo_view_override
                .unwrap_or_else(|| default_mongo_view_for_result(result)),
        )
    }

    fn set_mongo_view(&mut self, mode: MongoResultView, cx: &mut Context<Self>) {
        self.mongo_view_override = Some(mode);
        cx.notify();
    }

    // Builds (once per result) the read-only editor backing the Documents
    // view. Runs in render(), where a Window is available, mirroring
    // `sync_special_view`.
    fn sync_mongo_documents_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_mongo_view() != Some(MongoResultView::Documents)
            || self.mongo_documents_built_for == Some(self.result_generation)
        {
            return;
        }
        let Some(result) = self.result.clone() else {
            return;
        };
        let text = mongo_documents_display_text(&result);
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
        self.mongo_documents_view = Some(editor);
        self.mongo_documents_built_for = Some(self.result_generation);
    }

    // The Table/Documents toggle chips, shared between the Documents view's
    // own header and the grid toolbar (so either view can flip to the other).
    fn render_mongo_view_toggle(
        &self,
        mode: MongoResultView,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        cyberpunk::segmented(vec![
            div()
                .debug_selector(|| "mongo-view-table".into())
                .child(
                    Button::new("mongo-view-table", "Table")
                        .label_size(LabelSize::Small)
                        .toggle_state(mode == MongoResultView::Table)
                        .tooltip(Tooltip::text("Show documents projected into columns"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.set_mongo_view(MongoResultView::Table, cx);
                        })),
                )
                .into_any_element(),
            div()
                .debug_selector(|| "mongo-view-documents".into())
                .child(
                    Button::new("mongo-view-documents", "Documents")
                        .label_size(LabelSize::Small)
                        .toggle_state(mode == MongoResultView::Documents)
                        .tooltip(Tooltip::text("Show documents as pretty-printed BSON/JSON"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.set_mongo_view(MongoResultView::Documents, cx);
                        })),
                )
                .into_any_element(),
        ])
    }

    fn render_mongo_documents_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(result) = self.result.as_ref() else {
            return div().into_any_element();
        };
        let total_rows = result.rows.len();
        let status = format!(
            "{} document{}",
            total_rows,
            if total_rows == 1 { "" } else { "s" }
        );
        let mode = self
            .active_mongo_view()
            .unwrap_or(MongoResultView::Documents);
        let body = self
            .mongo_documents_view
            .clone()
            .map(|editor| {
                div()
                    .id("mongo-documents-view")
                    .debug_selector(|| "MONGO_DOCUMENTS_VIEW".to_string())
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .p_2()
                    .child(editor)
                    .into_any_element()
            })
            .unwrap_or_else(|| div().flex_1().into_any_element());

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
                        Label::new(status)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1())
                    .child(self.render_mongo_view_toggle(mode, cx)),
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
        // Same reasoning as `set_query_result`: this is a new, unrelated
        // query (e.g. an FK-navigation jump to a different table via
        // `navigate_to_fk_row`), so a hidden-column index from whatever was
        // shown before must not carry over.
        self.hidden_columns.clear();
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
        // Column-hide selections are per-result-shape, not a durable view
        // preference: `hidden_columns` holds column *indices*, so carrying it
        // into a new, unrelated query silently hides whatever column happens
        // to land on the same index in the new result. `set_result` (used for
        // same-query re-fetches, e.g. a sort-triggered refresh) intentionally
        // leaves this alone, matching `sort_columns`'s split above.
        self.hidden_columns.clear();
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
                        // A newer fill (a re-run, a page-size change, or a query
                        // dispatched straight into this view) may have started
                        // and cancelled this one while the batch was in flight.
                        // Applying it now would splice a stale batch into the
                        // newer request's rows.
                        if cancel.load(Ordering::SeqCst) {
                            break;
                        }
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
                        if cancel.load(Ordering::SeqCst) {
                            break;
                        }
                        this.update(cx, |view, cx| view.set_error(format_query_error(&err), cx))
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

        self.maybe_open_date_popup(abs_idx, col_idx, &initial, cx);
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
        self.maybe_open_date_popup(abs_idx, col_idx, &initial, cx);
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
        self.begin_added_cell_edit(
            abs_idx,
            col_idx,
            added_idx,
            CellEditEntry::CursorEnd,
            window,
            cx,
        );
    }

    fn render_cell_editor(editor: Entity<Editor>) -> AnyElement {
        h_flex()
            .size_full()
            .gap_1()
            .items_center()
            .child(div().flex_1().min_w_0().child(editor))
            .into_any_element()
    }

    fn render_typed_cell_body(
        body: AnyElement,
        align_right: bool,
        text_debug_id: String,
    ) -> AnyElement {
        h_flex()
            .size_full()
            .gap_1()
            .items_center()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .when(align_right, |element| element.flex().justify_end())
                    // A flex item shrinks to fit by default, which wraps the
                    // label's text onto multiple lines instead of clipping it.
                    // flex_none keeps it at its natural single-line width so
                    // justify_end only repositions it, matching how the
                    // non-right-aligned (block layout) case already clips.
                    .child(
                        div()
                            .flex_none()
                            .debug_selector(move || text_debug_id)
                            .child(body),
                    ),
            )
            .into_any_element()
    }

    // Opens the calendar popup alongside the just-spawned text editor when the
    // column is DATE/DATETIME; a no-op for every other column kind. The
    // calendar's initial page comes from the cell's current value when it
    // parses as a date, falling back to today so an empty/NULL cell still
    // opens on a sensible month rather than an arbitrary or invalid one.
    fn maybe_open_date_popup(
        &mut self,
        abs_idx: usize,
        col_idx: usize,
        initial: &str,
        cx: &mut Context<Self>,
    ) {
        let is_datetime = match self.column_kind_at(col_idx) {
            CellEditorKind::Date => false,
            CellEditorKind::DateTime => true,
            _ => return,
        };
        let page_date =
            parse_date_prefix(initial).unwrap_or_else(|| time::OffsetDateTime::now_utc().date());
        self.date_popup = Some(DatePopup {
            abs_idx,
            col_idx,
            is_datetime,
            display_year: page_date.year(),
            display_month: page_date.month(),
        });
        cx.notify();
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
        let column_kind = self.column_kind_at(col_idx);
        let placeholder = match column_kind {
            CellEditorKind::Numeric => Some("number"),
            CellEditorKind::Date => Some("YYYY-MM-DD"),
            CellEditorKind::DateTime => Some("YYYY-MM-DD HH:MM:SS"),
            _ => None,
        };
        // Value completion only makes sense for free-text columns; Boolean/Enum
        // never reach this function (begin_cell_edit routes them to a dedicated
        // toggle/popup before an inline editor is ever spawned).
        let completion_values = matches!(column_kind, CellEditorKind::Text)
            .then(|| {
                self.result
                    .as_ref()
                    .map(|result| distinct_column_values(result, col_idx))
            })
            .flatten()
            .filter(|values| !values.is_empty());
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
            if let Some(values) = completion_values {
                ed.set_completion_provider(Some(Rc::new(CellValueCompletionProvider { values })));
                // This curated, always-short list of the column's own loaded
                // values should show up regardless of the user's global "show
                // completions on input" code-editor preference.
                ed.set_show_completions_on_input(Some(true));
            }
            ed
        });
        // Best-effort backup: commits on blur when the editor's focus/blur
        // events do fire. This is NOT the primary mechanism — clicking a
        // different cell reliably commits via the explicit check at the top
        // of `click_loaded_cell`/`click_added_cell` instead, because
        // `EditorEvent::Focused`/`Blurred` are not guaranteed to be emitted
        // for this editor in every focus-transition path, which previously
        // left clicking away from an edit unable to commit it at all.
        let subscription = cx.subscribe_in(&editor, window, |this, _editor, event, window, cx| {
            if matches!(event, EditorEvent::Blurred) {
                this.commit_cell_edit(window, cx);
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
        self.date_popup = None;
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

    // Moves the active cell in Ready mode (no edit in progress), clamping at the
    // grid edges and scrolling the new row into view, like arrow keys in Excel.
    fn move_active_cell(&mut self, delta_col: i64, delta_row: i64, cx: &mut Context<Self>) {
        let row_count = self.result.as_ref().map_or(0, |r| r.rows.len());
        let vis_cols = self.visible_columns.clone();
        if row_count == 0 || vis_cols.is_empty() {
            return;
        }
        let (new_abs, new_col) = match self.selected_cell {
            Some((abs, col)) => {
                let new_abs = (abs as i64 + delta_row).clamp(0, row_count as i64 - 1) as usize;
                let cur_vis = vis_cols.iter().position(|&c| c == col).unwrap_or(0);
                let new_vis =
                    (cur_vis as i64 + delta_col).clamp(0, vis_cols.len() as i64 - 1) as usize;
                (new_abs, vis_cols[new_vis])
            }
            None => (0, vis_cols[0]),
        };
        self.selected_cell = Some((new_abs, new_col));
        self.selected_cell_range = None;
        self.selected_rows.clear();
        if let Some(display_idx) = self
            .filtered_display_order
            .iter()
            .position(|&a| a == new_abs)
        {
            self.scroll_handle
                .scroll_to_item(display_idx, gpui::ScrollStrategy::Center);
        }
        cx.notify();
    }

    // Begins editing the active cell from the keyboard (F2 / type-to-replace).
    // `begin_cell_edit` already no-ops on non-editable tables; boolean columns are
    // toggled by click, not text-edited, so they are skipped here.
    fn begin_edit_active_cell(
        &mut self,
        entry: CellEditEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((abs, col)) = self.selected_cell else {
            return;
        };
        if matches!(self.column_kind_at(col), CellEditorKind::Boolean) {
            return;
        }
        self.begin_cell_edit(abs, col, entry, window, cx);
    }

    // Starts an edit by typing on the active cell: the typed text replaces the
    // current value (Excel type-to-replace).
    fn type_to_replace_active_cell(
        &mut self,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.begin_edit_active_cell(CellEditEntry::Replace, window, cx);
        if let Some(editor) = self.cell_edit.as_ref().map(|edit| edit.editor.clone()) {
            editor.update(cx, |ed, cx| ed.set_text(text, window, cx));
        }
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

        // An editor left empty at commit time never overwrites the cell: this
        // matches leaving the edit unchanged (or typing something and erasing
        // it back to nothing) with cancelling the edit, so a NULL (or any
        // other) original value is never silently coerced into an empty
        // string. Set Empty Value is the only way to store a real "".
        if raw_text.is_empty() {
            self.cell_edit.take();
            self.date_popup = None;
            cx.notify();
            return true;
        }

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
        self.date_popup = None;
        let new_value = CellValue::from_text(raw_text);

        // Added rows have no loaded value or key: the edit writes straight into
        // the added-row buffer and is submitted as part of the INSERT.
        if let CellEditTarget::Added(added_idx) = target {
            let abs = self.loaded_row_count() + added_idx;
            self.write_cell_value(abs, col_idx, new_value, cx);
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
        self.maybe_record_edit_undo();
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
        // Provisional fallback while describe_table metadata hasn't loaded yet: a
        // sampled-boolean-looking column renders as Boolean from the first paint
        // instead of flashing raw "1"/"0" text. Real type metadata below always
        // takes priority once it arrives, exactly like numeric_columns' fallback.
        let provisional = || {
            if self.boolean_columns.get(col_idx).copied().unwrap_or(false) {
                CellEditorKind::Boolean
            } else {
                CellEditorKind::Text
            }
        };
        let col_name = self.result.as_ref().and_then(|r| r.columns.get(col_idx));
        let (Some(col_name), Some(infos)) = (col_name, self.column_infos.as_ref()) else {
            return provisional();
        };
        let Some(info) = infos.iter().find(|ci| &ci.name == col_name) else {
            return provisional();
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
        let abs = self.loaded_row_count() + added_idx;
        self.write_cell_value(abs, col_idx, new, cx);
        cx.notify();
    }

    // Applies the enum popup selection and closes the popup.
    fn apply_enum_selection(&mut self, value: CellValue, cx: &mut Context<Self>) {
        let Some(popup) = self.enum_popup.take() else {
            return;
        };
        if let Some(added_idx) = popup.added_idx {
            let abs = self.loaded_row_count() + added_idx;
            self.write_cell_value(abs, popup.col_idx, value, cx);
        } else {
            self.buffer_loaded_cell_value(popup.abs_idx, popup.col_idx, value, cx);
        }
        cx.notify();
    }

    // Moves the calendar popup's displayed page by one month, wrapping the year
    // at the December/January boundary. Browsing months never touches the cell
    // editor's text or the popup's target cell.
    fn date_popup_shift_month(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(popup) = self.date_popup.as_mut() else {
            return;
        };
        if forward {
            if popup.display_month == time::Month::December {
                popup.display_year += 1;
            }
            popup.display_month = popup.display_month.next();
        } else {
            if popup.display_month == time::Month::January {
                popup.display_year -= 1;
            }
            popup.display_month = popup.display_month.previous();
        }
        cx.notify();
    }

    // Writes the picked day into the open cell editor's text and closes the
    // calendar popup. For a DATETIME column, any time-of-day portion already in
    // the editor (typed or left at its default) is preserved rather than reset,
    // so picking a day never clobbers a time the user already set.
    fn apply_date_selection(
        &mut self,
        date: time::Date,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(popup) = self.date_popup.take() else {
            return;
        };
        let Some(edit) = self.cell_edit.as_ref() else {
            cx.notify();
            return;
        };
        let editor = edit.editor.clone();
        let date_text = format_date_ymd(date);
        let new_text = if popup.is_datetime {
            let current = editor.read(cx).text(cx);
            let time_part = current.get(11..).filter(|t| !t.is_empty());
            match time_part {
                Some(time_part) => format!("{date_text} {time_part}"),
                None => format!("{date_text} 00:00:00"),
            }
        } else {
            date_text
        };
        editor.update(cx, |editor, cx| {
            editor.set_text(new_text, window, cx);
            editor.move_to_end(&Default::default(), window, cx);
            let handle = editor.focus_handle(cx);
            window.focus(&handle, cx);
        });
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
            self.write_cell_value(abs_idx, col_idx, value, cx);
            self.status_message = None;
            cx.notify();
            return;
        }
        self.buffer_loaded_cell_value(abs_idx, col_idx, value, cx);
    }

    // Writes a value into a single cell, routing to the added-row buffer or the
    // loaded-row pending buffer. Shared by paste, cut, and range fills.
    fn write_cell_value(
        &mut self,
        abs_idx: usize,
        col_idx: usize,
        value: CellValue,
        cx: &mut Context<Self>,
    ) {
        let loaded_count = self.loaded_row_count();
        if abs_idx >= loaded_count {
            let added_idx = abs_idx - loaded_count;
            let changed = self
                .added_rows
                .get(added_idx)
                .and_then(|row| row.get(col_idx))
                .is_some_and(|cell| *cell != value);
            if changed {
                self.maybe_record_edit_undo();
            }
            if let Some(cell) = self
                .added_rows
                .get_mut(added_idx)
                .and_then(|row| row.get_mut(col_idx))
            {
                *cell = value;
            }
            return;
        }
        self.buffer_loaded_cell_value(abs_idx, col_idx, value, cx);
    }

    // The current buffered value of a cell: the pending edit if present, else the
    // loaded value, else the added-row value. Used as the source for fills.
    fn current_cell_value(&self, abs_idx: usize, col_idx: usize) -> CellValue {
        if let Some(value) = self.pending_cell_value(abs_idx, col_idx) {
            return value.clone();
        }
        let loaded_count = self.loaded_row_count();
        if abs_idx < loaded_count {
            return CellValue::from_loaded(&self.loaded_cell_value(abs_idx, col_idx));
        }
        self.added_rows
            .get(abs_idx - loaded_count)
            .and_then(|row| row.get(col_idx))
            .cloned()
            .unwrap_or(CellValue::Null)
    }

    fn edit_snapshot(&self) -> EditSnapshot {
        EditSnapshot {
            pending_edits: self.pending_edits.clone(),
            deleted_rows: self.deleted_rows.clone(),
            added_rows: self.added_rows.clone(),
            added_row_anchors: self.added_row_anchors.clone(),
        }
    }

    // Records the buffer state before a mutating edit so it can be undone. Any
    // pending redo is invalidated, matching the usual undo/redo model.
    fn record_edit_undo(&mut self) {
        self.edit_undo_stack.push(self.edit_snapshot());
        self.edit_redo_stack.clear();
    }

    // Per-cell undo recording for single-cell writes. A multi-cell operation sets
    // `suppress_edit_undo` so its one up-front entry covers the whole batch.
    fn maybe_record_edit_undo(&mut self) {
        if !self.suppress_edit_undo {
            self.record_edit_undo();
        }
    }

    // Runs a multi-cell edit as one undo unit: records a single snapshot up front,
    // then suppresses the per-cell recording the inner writes would otherwise do.
    fn run_edit_batch(
        &mut self,
        cx: &mut Context<Self>,
        body: impl FnOnce(&mut Self, &mut Context<Self>),
    ) {
        self.record_edit_undo();
        self.suppress_edit_undo = true;
        body(self, cx);
        self.suppress_edit_undo = false;
    }

    fn restore_edit_snapshot(&mut self, snapshot: EditSnapshot) {
        self.pending_edits = snapshot.pending_edits;
        self.deleted_rows = snapshot.deleted_rows;
        self.added_rows = snapshot.added_rows;
        self.added_row_anchors = snapshot.added_row_anchors;
    }

    // Undo/redo of buffered edits (Excel Ctrl+Z / Ctrl+Y). Operates on the local
    // edit buffer only; nothing is sent to the database until Submit.
    fn undo_edit(&mut self, cx: &mut Context<Self>) {
        let Some(previous) = self.edit_undo_stack.pop() else {
            return;
        };
        self.edit_redo_stack.push(self.edit_snapshot());
        self.restore_edit_snapshot(previous);
        self.cell_edit = None;
        self.status_message = None;
        cx.notify();
    }

    fn redo_edit(&mut self, cx: &mut Context<Self>) {
        let Some(next) = self.edit_redo_stack.pop() else {
            return;
        };
        self.edit_undo_stack.push(self.edit_snapshot());
        self.restore_edit_snapshot(next);
        self.cell_edit = None;
        self.status_message = None;
        cx.notify();
    }

    // Drops the undo/redo history. Called whenever the edit buffer is reset to the
    // database state (new result, Submit, Revert, Commit, Rollback), since the old
    // snapshots no longer describe a reachable buffer state.
    fn clear_edit_history(&mut self) {
        self.edit_undo_stack.clear();
        self.edit_redo_stack.clear();
        self.suppress_edit_undo = false;
    }

    // Fill down (Ctrl+D): every column of the selection takes the value of its
    // top cell and copies it down to the rows below. With no range, the active
    // cell's value fills the cell directly below it.
    fn fill_down(&mut self, cx: &mut Context<Self>) {
        if self.table_name.is_none() {
            self.status_message = Some("Fill needs a table-backed result.".to_string());
            cx.notify();
            return;
        }
        let (rows, cols) = self.fill_region(true);
        if rows.len() < 2 || cols.is_empty() {
            self.status_message = Some("Select a range, or a cell with a row below.".to_string());
            cx.notify();
            return;
        }
        let top = rows[0];
        self.run_edit_batch(cx, |this, cx| {
            for &col in &cols {
                let value = this.current_cell_value(top, col);
                for &abs_idx in &rows[1..] {
                    this.write_cell_value(abs_idx, col, value.clone(), cx);
                }
            }
        });
        self.status_message = None;
        cx.notify();
    }

    // Fill right (Ctrl+R): every row of the selection takes the value of its left
    // cell and copies it across. With no range, the active cell's value fills the
    // cell to its right.
    fn fill_right(&mut self, cx: &mut Context<Self>) {
        if self.table_name.is_none() {
            self.status_message = Some("Fill needs a table-backed result.".to_string());
            cx.notify();
            return;
        }
        let (rows, cols) = self.fill_region(false);
        if cols.len() < 2 || rows.is_empty() {
            self.status_message =
                Some("Select a range, or a cell with a column to the right.".to_string());
            cx.notify();
            return;
        }
        let left = cols[0];
        self.run_edit_batch(cx, |this, cx| {
            for &abs_idx in &rows {
                let value = this.current_cell_value(abs_idx, left);
                for &col in &cols[1..] {
                    this.write_cell_value(abs_idx, col, value.clone(), cx);
                }
            }
        });
        self.status_message = None;
        cx.notify();
    }

    // Computes the (display-ordered rows, columns) a fill should cover. A real
    // selection is used as-is; a lone active cell is grown by one row (down) or
    // one column (right) so a single Ctrl+D/Ctrl+R still fills the neighbour.
    fn fill_region(&self, down: bool) -> (Vec<usize>, Vec<usize>) {
        let Some(result) = self.result.as_ref() else {
            return (Vec::new(), Vec::new());
        };
        let has_selection = self.selected_cell_range.is_some() || !self.selected_rows.is_empty();
        if has_selection {
            return (
                self.selected_display_rows_for_copy(),
                self.selected_columns_for_copy(result),
            );
        }
        let Some((abs_idx, col_idx)) = self.selected_cell else {
            return (Vec::new(), Vec::new());
        };
        if down {
            let mut rows = vec![abs_idx];
            if let Some(display_idx) = self.display_idx_of(abs_idx) {
                if let Some(below) = self.abs_idx_at_display_idx(display_idx + 1) {
                    rows.push(below);
                }
            }
            (rows, vec![col_idx])
        } else {
            let vis_cols = self.visible_columns.clone();
            let mut cols = vec![col_idx];
            if let Some(pos) = vis_cols.iter().position(|&c| c == col_idx) {
                if let Some(&right) = vis_cols.get(pos + 1) {
                    cols.push(right);
                }
            }
            (vec![abs_idx], cols)
        }
    }

    // Extends the selection from the active cell (the anchor) by moving the far
    // corner, mirroring `move_active_cell`'s clamp model. The active cell stays
    // put; Shift+Arrow grows/shrinks the rectangle (Excel range select).
    fn extend_selection(&mut self, delta_col: i64, delta_row: i64, cx: &mut Context<Self>) {
        let row_count = self.result.as_ref().map_or(0, |r| r.rows.len());
        let vis_cols = self.visible_columns.clone();
        if row_count == 0 || vis_cols.is_empty() {
            return;
        }
        let Some(anchor) = self.selected_cell else {
            self.selected_cell = Some((0, vis_cols[0]));
            cx.notify();
            return;
        };
        let (end_abs, end_col) = self
            .selected_cell_range
            .map(|(_, end)| end)
            .unwrap_or(anchor);
        let new_abs = (end_abs as i64 + delta_row).clamp(0, row_count as i64 - 1) as usize;
        let cur_vis = vis_cols.iter().position(|&c| c == end_col).unwrap_or(0);
        let new_vis = (cur_vis as i64 + delta_col).clamp(0, vis_cols.len() as i64 - 1) as usize;
        let new_end = (new_abs, vis_cols[new_vis]);
        self.selected_cell_range = (new_end != anchor).then_some((anchor, new_end));
        self.selected_rows.clear();
        if let Some(display_idx) = self.display_idx_of(new_abs) {
            self.scroll_handle
                .scroll_to_item(display_idx, gpui::ScrollStrategy::Center);
        }
        cx.notify();
    }

    // Selects every loaded cell (all display rows x all visible columns). The
    // active cell becomes the top-left. Added rows are not included.
    fn select_all_cells(&mut self, cx: &mut Context<Self>) {
        let vis_cols = self.visible_columns.clone();
        if vis_cols.is_empty() || self.filtered_display_order.is_empty() {
            return;
        }
        let last_display = self.filtered_display_order.len() - 1;
        let (Some(first_abs), Some(last_abs)) = (
            self.abs_idx_at_display_idx(0),
            self.abs_idx_at_display_idx(last_display),
        ) else {
            return;
        };
        let first_col = vis_cols[0];
        let last_col = vis_cols[vis_cols.len() - 1];
        self.selected_cell = Some((first_abs, first_col));
        self.selected_cell_range = Some(((first_abs, first_col), (last_abs, last_col)));
        self.selected_rows.clear();
        cx.notify();
    }

    // Pastes clipboard text (TSV) starting at the active cell, filling right and
    // down within the grid bounds. A single clipboard cell fills the whole
    // current selection (Excel behaviour). No SQL runs; edits go to the buffer.
    fn paste_from_clipboard(&mut self, cx: &mut Context<Self>) {
        if self.table_name.is_none() {
            self.status_message = Some("Paste needs a table-backed result.".to_string());
            cx.notify();
            return;
        }
        let Some((anchor_abs, anchor_col)) = self.selected_cell else {
            self.status_message = Some("Select a cell to paste into.".to_string());
            cx.notify();
            return;
        };
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        let grid = Self::parse_tsv_grid(&text);
        if grid.is_empty() {
            return;
        }
        let vis_cols = self.visible_columns.clone();
        let Some(anchor_vis) = vis_cols.iter().position(|&c| c == anchor_col) else {
            return;
        };
        let col_count = self.result.as_ref().map_or(0, |r| r.columns.len());

        // A single copied value fills the whole current selection.
        let single_value_fill =
            grid.len() == 1 && grid[0].len() == 1 && self.selected_cell_range.is_some();

        self.run_edit_batch(cx, |this, cx| {
            if single_value_fill {
                let value = grid[0][0].clone();
                let rows = this.selected_display_rows_for_copy();
                let cols: Vec<usize> = match this.selected_cell_range {
                    Some(((_, a), (_, b))) => {
                        (a.min(b)..=a.max(b)).filter(|c| *c < col_count).collect()
                    }
                    None => vec![anchor_col],
                };
                for abs_idx in rows {
                    for &col in &cols {
                        this.write_cell_value(
                            abs_idx,
                            col,
                            CellValue::from_text(value.clone()),
                            cx,
                        );
                    }
                }
                return;
            }

            let Some(anchor_display) = this.display_idx_of(anchor_abs) else {
                return;
            };
            for (row_offset, row) in grid.iter().enumerate() {
                let Some(target_abs) = this.abs_idx_at_display_idx(anchor_display + row_offset)
                else {
                    continue;
                };
                for (col_offset, value) in row.iter().enumerate() {
                    let Some(&col) = vis_cols.get(anchor_vis + col_offset) else {
                        continue;
                    };
                    this.write_cell_value(target_abs, col, CellValue::from_text(value.clone()), cx);
                }
            }
        });
        self.status_message = None;
        cx.notify();
    }

    // Copies the selection (like Ctrl+C) then clears those cells to empty.
    fn cut_selection(&mut self, cx: &mut Context<Self>) {
        if self.table_name.is_none() {
            self.status_message = Some("Cut needs a table-backed result.".to_string());
            cx.notify();
            return;
        }
        self.copy_selected_to_clipboard(cx);
        let rows = self.selected_display_rows_for_copy();
        let col_count = self.result.as_ref().map_or(0, |r| r.columns.len());
        let cols: Vec<usize> = if !self.selected_rows.is_empty() {
            (0..col_count).collect()
        } else if let Some(((_, a), (_, b))) = self.selected_cell_range {
            (a.min(b)..=a.max(b)).filter(|c| *c < col_count).collect()
        } else if let Some((_, col)) = self.selected_cell {
            vec![col]
        } else {
            Vec::new()
        };
        if rows.is_empty() || cols.is_empty() {
            return;
        }
        self.run_edit_batch(cx, |this, cx| {
            for abs_idx in rows {
                for &col in &cols {
                    this.write_cell_value(abs_idx, col, CellValue::from_text(String::new()), cx);
                }
            }
        });
        self.status_message = None;
        cx.notify();
    }

    // Parses clipboard TSV into a row-major grid. Splits on newlines then tabs;
    // embedded tabs/newlines inside Excel-style quoted cells are not unquoted.
    fn parse_tsv_grid(text: &str) -> Vec<Vec<String>> {
        let body = text.strip_suffix('\n').unwrap_or(text);
        if body.is_empty() {
            return Vec::new();
        }
        body.split('\n')
            .map(|line| {
                line.strip_suffix('\r')
                    .unwrap_or(line)
                    .split('\t')
                    .map(|cell| cell.to_string())
                    .collect()
            })
            .collect()
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
        self.record_edit_undo();
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
        self.record_edit_undo();
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
        self.record_edit_undo();
        let added_idx = self.added_rows.len();
        self.added_rows.push(clone);
        self.added_row_anchors
            .push(self.anchor_for_abs_idx(after_abs_idx));
        self.status_message = None;
        cx.notify();
        Some(added_idx)
    }

    // Checks whether a single row can be safely targeted by a PK-based WHERE
    // clause: a table connection exists, the primary key is loaded and
    // non-empty, and every key column is present in the result. Returns the
    // user-facing note on failure, reusing the wording shown elsewhere.
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
        if result.rows.get(abs_idx).is_none() {
            return Err("Edit kept in grid: row is no longer in the result.".to_string());
        }
        for key in key_columns {
            if !result.columns.iter().any(|col| col == key) {
                return Err(
                    "Edit kept in grid: key column is not in the result; not written.".to_string(),
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
                    this.update(cx, |this, cx| this.set_error(format_query_error(&err), cx))
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
                    this.clear_edit_history();
                    this.status_message = None;
                    this.refresh_table_data(window, cx);
                } else {
                    this.apply_pending_edits_to_result();
                    this.pending_edits.clear();
                    this.clear_edit_history();
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
        self.clear_edit_history();
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
                this.update(cx, |this, cx| this.set_error(format_query_error(&err), cx))
                    .log_err();
                return Ok(());
            }
            this.update_in(cx, |this, window, cx| {
                this.staged_statements.clear();
                this.pending_edits.clear();
                this.deleted_rows.clear();
                this.added_rows.clear();
                this.added_row_anchors.clear();
                this.clear_edit_history();
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
        self.clear_edit_history();
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
            .map(|cell| sql_literal(cell.as_deref()))
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
            timing: None,
            raw_documents: None,
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
            timing: None,
            raw_documents: None,
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

    fn export_xlsx(result: &QueryResult) -> anyhow::Result<Vec<u8>> {
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

        zip.start_file("[Content_Types].xml", opts)?;
        zip.write_all(content_types.as_bytes())?;
        zip.start_file("_rels/.rels", opts)?;
        zip.write_all(rels.as_bytes())?;
        zip.start_file("xl/workbook.xml", opts)?;
        zip.write_all(workbook.as_bytes())?;
        zip.start_file("xl/_rels/workbook.xml.rels", opts)?;
        zip.write_all(workbook_rels.as_bytes())?;
        zip.start_file("xl/styles.xml", opts)?;
        zip.write_all(styles.as_bytes())?;
        zip.start_file("xl/worksheets/sheet1.xml", opts)?;
        zip.write_all(sheet.as_bytes())?;

        Ok(zip.finish()?.into_inner())
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
        let mut numeric_sum = 0.0f64;
        let mut numeric_count = 0usize;
        let mut numeric_min: Option<f64> = None;
        let mut numeric_max: Option<f64> = None;

        for &abs_idx in display_order {
            let cell = result.rows.get(abs_idx).and_then(|row| row.get(col_idx));
            match cell.and_then(|c| c.as_deref()) {
                None => null_count += 1,
                Some(val) => {
                    count += 1;
                    if let Ok(n) = val.parse::<f64>() {
                        numeric_sum += n;
                        numeric_count += 1;
                        numeric_min = Some(numeric_min.map_or(n, |m| m.min(n)));
                        numeric_max = Some(numeric_max.map_or(n, |m| m.max(n)));
                    }
                }
            }
        }

        if numeric_count == count && count > 0 {
            // Every non-null value in this column parsed as a number, so MIN/MAX/SUM/AVG
            // are numerically meaningful; format whole numbers without a trailing ".00".
            let format_numeric = |n: f64| {
                if n.fract() == 0.0 {
                    format!("{n:.0}")
                } else {
                    format!("{n:.2}")
                }
            };
            let min_s = numeric_min
                .map(format_numeric)
                .unwrap_or_else(|| "—".to_string());
            let max_s = numeric_max
                .map(format_numeric)
                .unwrap_or_else(|| "—".to_string());
            let avg = numeric_sum / count as f64;
            format!(
                "COUNT {count} | NULLS {null_count} | MIN {min_s} | MAX {max_s} | SUM {numeric_sum} | AVG {avg:.2}"
            )
        } else {
            format!("COUNT {count} | NULLS {null_count}")
        }
    }

    fn render_status_bar(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let result = self.result.as_ref()?;
        let total_rows = result.rows.len();
        let total_cols = result.columns.len();
        let ms = result.execution_time_ms;

        // The bar is drawn outside the choice of body, so it has to answer for a
        // statement with no result set too, or it goes on counting rows that were
        // never asked for.
        let row_summary = if returns_no_result_set(result, self.base_sql.as_deref()) {
            let (headline, _) = statement_outcome(result);
            headline
        } else {
            format!(
                "{} row{} · {} col{}",
                total_rows,
                if total_rows == 1 { "" } else { "s" },
                total_cols,
                if total_cols == 1 { "" } else { "s" },
            )
        };
        let elapsed_label = format!("{ms}ms");
        let timing_bar = result
            .timing
            .as_ref()
            .and_then(|timing| render_query_timing_bar(timing, cx));

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
                .child(
                    h_flex()
                        .gap_1()
                        .items_center()
                        .when_some(timing_bar, |el, bar| el.child(bar))
                        .child(
                            Label::new(elapsed_label)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                )
                .when_some(fk_button, |el, (label, tip)| {
                    el.child(
                        Button::new("fk-nav", label)
                            .style(cyberpunk::Rank::Quiet.style())
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
                        .style(cyberpunk::Rank::Quiet.style())
                        .icon_size(IconSize::Small)
                        .disabled(is_loading)
                        .tooltip(Tooltip::text("Refresh (apply filter)"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.refresh_table_data(window, cx);
                        })),
                ),
        )
    }

    /// The whole panel for a statement that returns no rows. An empty grid under
    /// "0 rows" reads as a query that found nothing; there is nothing to put in a
    /// grid here, so none is drawn.
    fn render_statement_outcome(&self, _cx: &mut Context<Self>) -> AnyElement {
        let Some(result) = self.result.as_ref() else {
            return div().into_any_element();
        };
        let (headline, detail) = statement_outcome(result);
        v_flex()
            .id("statement-outcome")
            .debug_selector(|| "STATEMENT_OUTCOME".to_string())
            .size_full()
            .items_center()
            .justify_center()
            .gap_1()
            .child(
                Icon::new(IconName::Check)
                    .size(IconSize::Medium)
                    .color(Color::Success),
            )
            .child(Label::new(headline).size(LabelSize::Default))
            .child(
                Label::new(detail)
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .into_any_element()
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

    // Builds (once per distinct failure) the read-only editors the error state
    // renders. Runs in render(), where a Window is available, mirroring
    // `sync_special_view`.
    fn sync_error_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(error) = self.error.clone() else {
            self.error_view = None;
            return;
        };
        if self
            .error_view
            .as_ref()
            .is_none_or(|view| view.source != error)
        {
            let parts = parse_query_error(&error);
            let message = cx.new(|cx| {
                let mut editor = Editor::auto_height(1, ERROR_MESSAGE_MAX_LINES, window, cx);
                editor.set_show_gutter(false, cx);
                editor.set_soft_wrap_mode(SoftWrap::EditorWidth, cx);
                editor.set_show_indent_guides(false, cx);
                editor.disable_mouse_wheel_zoom();
                editor.set_text(parts.headline.as_str(), window, cx);
                editor.set_read_only(true);
                editor
            });
            let detail = parts.detail.as_deref().map(|text| {
                cx.new(|cx| {
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
                })
            });
            self.error_view = Some(QueryErrorView {
                source: error,
                parts,
                message,
                detail,
            });
        }
        let Some((message, detail)) = self
            .error_view
            .as_ref()
            .map(|view| (view.message.clone(), view.detail.clone()))
        else {
            return;
        };
        // An editor caches its style refinement, so these are re-applied every
        // frame: set once at build time they would keep the colors of whichever
        // theme happened to be active then.
        let muted = Color::Muted.color(cx);
        message.update(cx, |editor, _cx| {
            editor.set_text_style_refinement(TextStyleRefinement {
                font_weight: Some(FontWeight::MEDIUM),
                ..Default::default()
            });
        });
        if let Some(detail) = detail {
            detail.update(cx, |editor, _cx| {
                editor.set_text_style_refinement(TextStyleRefinement {
                    color: Some(muted),
                    ..Default::default()
                });
            });
        }
    }

    // The failure state. Laid out as the panel's other states are -- a header
    // row over a top-left aligned body -- so a rejected query reads as
    // information the panel is reporting rather than as an exception screen.
    // The error color marks the state (the header icon, the rule beside the
    // message) and never the prose, which stays at full reading contrast.
    fn render_error(&self, error: &str, cx: &mut Context<Self>) -> AnyElement {
        let Some(view) = self.error_view.as_ref() else {
            return v_flex()
                .size_full()
                .p_3()
                .child(Label::new(error.to_string()).color(Color::Error))
                .into_any_element();
        };
        let border = cx.theme().colors().border;
        let status_mark = cx.theme().status().error;
        let detail_background = cx.theme().colors().editor_background;

        v_flex()
            .size_full()
            .debug_selector(|| "query-error".to_string())
            .child(
                h_flex()
                    .flex_none()
                    .w_full()
                    .px_3()
                    .py_2()
                    .gap_2()
                    .border_b_1()
                    .border_color(border)
                    .child(
                        Icon::new(IconName::XCircle)
                            .size(IconSize::Small)
                            .color(Color::Error),
                    )
                    .child(
                        Label::new("Query failed")
                            .size(LabelSize::Small)
                            .single_line(),
                    )
                    .when_some(view.parts.vendor_code.clone(), |el, code| {
                        el.child(
                            div()
                                .flex_none()
                                .debug_selector({
                                    let code = code.clone();
                                    move || format!("query-error-code:{code}")
                                })
                                .child(Chip::new(format!("Error {code}"))),
                        )
                    })
                    .when_some(view.parts.sqlstate.clone(), |el, sqlstate| {
                        el.child(
                            div()
                                .flex_none()
                                .debug_selector({
                                    let sqlstate = sqlstate.clone();
                                    move || format!("query-error-sqlstate:{sqlstate}")
                                })
                                .child(Chip::new(format!("SQLSTATE {sqlstate}"))),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        div()
                            .id("query-error-copy-hitbox")
                            .flex_none()
                            .debug_selector(|| "query-error-copy".to_string())
                            .child(
                                CopyButton::new("query-error-copy", view.source.clone())
                                    .style(ui::cyberpunk::Rank::Quiet.style())
                                    .tooltip_label("Copy the full error"),
                            ),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .w_full()
                    .min_w_0()
                    .px_3()
                    .pt_3()
                    .pb_2()
                    .child(
                        div()
                            .w_full()
                            .min_w_0()
                            .border_l_2()
                            .border_color(status_mark)
                            .pl_2p5()
                            .debug_selector(|| "query-error-message".to_string())
                            .child(view.message.clone()),
                    ),
            )
            .when_some(view.detail.clone(), |el, detail| {
                el.child(
                    v_flex()
                        .flex_1()
                        .min_h_0()
                        .w_full()
                        .min_w_0()
                        .px_3()
                        .pb_3()
                        .gap_1()
                        .child(
                            Label::new("Driver details")
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_h_0()
                                .w_full()
                                .min_w_0()
                                .p_2()
                                .rounded_md()
                                .border_1()
                                .border_color(border)
                                .bg(detail_background)
                                .debug_selector(|| "query-error-detail".to_string())
                                .child(detail),
                        ),
                )
            })
            .into_any_element()
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
        // A flat `element_selected` fill reads as invisible against the grid's own
        // background here; blend a strongly saturated accent tint into the grid's
        // base color instead so the selection stays clearly visible over the zebra
        // stripes in both light and dark themes.
        let selection_bg = cx
            .theme()
            .colors()
            .editor_background
            .blend(cx.theme().colors().text_accent.opacity(0.32));
        let active_cell_border = cx.theme().colors().border_focused;
        let search_match_bg = cx.theme().colors().search_match_background;
        let active_line_bg = cx.theme().colors().editor_active_line_background;
        let row_selected_bg = cx.theme().colors().editor_highlighted_line_background;
        let heatmap_base = cx.theme().colors().editor_background;
        let heatmap_tint = cx.theme().colors().text_accent;
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
            let is_active = self.selected_cell == Some((abs_idx, cell_idx));
            let is_selected =
                is_active || self.selected_cell_range_contains(abs_idx, display_idx, cell_idx);
            let is_modified = self.pending_edits.contains_key(&(abs_idx, cell_idx));
            let editing = self
                .cell_edit
                .as_ref()
                .filter(|edit| edit.abs_idx == abs_idx && edit.col_idx == cell_idx);
            let cell_body: AnyElement = if let Some(edit) = editing {
                let editor = edit.editor.clone();
                div()
                    // Unlike the read-mode branch (an h_flex with size_full at
                    // its top level), this wraps render_cell_editor in an extra
                    // div for key capture. Without an explicit size here, that
                    // wrapper doesn't establish a definite size for its child's
                    // size_full to fill, so the live editor paints with zero
                    // effective area even though its text is set correctly.
                    .size_full()
                    .debug_selector(|| "CELL_EDITOR_BODY".into())
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
                            // Raw fallback: only reached when nothing resolved
                            // "escape" to a bound action first (e.g. no keymap
                            // loaded). The real app's keymap binds "escape" to
                            // the `editor::Cancel` action instead, which is why
                            // `capture_action` below also handles it.
                            "escape" => this.cancel_cell_edit(window, cx),
                            _ => {}
                        }
                    }))
                    // When the real app's keymap resolves "escape" to the
                    // `editor::Cancel` action instead of a raw keystroke, it
                    // never reaches the raw fallback above: the inner Editor's
                    // own Cancel handler claims the action first and
                    // re-propagates it (finding nothing internal to dismiss),
                    // but with no listener catching that re-propagated action,
                    // nothing ever cancelled the cell edit. Intercepting during
                    // the action capture phase (before the Editor's own handler
                    // runs) and stopping propagation is what actually cancels it.
                    .capture_action(
                        cx.listener(|this, _: &editor::actions::Cancel, window, cx| {
                            this.cancel_cell_edit(window, cx);
                            cx.stop_propagation();
                        }),
                    )
                    .child(Self::render_cell_editor(editor))
                    .into_any_element()
            } else if matches!(self.column_kind_at(cell_idx), CellEditorKind::Boolean) {
                let cell_val = match self.pending_cell_value(abs_idx, cell_idx) {
                    Some(cv) => cv.clone(),
                    None => CellValue::from_loaded(&self.loaded_cell_value(abs_idx, cell_idx)),
                };
                let (display, color) = bool_cell_display(&cell_val);
                let label = Label::new(display.clone())
                    .size(LabelSize::Small)
                    .color(if is_deleted { Color::Muted } else { color })
                    .single_line()
                    .when(
                        display == NULL_MARKER || display == DEFAULT_MARKER,
                        |label| label.italic(),
                    )
                    .when(is_deleted, |label| label.strikethrough())
                    .into_any_element();
                Self::render_typed_cell_body(
                    label,
                    false,
                    format!("CELL_TEXT-{abs_idx}-{cell_idx}"),
                )
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
                    .single_line()
                    .when(
                        display == NULL_MARKER || display == DEFAULT_MARKER,
                        |label| label.italic(),
                    )
                    .when(is_deleted, |label| label.strikethrough())
                    .into_any_element();
                let align_right = self.numeric_columns.get(cell_idx).copied().unwrap_or(false);
                Self::render_typed_cell_body(
                    label,
                    align_right,
                    format!("CELL_TEXT-{abs_idx}-{cell_idx}"),
                )
            };
            let is_find_match = self.find_query.as_ref().is_some_and(|q| !q.is_empty())
                && self.find_matches.contains(&(abs_idx, cell_idx));
            let is_current_find = is_find_match
                && self.find_matches.get(self.find_current) == Some(&(abs_idx, cell_idx));
            let cell_display_idx = display_idx;
            let heatmap_bg = self.heatmap_cell_bg(
                cell_idx,
                match self.pending_cell_value(abs_idx, cell_idx) {
                    Some(CellValue::Text(text)) => Some(text.as_str()),
                    Some(CellValue::Null) | Some(CellValue::Default) => None,
                    None => self
                        .result
                        .as_ref()
                        .and_then(|result| result.rows.get(abs_idx))
                        .and_then(|row| row.get(cell_idx))
                        .and_then(|cell| cell.as_deref()),
                },
                heatmap_base,
                heatmap_tint,
            );

            let cell_div = div()
                .id(ElementId::from(SharedString::from(format!(
                    "cell-{abs_idx}-{cell_idx}"
                ))))
                .debug_selector(move || format!("CELL-{abs_idx}-{cell_idx}"))
                .px_1p5()
                .h(px(Self::GRID_ROW_H))
                .w(width)
                .flex_none()
                .flex()
                .items_center()
                .border_r_1()
                .border_color(grid_border)
                .overflow_hidden()
                // Lowest priority: the heatmap tint is a base data indicator, so any
                // higher-priority highlight below overwrites it by being applied later.
                .when_some(heatmap_bg, move |this, bg| this.bg(bg))
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
                .when(
                    self.cell_receives_selection_tint(abs_idx, display_idx, cell_idx),
                    |this| this.bg(selection_bg),
                )
                .when(is_deleted, |this| this.bg(deleted_bg))
                .when(is_active, |this| {
                    this.border_1().border_color(active_cell_border)
                })
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
                        let wt_empty = wt.clone();
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
                            .entry("Set Empty Value", None, move |_, cx| {
                                wt_empty
                                    .update(cx, |this, cx| {
                                        this.selected_cell = Some((abs_idx, cell_idx));
                                        this.set_selected_cell_value(
                                            CellValue::Text(String::new()),
                                            cx,
                                        );
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
        // See render_grid_row's identical comment: a flat element_selected fill is
        // invisible here, so blend a strongly saturated accent tint into the grid's
        // base color instead.
        let selection_bg = cx
            .theme()
            .colors()
            .editor_background
            .blend(cx.theme().colors().text_accent.opacity(0.32));
        let active_cell_border = cx.theme().colors().border_focused;
        let heatmap_base = cx.theme().colors().editor_background;
        let heatmap_tint = cx.theme().colors().text_accent;
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
            let is_active = self.selected_cell == Some((abs_idx, cell_idx));
            let is_selected =
                is_active || self.selected_cell_range_contains(abs_idx, display_idx, cell_idx);
            let editing = self
                .cell_edit
                .as_ref()
                .filter(|edit| edit.abs_idx == abs_idx && edit.col_idx == cell_idx);
            let cell_body: AnyElement = if let Some(edit) = editing {
                let editor = edit.editor.clone();
                div()
                    // See the equivalent branch in the non-transposed grid:
                    // without size_full here, this wrapper doesn't establish a
                    // definite size for the editor's own size_full to fill, so
                    // it paints with zero effective area despite having the
                    // correct text set.
                    .size_full()
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
                            // Raw fallback: only reached when nothing resolved
                            // "escape" to a bound action first (e.g. no keymap
                            // loaded). The real app's keymap binds "escape" to
                            // the `editor::Cancel` action instead, which is why
                            // `capture_action` below also handles it.
                            "escape" => this.cancel_cell_edit(window, cx),
                            _ => {}
                        }
                    }))
                    // See the equivalent branch in the non-transposed grid: Escape
                    // resolves to the `editor::Cancel` action, not a raw keystroke,
                    // so it must be intercepted during the action capture phase.
                    .capture_action(
                        cx.listener(|this, _: &editor::actions::Cancel, window, cx| {
                            this.cancel_cell_edit(window, cx);
                            cx.stop_propagation();
                        }),
                    )
                    .child(Self::render_cell_editor(editor))
                    .into_any_element()
            } else if matches!(self.column_kind_at(cell_idx), CellEditorKind::Boolean) {
                let cell_val = row.get(cell_idx).cloned().unwrap_or(CellValue::Null);
                let (display, color) = bool_cell_display(&cell_val);
                let label = Label::new(display)
                    .size(LabelSize::Small)
                    .color(color)
                    .single_line()
                    .into_any_element();
                Self::render_typed_cell_body(
                    label,
                    false,
                    format!("ADDED_CELL_TEXT-{added_idx}-{cell_idx}"),
                )
            } else {
                let (display, color) = match row.get(cell_idx) {
                    Some(value) => render_cell_value(value),
                    None => (NULL_MARKER.to_string(), Color::Muted),
                };
                let label = Label::new(display)
                    .size(LabelSize::Small)
                    .color(color)
                    .single_line()
                    .into_any_element();
                let align_right = self.numeric_columns.get(cell_idx).copied().unwrap_or(false);
                Self::render_typed_cell_body(
                    label,
                    align_right,
                    format!("ADDED_CELL_TEXT-{added_idx}-{cell_idx}"),
                )
            };
            let cell_display_idx = display_idx;
            let heatmap_bg = self.heatmap_cell_bg(
                cell_idx,
                row.get(cell_idx).and_then(|value| match value {
                    CellValue::Text(text) => Some(text.as_str()),
                    CellValue::Null | CellValue::Default => None,
                }),
                heatmap_base,
                heatmap_tint,
            );
            cells.push(
                div()
                    .id(ElementId::from(SharedString::from(format!(
                        "added-cell-{added_idx}-{cell_idx}"
                    ))))
                    .debug_selector(move || format!("ADDED_CELL-{added_idx}-{cell_idx}"))
                    .px_1p5()
                    .h(px(Self::GRID_ROW_H))
                    .w(width)
                    .flex_none()
                    .flex()
                    .items_center()
                    .border_r_1()
                    .border_color(grid_border)
                    .overflow_hidden()
                    .when_some(heatmap_bg, move |this, bg| this.bg(bg))
                    .when(
                        self.cell_receives_selection_tint(abs_idx, display_idx, cell_idx),
                        |this| this.bg(selection_bg),
                    )
                    .when(is_active, |this| {
                        this.border_1().border_color(active_cell_border)
                    })
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
        let mongo_mode = self.active_mongo_view();
        let transposed = self.transposed;
        let chart_open = self.chart_open;
        let heatmap_enabled = self.heatmap_enabled;
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
                                        .style(cyberpunk::Rank::Quiet.style())
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
                                .style(cyberpunk::Rank::Quiet.style())
                                .label_size(LabelSize::Small),
                                Tooltip::text("Format used by Ctrl+C"),
                            ),
                    )
                    .when(has_selected_cell, |el| {
                        el.child(Divider::vertical())
                        .when(selected_col_nullable, |el| {
                            el.child(
                                Button::new("set-null", "Set NULL")
                                    .style(cyberpunk::Rank::Quiet.style())
                                    .tooltip(Tooltip::text("Set the selected cell to NULL (Ctrl+Alt+N / Cmd+Alt+N)"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.set_selected_cell_value(CellValue::Null, cx);
                                    })),
                            )
                        })
                        .when(selected_col_has_default, |el| {
                            el.child(
                                Button::new("set-default", "Set DEFAULT")
                                    .style(cyberpunk::Rank::Quiet.style())
                                    .tooltip(Tooltip::text("Set the selected cell to the column DEFAULT (Ctrl+Alt+D / Cmd+Alt+D)"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.set_selected_cell_value(CellValue::Default, cx);
                                    })),
                            )
                        })
                        .child(
                            Button::new("set-empty-value", "Set Empty Value")
                                .style(cyberpunk::Rank::Quiet.style())
                                .tooltip(Tooltip::text("Set the selected cell to an explicit empty string (Ctrl+Alt+E / Cmd+Alt+E)"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.set_selected_cell_value(CellValue::Text(String::new()), cx);
                                })),
                        )
                    })
                    .when(pending_count > 0, |el| {
                        el.child(Divider::vertical())
                        .child(
                            Button::new("submit-edits", "Submit")
                                .style(cyberpunk::Rank::Accent.style())
                                .tooltip(Tooltip::text("Write pending changes to the database"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.submit_pending_edits(window, cx);
                                })),
                        )
                        .child(
                            Button::new("preview-pending", "Preview")
                                .style(cyberpunk::Rank::Quiet.style())
                                .tooltip(Tooltip::text("Preview the SQL these changes will run"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.preview_open = !this.preview_open;
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new("revert-edits", "Revert")
                                .style(cyberpunk::Rank::Quiet.style())
                                .tooltip(Tooltip::text("Discard pending changes"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.revert_pending_edits(cx);
                                })),
                        )
                    })
                    .child(Divider::vertical())
                    .child(
                        Button::new("transaction-mode", transaction_mode.label())
                            .style(cyberpunk::Rank::Quiet.style())
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
                                .style(cyberpunk::Rank::Accent.style())
                                .label_size(LabelSize::Small)
                                .tooltip(Tooltip::text("Run the staged statements as one transaction"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.commit_transaction(window, cx);
                                })),
                        )
                        .child(
                            Button::new("rollback-transaction", "Roll Back")
                                .style(cyberpunk::Rank::Quiet.style())
                                .label_size(LabelSize::Small)
                                .tooltip(Tooltip::text("Discard the staged statements"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.rollback_transaction(cx);
                                })),
                        )
                    })
                    .child(Divider::vertical())
                    .child(cyberpunk::segmented(vec![
                        IconButton::new("toggle-local-filters", IconName::Filter)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Toggle column filters (Ctrl+F5)"))
                            .toggle_state(self.local_filter_visible)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.toggle_local_filter_row(window, cx);
                            }))
                            .into_any_element(),
                        IconButton::new("toggle-column-list", IconName::ListTree)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Show/hide columns (Ctrl+F12)"))
                            .toggle_state(self.column_list_visible)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.toggle_column_list(cx);
                            }))
                            .into_any_element(),
                    ]))
                    .when_some(mongo_mode, |el, mode| {
                        el.child(Divider::vertical())
                            .child(self.render_mongo_view_toggle(mode, cx))
                    })
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
                                        .action_checked("Heatmap", Box::new(ToggleHeatmap), heatmap_enabled)
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
                                    .style(cyberpunk::Rank::Quiet.style())
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
                                                            if let Some(bytes) = ResultView::export_xlsx(&result).log_err() {
                                                                std::fs::write(path, bytes).log_err();
                                                            }
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
                                    .style(cyberpunk::Rank::Quiet.style())
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
                // Accent tint marking the active cell's column header (spreadsheet
                // cross-highlight). Distinct from the sorted-column shade.
                let active_header_bg =
                    header_bg.blend(cx.theme().colors().text_accent.opacity(0.18));
                let active_col = self.active_cell_column();
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
                    let is_active_col = active_col == Some(col_idx);
                    // Numeric columns render their data right-aligned; right-aligning
                    // the header too keeps it visually anchored over its column
                    // instead of floating at the opposite edge on wide columns.
                    let is_numeric_header = self.numeric_columns.get(col_idx).copied().unwrap_or(false);
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
                            .px_1p5()
                            .h(px(Self::GRID_HEADER_H))
                            .w(width)
                            .flex_none()
                            .flex()
                            .relative()
                            .items_center()
                            .border_r_1()
                            .border_color(strong_grid_border)
                            .bg(header_bg)
                            .overflow_hidden()
                            .cursor_pointer()
                            .hover(move |style| style.bg(header_cell_hover_bg))
                            .when(is_sorted, move |this| this.bg(sorted_header_bg))
                            .when(is_active_col, move |this| this.bg(active_header_bg))
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
                            .when(is_numeric_header, |el| el.justify_end())
                            // The column's own edge, to be dragged. Narrow, on the
                            // seam between two columns, and the press stops here so
                            // that taking hold of it does not also sort the column.
                            // A double click gives the column back the width its own
                            // rows suggest, which is the way out of having dragged it
                            // too narrow to read.
                            .child(
                                div()
                                    .id(ElementId::from(SharedString::from(format!(
                                        "col-resize-{col_idx}"
                                    ))))
                                    .debug_selector(move || format!("COL_RESIZE-{col_idx}"))
                                    .absolute()
                                    .right_0()
                                    .top_0()
                                    .h_full()
                                    .w(px(6.0))
                                    .cursor_ew_resize()
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                                            cx.stop_propagation();
                                            if event.click_count >= 2 {
                                                this.fit_column_to_its_rows(display_pos);
                                            } else {
                                                this.begin_column_resize(
                                                    display_pos,
                                                    f32::from(event.position.x),
                                                );
                                            }
                                            cx.notify();
                                        }),
                                    ),
                            )
                            .child(
                                h_flex()
                                    .debug_selector(move || format!("COL_HEADER_CONTENT-{col_idx}"))
                                    .gap_1()
                                    .items_center()
                                    // flex_none keeps this at its natural width when the
                                    // parent switches to justify_end above -- without it,
                                    // a flex child is shrinkable and long header names
                                    // wrap onto two lines instead of just moving right
                                    // (the same class of bug fixed for numeric cells in
                                    // commit d0084bff98).
                                    .when(is_numeric_header, |el| el.flex_none())
                                    .child(
                                        Label::new(format!("{}{}", col, sort_label))
                                            .size(LabelSize::Small)
                                            .single_line()
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

                // `overflow_x_scroll` (not `overflow_x_hidden`) is required here:
                // GPUI's wheel-scroll listener only moves a tracked scroll
                // handle's offset on an axis whose overflow is `Overflow::Scroll`
                // (see `paint_scroll_listener` in gpui's div.rs) — `Hidden` still
                // clips content but silently drops all wheel/trackpad deltas on
                // that axis, leaving only the manual scrollbar-thumb drag able to
                // move it.
                let mut h_scroll_div = div()
                    .id("result-grid")
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .h_full()
                    .overflow_x_scroll()
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
                let gutter_active_bg = gutter_bg.blend(cx.theme().colors().element_selected.opacity(0.5));
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
                            let is_active_row = this.active_cell_row() == Some(abs_idx);
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
                                .when(is_active_row && !is_selected, move |el| {
                                    el.bg(gutter_active_bg)
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
                    .when_some(self.render_column_resize_overlay(cx), |el, overlay| {
                        el.child(overlay)
                    })
                    .when_some(self.render_enum_popup(cx), |el, popup| el.child(popup))
                    .when_some(self.render_date_popup(cx), |el, popup| el.child(popup))
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
        let heatmap_base = cx.theme().colors().editor_background;
        let heatmap_tint = cx.theme().colors().text_accent;

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
                        // Mirror the normal grid's resolution order (pending
                        // edit wins over the loaded value) so toggling
                        // Transpose never hides an in-progress edit.
                        let (display, color) = match self.pending_cell_value(record_idx, col_idx) {
                            Some(value) => render_cell_value(value),
                            None => render_loaded_value(
                                result
                                    .rows
                                    .get(record_idx)
                                    .and_then(|row| row.get(col_idx))
                                    .and_then(|cell| cell.as_deref()),
                            ),
                        };
                        let heatmap_bg = self.heatmap_cell_bg(
                            col_idx,
                            match self.pending_cell_value(record_idx, col_idx) {
                                Some(CellValue::Text(text)) => Some(text.as_str()),
                                Some(CellValue::Null) | Some(CellValue::Default) => None,
                                None => result
                                    .rows
                                    .get(record_idx)
                                    .and_then(|row| row.get(col_idx))
                                    .and_then(|cell| cell.as_deref()),
                            },
                            heatmap_base,
                            heatmap_tint,
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
                            .when_some(heatmap_bg, move |this, bg| this.bg(bg))
                            .debug_selector({
                                let display = display.clone();
                                move || format!("TCELL-{record_idx}-{col_idx}-{display}")
                            })
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
        if raw_text.is_empty() {
            self.value_editor_open = false;
            self.value_editor_resize_drag = None;
            cx.notify();
            return;
        }
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
        self.heatmap_enabled = false;
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
        // The transposed grid has no edges to drag, and coming back must not
        // find a drag from before still in hand.
        self.column_resize = None;
        cx.notify();
    }

    fn toggle_heatmap(&mut self, cx: &mut Context<Self>) {
        self.heatmap_enabled = !self.heatmap_enabled;
        cx.notify();
    }

    // The tint a heatmapped numeric cell should paint, or None when heatmap mode
    // is off, the column has no known range, or the cell's own value doesn't
    // parse as a number (e.g. NULL). Takes the base/tint colors already resolved
    // from the theme by the caller so this stays callable from inside the nested
    // per-row/per-column closures that build the grid, without threading `cx`
    // (and its borrow-checker friction) through every closure layer.
    fn heatmap_cell_bg(
        &self,
        col_idx: usize,
        value: Option<&str>,
        base: gpui::Hsla,
        tint: gpui::Hsla,
    ) -> Option<gpui::Hsla> {
        if !self.heatmap_enabled {
            return None;
        }
        let (min, max) = (*self.heatmap_ranges.get(col_idx)?)?;
        let value: f64 = value?.parse().ok()?;
        let ratio = heatmap_ratio(value, min, max);
        Some(base.blend(tint.opacity(0.05 + ratio * 0.25)))
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
                                    .style(cyberpunk::Rank::Quiet.style())
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
                                    .style(cyberpunk::Rank::Accent.style())
                                    .label_size(LabelSize::Small)
                                    .tooltip(Tooltip::text("Apply to pending edits (Ctrl+Enter)"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.commit_value_editor(window, cx);
                                    })),
                            )
                        })
                        .child(cyberpunk::segmented(vec![
                            IconButton::new("value-editor-copy", IconName::Copy)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text("Copy full value"))
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(value.clone()));
                                }))
                                .into_any_element(),
                            IconButton::new("value-editor-close", IconName::Close)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text("Close value panel"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.value_editor_open = false;
                                    this.value_editor_resize_drag = None;
                                    cx.notify();
                                }))
                                .into_any_element(),
                        ])),
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

    /// While a column's edge is held, the pointer is followed here rather than
    /// on the edge itself: a drag leaves the six pixels it started in at once.
    fn render_column_resize_overlay(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.column_resize?;

        Some(
            div()
                .absolute()
                .inset_0()
                .cursor_ew_resize()
                .debug_selector(|| "COL_RESIZE_OVERLAY".to_string())
                .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                    cx.stop_propagation();
                    if event.pressed_button != Some(MouseButton::Left) {
                        this.end_column_resize();
                        cx.notify();
                        return;
                    }
                    this.update_column_resize(f32::from(event.position.x));
                    cx.notify();
                }))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseUpEvent, _, cx| {
                        cx.stop_propagation();
                        this.end_column_resize();
                        cx.notify();
                    }),
                )
                .on_mouse_up_out(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseUpEvent, _, cx| {
                        this.end_column_resize();
                        cx.notify();
                    }),
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
                    .child(cyberpunk::segmented(vec![
                        IconButton::new("rv-prev", IconName::ArrowLeft)
                            .icon_size(IconSize::Small)
                            .disabled(at_first)
                            .tooltip(Tooltip::text("Previous row"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.record_view_step(-1, cx);
                            }))
                            .into_any_element(),
                        IconButton::new("rv-next", IconName::ArrowRight)
                            .icon_size(IconSize::Small)
                            .disabled(at_last)
                            .tooltip(Tooltip::text("Next row"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.record_view_step(1, cx);
                            }))
                            .into_any_element(),
                        IconButton::new("rv-close", IconName::Close)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Close record view"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.record_view_open = false;
                                cx.notify();
                            }))
                            .into_any_element(),
                    ])),
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
                            .style(cyberpunk::Rank::Neutral.style())
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
        let quote = self.identifier_quote(cx);
        let escaped = value.replace('\'', "''");
        let sql = format!(
            "SELECT * FROM {} WHERE {} = '{}'",
            quote_identifier(quote, &fk.to_table),
            quote_identifier(quote, &fk.to_column),
            escaped
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
            .child(cyberpunk::segmented(vec![
                IconButton::new("find-prev", IconName::ArrowUp)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Previous match (Shift+Ctrl+G)"))
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.find_previous(cx);
                    }))
                    .into_any_element(),
                IconButton::new("find-next", IconName::ArrowDown)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Next match (Ctrl+G)"))
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.find_next(cx);
                    }))
                    .into_any_element(),
            ]))
            .child(div().w(gpui::px(200.0)).child(editor))
            .child(
                Label::new(current_label)
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                Button::new("find-filter-rows", "Filter rows")
                    .style(cyberpunk::Rank::Quiet.style())
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
                    .style(cyberpunk::Rank::Neutral.style())
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
                    .style(cyberpunk::Rank::Neutral.style())
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
                                        .style(cyberpunk::Rank::Accent.style())
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
                                        .style(cyberpunk::Rank::Neutral.style())
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
                                .style(cyberpunk::Rank::Neutral.style())
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
                                .style(cyberpunk::Rank::Neutral.style())
                                .label_size(LabelSize::Small)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.export_to_clipboard(cx);
                                })),
                        )
                        .child(
                            Button::new("export-file", "Save to File…")
                                .style(cyberpunk::Rank::Neutral.style())
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
                            .style(cyberpunk::Rank::Neutral.style())
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
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let raw_points: Vec<(String, String)> = result
            .rows
            .iter()
            .filter_map(|row| {
                let lat_value = row.get(lat)?.clone()?;
                let lon_value = row.get(lon)?.clone()?;
                Some((lat_value, lon_value))
            })
            .collect();
        let plotted: Vec<(f32, f32)> = raw_points
            .iter()
            .filter_map(|(lat_value, lon_value)| {
                let lat_num: f64 = lat_value.trim().parse().ok()?;
                let lon_num: f64 = lon_value.trim().parse().ok()?;
                project_lat_lon(lat_num, lon_num)
            })
            .collect();
        let plotted_count = plotted.len();

        let point_color = cx.theme().status().info;
        let grid_color = cx.theme().colors().border;
        let plot = div()
            .flex_1()
            .w_full()
            .min_h(px(160.))
            .border_1()
            .border_color(grid_color)
            .child(
                gpui::canvas(
                    move |_, _, _| {},
                    move |bounds, _, window, _| {
                        let origin = bounds.origin;
                        let width = bounds.size.width;
                        let height = bounds.size.height;

                        // Gridlines every 60° longitude / 30° latitude, as a
                        // coarse offline frame of reference for the plot.
                        for lon_deg in (-180..=180).step_by(60) {
                            let x = origin.x + width * ((lon_deg as f32 + 180.0) / 360.0);
                            let mut builder = gpui::PathBuilder::stroke(px(1.0));
                            builder.move_to(gpui::point(x, origin.y));
                            builder.line_to(gpui::point(x, origin.y + height));
                            if let Ok(path) = builder.build() {
                                window.paint_path(path, grid_color);
                            }
                        }
                        for lat_deg in (-90..=90).step_by(30) {
                            let y = origin.y + height * ((90.0 - lat_deg as f32) / 180.0);
                            let mut builder = gpui::PathBuilder::stroke(px(1.0));
                            builder.move_to(gpui::point(origin.x, y));
                            builder.line_to(gpui::point(origin.x + width, y));
                            if let Ok(path) = builder.build() {
                                window.paint_path(path, grid_color);
                            }
                        }

                        const DOT_SIZE: f32 = 4.0;
                        for (x_frac, y_frac) in &plotted {
                            let center = gpui::point(
                                origin.x + width * *x_frac,
                                origin.y + height * *y_frac,
                            );
                            let dot_bounds = gpui::Bounds {
                                origin: gpui::point(
                                    center.x - px(DOT_SIZE / 2.0),
                                    center.y - px(DOT_SIZE / 2.0),
                                ),
                                size: gpui::size(px(DOT_SIZE), px(DOT_SIZE)),
                            };
                            window.paint_quad(gpui::fill(dot_bounds, point_color));
                        }
                    },
                )
                .size_full(),
            );

        let skipped = raw_points.len() - plotted_count;
        let summary = if skipped > 0 {
            format!(
                "{} geo points ({skipped} skipped: invalid or out of range)",
                raw_points.len()
            )
        } else {
            format!("{} geo points", raw_points.len())
        };
        let rows: Vec<AnyElement> = raw_points
            .iter()
            .take(500)
            .map(|(lat_value, lon_value)| {
                Label::new(SharedString::from(format!("{lat_value}, {lon_value}")))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .into_any_element()
            })
            .collect();
        v_flex()
            .id("chart-geo-map")
            .flex_1()
            .gap_2()
            .child(Label::new(SharedString::from(summary)).size(LabelSize::Small))
            .child(plot)
            .child(
                v_flex()
                    .id("chart-geo-list")
                    .max_h(px(96.))
                    .gap_1()
                    .overflow_y_scroll()
                    .children(rows),
            )
            .into_any_element()
    }

    // Approximate row/header heights for enum popup positioning (px).
    // These match the py_1 + LabelSize::Small layout used throughout the grid.
    const GRID_HEADER_H: f32 = 24.0;
    const GRID_ROW_H: f32 = 22.0;

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

    // Renders the calendar popup for an open DATE/DATETIME cell editor. Uses the
    // same absolute-positioning technique as `render_enum_popup` (anchored to the
    // edited cell's column/row via `column_edges` and the current scroll offset)
    // so both popups sit consistently next to the cell they edit.
    fn render_date_popup(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let popup = self.date_popup.as_ref()?;
        let (_, _, _, scroll_y) = axis_metrics(&self.scroll_handle, true);

        let col_x = self
            .column_edges
            .get(popup.col_idx.saturating_sub(1))
            .copied()
            .unwrap_or(0.0);
        let screen_x = (col_x + f32::from(self.h_scroll.offset().x)).max(0.0);
        let screen_y =
            (Self::GRID_HEADER_H + popup.abs_idx as f32 * Self::GRID_ROW_H + scroll_y).max(0.0);

        let year = popup.display_year;
        let month = popup.display_month;

        let header_id = format!("DATE_POPUP_HEADER-{year:04}-{:02}", month as u8);
        let header = h_flex()
            .items_center()
            .justify_between()
            .gap_2()
            .px_1()
            .pb_1()
            .child(
                div()
                    .id("date-popup-prev")
                    .debug_selector(|| "date-popup-prev".to_string())
                    .cursor_pointer()
                    .hover(|el| el.bg(cx.theme().colors().element_hover))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.date_popup_shift_month(false, cx);
                    }))
                    .child(Icon::new(IconName::ChevronLeft).size(IconSize::Small)),
            )
            .child(
                div()
                    .debug_selector(move || header_id)
                    .child(Label::new(format!("{month} {year}")).size(LabelSize::Small)),
            )
            .child(
                div()
                    .id("date-popup-next")
                    .debug_selector(|| "date-popup-next".to_string())
                    .cursor_pointer()
                    .hover(|el| el.bg(cx.theme().colors().element_hover))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.date_popup_shift_month(true, cx);
                    }))
                    .child(Icon::new(IconName::ChevronRight).size(IconSize::Small)),
            );

        let weekday_row =
            h_flex().children(["Su", "Mo", "Tu", "We", "Th", "Fr", "Sa"].into_iter().map(
                |label| {
                    div()
                        .w(gpui::px(28.0))
                        .items_center()
                        .flex()
                        .justify_center()
                        .child(
                            Label::new(label)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                        .into_any_element()
                },
            ));

        let first_of_month = time::Date::from_calendar_date(year, month, 1).ok();
        let leading_blanks = first_of_month
            .map(|date| date.weekday().number_days_from_sunday() as usize)
            .unwrap_or(0);
        let days_in_month = month.length(year);

        let mut day_cells: Vec<AnyElement> = (0..leading_blanks)
            .map(|_| div().w(gpui::px(28.0)).h(gpui::px(24.0)).into_any_element())
            .collect();
        for day in 1..=days_in_month {
            let cell_id = format!("DATE_POPUP_DAY-{year:04}-{:02}-{day:02}", month as u8);
            day_cells.push(
                div()
                    .id(SharedString::from(cell_id.clone()))
                    .debug_selector(move || cell_id)
                    .w(gpui::px(28.0))
                    .h(gpui::px(24.0))
                    .items_center()
                    .flex()
                    .justify_center()
                    .cursor_pointer()
                    .hover(|el| el.bg(cx.theme().colors().element_hover))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if let Ok(date) = time::Date::from_calendar_date(year, month, day) {
                            this.apply_date_selection(date, window, cx);
                        }
                    }))
                    .child(Label::new(day.to_string()).size(LabelSize::Small))
                    .into_any_element(),
            );
        }

        let mut week_rows: Vec<AnyElement> = Vec::new();
        let mut current_week: Vec<AnyElement> = Vec::new();
        for cell in day_cells {
            current_week.push(cell);
            if current_week.len() == 7 {
                week_rows.push(
                    h_flex()
                        .children(std::mem::take(&mut current_week))
                        .into_any_element(),
                );
            }
        }
        if !current_week.is_empty() {
            week_rows.push(h_flex().children(current_week).into_any_element());
        }

        let backdrop = div().absolute().inset_0().on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _, _, cx| {
                this.date_popup = None;
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
                    popup_surface(cx)
                        .id("date-popup")
                        .debug_selector(|| "DATE_POPUP".to_string())
                        .absolute()
                        .left(gpui::px(0.0))
                        .top(gpui::px(0.0))
                        .p_2()
                        .child(header)
                        .child(weekday_row)
                        .children(week_rows),
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
    let thumb_len = ((viewport_len * viewport_len) / content_len)
        .clamp(SCROLLBAR_THUMB_MIN.min(viewport_len), viewport_len);
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
    let thumb_len = ((viewport_len * viewport_len) / content_len)
        .clamp(SCROLLBAR_THUMB_MIN.min(viewport_len), viewport_len);
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
// predicate uniquely identifies the row.
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
// targeted: a missing key column. The caller must already have rejected an
// empty primary key.
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
        key_predicate.push((key.clone(), value));
    }
    Ok(key_predicate)
}

// Turns the pending-edit buffer into one UPDATE per changed cell. Pure and
// testable. Returns Err with a human note for the first cell that cannot be
// safely targeted (no primary key or a missing key column), so the caller can
// surface it and keep the buffer intact. `edits` are `((absolute row,
// column), new value)` and should be pre-sorted for a stable statement order.
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
// row.
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

    fn tab_background_color(&self, _cx: &App) -> Option<gpui::Hsla> {
        self.env_accent.map(|accent| accent.opacity(0.12))
    }
}

impl Focusable for ResultView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ResultView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
        self.sync_mongo_documents_view(window, cx);
        self.sync_error_view(window, cx);
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
            self.render_error(&error, cx)
        } else if self
            .result
            .as_ref()
            .is_some_and(|result| returns_no_result_set(result, self.base_sql.as_deref()))
        {
            self.render_statement_outcome(cx)
        } else if self.result.is_some() {
            if self.active_special() != SpecialResult::None {
                self.render_special_view(cx).into_any_element()
            } else if self.active_mongo_view() == Some(MongoResultView::Documents) {
                self.render_mongo_documents_view(cx).into_any_element()
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
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                // Ready-mode navigation/edit-entry. Bubbled key events still reach
                // this handler while a cell editor, the find box, or any other
                // inline editor owns focus -- checking only `cell_edit.is_some()`
                // let a printable keystroke fall through to type-to-replace even
                // while the find editor was focused, silently starting a cell edit
                // instead of typing the find query. Requiring this view's own
                // focus handle to be exactly focused covers every inline editor,
                // not just the cell editor.
                if !this.focus_handle.is_focused(window) {
                    return;
                }
                let keystroke = &event.keystroke;
                let modifiers = keystroke.modifiers;
                let plain = !modifiers.modified();
                let only_shift =
                    modifiers.shift && !modifiers.control && !modifiers.platform && !modifiers.alt;
                // Primary shortcut modifier: Ctrl on Linux/Windows, Cmd on macOS.
                let primary = (modifiers.control || modifiers.platform)
                    && !modifiers.shift
                    && !modifiers.alt
                    && !modifiers.function;
                match keystroke.key.as_str() {
                    "up" if only_shift => {
                        this.extend_selection(0, -1, cx);
                        cx.stop_propagation();
                        return;
                    }
                    "down" if only_shift => {
                        this.extend_selection(0, 1, cx);
                        cx.stop_propagation();
                        return;
                    }
                    "left" if only_shift => {
                        this.extend_selection(-1, 0, cx);
                        cx.stop_propagation();
                        return;
                    }
                    "right" if only_shift => {
                        this.extend_selection(1, 0, cx);
                        cx.stop_propagation();
                        return;
                    }
                    "a" if primary => {
                        this.select_all_cells(cx);
                        cx.stop_propagation();
                        return;
                    }
                    "v" if primary => {
                        this.paste_from_clipboard(cx);
                        cx.stop_propagation();
                        return;
                    }
                    "x" if primary => {
                        this.cut_selection(cx);
                        cx.stop_propagation();
                        return;
                    }
                    "d" if primary => {
                        this.fill_down(cx);
                        cx.stop_propagation();
                        return;
                    }
                    "r" if primary => {
                        this.fill_right(cx);
                        cx.stop_propagation();
                        return;
                    }
                    "z" if primary => {
                        this.undo_edit(cx);
                        cx.stop_propagation();
                        return;
                    }
                    "y" if primary => {
                        this.redo_edit(cx);
                        cx.stop_propagation();
                        return;
                    }
                    "f2" if plain => {
                        this.begin_edit_active_cell(CellEditEntry::CursorEnd, window, cx);
                        cx.stop_propagation();
                        return;
                    }
                    "up" if plain => {
                        this.move_active_cell(0, -1, cx);
                        cx.stop_propagation();
                        return;
                    }
                    "down" if plain => {
                        this.move_active_cell(0, 1, cx);
                        cx.stop_propagation();
                        return;
                    }
                    "left" if plain => {
                        this.move_active_cell(-1, 0, cx);
                        cx.stop_propagation();
                        return;
                    }
                    "right" if plain => {
                        this.move_active_cell(1, 0, cx);
                        cx.stop_propagation();
                        return;
                    }
                    "enter" if only_shift => {
                        this.move_active_cell(0, -1, cx);
                        cx.stop_propagation();
                        return;
                    }
                    "enter" if plain => {
                        this.move_active_cell(0, 1, cx);
                        cx.stop_propagation();
                        return;
                    }
                    "tab" if only_shift => {
                        this.move_active_cell(-1, 0, cx);
                        cx.stop_propagation();
                        return;
                    }
                    "tab" if plain => {
                        this.move_active_cell(1, 0, cx);
                        cx.stop_propagation();
                        return;
                    }
                    _ => {}
                }
                // Type-to-replace: a printable character starts an edit that
                // overwrites the cell (shift for uppercase is allowed).
                if !modifiers.control
                    && !modifiers.platform
                    && !modifiers.alt
                    && !modifiers.function
                {
                    if let Some(text) = keystroke.key_char.as_ref() {
                        if text.chars().count() == 1
                            && text.chars().next().is_some_and(|c| !c.is_control())
                        {
                            this.type_to_replace_active_cell(text, window, cx);
                            cx.stop_propagation();
                        }
                    }
                }
            }))
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
            .on_action(cx.listener(|this, _: &SetEmptyValue, _window, cx| {
                this.set_selected_cell_value(CellValue::Text(String::new()), cx);
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
            .on_action(cx.listener(|this, _: &ToggleHeatmap, _window, cx| {
                this.toggle_heatmap(cx);
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
            .on_action(cx.listener(|this, _: &SelectRowStart, _window, cx| {
                this.select_row_start(cx);
            }))
            .on_action(cx.listener(|this, _: &SelectRowEnd, _window, cx| {
                this.select_row_end(cx);
            }))
            .on_action(cx.listener(|this, _: &SelectFirstCell, _window, cx| {
                this.select_first_cell(cx);
            }))
            .on_action(cx.listener(|this, _: &SelectLastCell, _window, cx| {
                this.select_last_cell(cx);
            }))
            .on_action(cx.listener(|this, _: &SelectPageUp, _window, cx| {
                this.move_page(-(PAGE_ROW_JUMP as isize), cx);
            }))
            .on_action(cx.listener(|this, _: &SelectPageDown, _window, cx| {
                this.move_page(PAGE_ROW_JUMP as isize, cx);
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
    fn format_query_error_preserves_the_driver_message_under_the_generic_context() {
        // Simulates a provider call like `.context("Query execution failed")?`
        // wrapping a driver-returned error (e.g. sqlx's or scylla's own error
        // type), the way `crates/db_client/src/{mysql,postgres,sqlite,
        // cassandra_provider}.rs` do. `.to_string()` on the resulting
        // `anyhow::Error` would only print "Query execution failed" and lose
        // the actual database-reported detail below it.
        let driver_error = anyhow::anyhow!(
            "error returned from database: 1146 (42S02): Table 'app.does_not_exist' doesn't exist"
        );
        let wrapped = driver_error.context("Query execution failed");

        let shown = format_query_error(&wrapped);
        assert!(
            shown.contains("Table 'app.does_not_exist' doesn't exist"),
            "expected the underlying driver message in the formatted error, got: {shown:?}"
        );
        assert!(
            shown.contains("Query execution failed"),
            "the generic context should still be present as the top-level message, got: {shown:?}"
        );
    }

    #[test]
    fn query_timing_segments_is_none_without_any_measured_time() {
        let timing = QueryTiming {
            pool_wait_ms: 0,
            execute_ms: 0,
            streaming_ms: None,
        };
        assert!(query_timing_segments(&timing).is_none());
    }

    #[test]
    fn query_timing_segments_omits_streaming_when_not_measured() {
        let timing = QueryTiming {
            pool_wait_ms: 2,
            execute_ms: 8,
            streaming_ms: None,
        };
        let segments = query_timing_segments(&timing).expect("non-zero total");
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].label, "Waiting for connection");
        assert_eq!(segments[0].ms, 2);
        assert_eq!(segments[1].label, "Executing");
        assert_eq!(segments[1].ms, 8);
    }

    #[test]
    fn query_timing_segments_includes_streaming_when_measured() {
        let timing = QueryTiming {
            pool_wait_ms: 2,
            execute_ms: 8,
            streaming_ms: Some(10),
        };
        let segments = query_timing_segments(&timing).expect("non-zero total");
        assert_eq!(segments.len(), 3);
        assert_eq!(segments[2].label, "Streaming rows");
        assert_eq!(segments[2].ms, 10);
    }

    #[test]
    fn query_timing_segments_fractions_sum_to_the_whole_bar() {
        let timing = QueryTiming {
            pool_wait_ms: 3,
            execute_ms: 5,
            streaming_ms: Some(12),
        };
        let segments = query_timing_segments(&timing).expect("non-zero total");
        let total_fraction: f32 = segments.iter().map(|segment| segment.fraction).sum();
        assert!(
            (total_fraction - 1.0).abs() < 0.0001,
            "expected the segment fractions to sum to 1.0, got {total_fraction}"
        );
    }

    #[test]
    fn format_query_timing_tooltip_names_every_present_phase() {
        let timing = QueryTiming {
            pool_wait_ms: 2,
            execute_ms: 8,
            streaming_ms: Some(15),
        };
        let segments = query_timing_segments(&timing).expect("non-zero total");
        let tooltip = format_query_timing_tooltip(&segments);
        assert_eq!(
            tooltip,
            "Waiting for connection: 2ms · Executing: 8ms · Streaming rows: 15ms"
        );
    }

    #[test]
    fn format_query_timing_tooltip_omits_streaming_when_absent() {
        let timing = QueryTiming {
            pool_wait_ms: 1,
            execute_ms: 4,
            streaming_ms: None,
        };
        let segments = query_timing_segments(&timing).expect("non-zero total");
        let tooltip = format_query_timing_tooltip(&segments);
        assert_eq!(tooltip, "Waiting for connection: 1ms · Executing: 4ms");
        assert!(!tooltip.contains("Streaming"));
    }

    #[gpui::test]
    fn clicking_the_copy_button_on_a_query_error_puts_the_full_message_on_the_clipboard(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });

        let error_message =
            "Query execution failed\n\nCaused by:\n    Table 'app.does_not_exist' doesn't exist";
        let window = cx.add_window(|_window, cx| ResultView::new("error-copy", cx));
        let cx = &mut gpui::VisualTestContext::from_window(window.into(), cx);
        window
            .update(cx, |view, _window, cx| {
                view.set_error(error_message.to_string(), cx)
            })
            .unwrap();

        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        cx.run_until_parked();

        let target = cx
            .debug_bounds("query-error-copy")
            .map(|bounds| bounds.center())
            .expect("the error's copy button should render");
        cx.simulate_click(target, gpui::Modifiers::none());
        cx.run_until_parked();

        let clipboard = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .expect("the error message should be on the clipboard after the click");
        assert_eq!(
            clipboard, error_message,
            "the copy button must put the full error text on the clipboard, not a truncated preview"
        );
    }

    #[test]
    fn parse_query_error_pulls_the_database_sentence_out_of_the_chain() {
        let parts = parse_query_error(MYSQL_MISSING_TABLE_ERROR);
        assert_eq!(parts.headline, MYSQL_MISSING_TABLE_SENTENCE);
        assert_eq!(parts.vendor_code.as_deref(), Some("1146"));
        assert_eq!(parts.sqlstate.as_deref(), Some("42S02"));
        assert_eq!(parts.detail.as_deref(), Some(MYSQL_MISSING_TABLE_ERROR));
    }

    #[test]
    fn parse_query_error_keeps_a_message_that_carries_no_driver_codes() {
        let parts = parse_query_error("relation \"public.orders\" does not exist");
        assert_eq!(parts.headline, "relation \"public.orders\" does not exist");
        assert_eq!(parts.vendor_code, None);
        assert_eq!(parts.sqlstate, None);
        assert_eq!(
            parts.detail, None,
            "a single-line message says nothing more as detail than it does as the headline"
        );
    }

    #[test]
    fn parse_query_error_ignores_a_parenthesised_word_that_is_not_a_sqlstate() {
        let parts = parse_query_error("Incorrect value 12345 (SCALE) for column 'ratio'");
        assert_eq!(parts.vendor_code, None);
        assert_eq!(parts.sqlstate, None);
        assert_eq!(
            parts.headline,
            "Incorrect value 12345 (SCALE) for column 'ratio'"
        );
    }

    #[gpui::test]
    fn query_error_message_area_paints_real_area(cx: &mut gpui::TestAppContext) {
        let (_window, _view, mut cx) = error_window(cx, MYSQL_MISSING_TABLE_ERROR);

        let message = cx
            .debug_bounds("query-error-message")
            .expect("the failure message area should render");
        assert!(
            message.size.height > px(14.),
            "the message area collapsed to {}px instead of painting a real line of text",
            f32::from(message.size.height)
        );
        let panel = cx
            .debug_bounds("query-error")
            .expect("the failure state root should render");
        assert!(
            message.size.width > panel.size.width - px(64.),
            "the message area should span the panel: {}px inside a {}px panel",
            f32::from(message.size.width),
            f32::from(panel.size.width)
        );

        let detail = cx
            .debug_bounds("query-error-detail")
            .expect("the driver detail area should render for a chained error");
        assert!(
            detail.size.height > px(14.),
            "the driver detail area collapsed to {}px",
            f32::from(detail.size.height)
        );
        assert!(
            detail.origin.y > message.origin.y + message.size.height,
            "the driver detail belongs below the message, not on top of it"
        );
    }

    #[gpui::test]
    fn query_error_header_shows_the_vendor_code_and_sqlstate(cx: &mut gpui::TestAppContext) {
        let (_window, _view, mut cx) = error_window(cx, MYSQL_MISSING_TABLE_ERROR);

        let code = cx
            .debug_bounds("query-error-code:1146")
            .expect("the header should carry the driver's own error code");
        assert!(code.size.width > px(0.) && code.size.height > px(0.));

        let sqlstate = cx
            .debug_bounds("query-error-sqlstate:42S02")
            .expect("the header should carry the SQLSTATE");
        assert!(sqlstate.size.width > px(0.) && sqlstate.size.height > px(0.));
    }

    #[gpui::test]
    fn query_error_message_drops_the_driver_framing_and_keeps_it_as_detail(
        cx: &mut gpui::TestAppContext,
    ) {
        let (_window, view, mut cx) = error_window(cx, MYSQL_MISSING_TABLE_ERROR);

        let message = error_message_editor(&view, &mut cx);
        assert_eq!(
            message.update(&mut cx, |editor, cx| editor.text(cx)),
            MYSQL_MISSING_TABLE_SENTENCE,
            "the message area must show what the database said, not the layers wrapped around it"
        );

        let detail = error_detail_editor(&view, &mut cx)
            .expect("a chained error keeps its verbatim chain as detail");
        assert_eq!(
            detail.update(&mut cx, |editor, cx| editor.text(cx)),
            MYSQL_MISSING_TABLE_ERROR,
            "the detail area must be the unmodified driver output, matching what Copy yields"
        );
    }

    #[gpui::test]
    fn dragging_across_the_query_error_message_selects_only_what_was_dragged_over(
        cx: &mut gpui::TestAppContext,
    ) {
        let (_window, view, mut cx) = error_window(cx, MYSQL_MISSING_TABLE_ERROR);
        let message = error_message_editor(&view, &mut cx);
        let bounds = cx
            .debug_bounds("query-error-message")
            .expect("the failure message area should render");

        drag_across_error_message(&mut cx, bounds, px(30.), px(80.));
        let short = selected_error_text(&message, &mut cx);
        drag_across_error_message(&mut cx, bounds, px(30.), px(200.));
        let long = selected_error_text(&message, &mut cx);

        assert!(
            !short.is_empty(),
            "dragging across part of the message selected nothing"
        );
        assert!(
            MYSQL_MISSING_TABLE_SENTENCE.contains(short.as_str()),
            "the selection {short:?} is not part of the rendered message"
        );
        assert_ne!(
            long, MYSQL_MISSING_TABLE_SENTENCE,
            "a partial drag must not select the whole message"
        );
        assert!(
            long.starts_with(short.as_str()),
            "both drags began at the same point, so the wider one must extend the shorter: \
             {short:?} then {long:?}"
        );
        assert!(
            long.len() > short.len(),
            "the selection did not follow the mouse: {short:?} and {long:?} cover the same text"
        );
    }

    #[gpui::test]
    fn a_very_long_error_wraps_instead_of_widening_the_panel(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) = error_window(cx, "Table 'x' doesn't exist");
        let one_line = cx
            .debug_bounds("query-error-message")
            .expect("the failure message area should render")
            .size
            .height;

        let columns = ["column_name_that_does_not_exist"; 70].join(", ");
        let long = format!("Unknown columns {columns} in 'field list'");
        assert!(long.len() >= 2000, "the fixture should be a long one line");
        view.update(&mut cx, |view, cx| view.set_error(long, cx));
        draw_result_view(window, &mut cx);
        draw_result_view(window, &mut cx);

        let wrapped = cx
            .debug_bounds("query-error-message")
            .expect("the failure message area should still render");
        let panel = cx
            .debug_bounds("query-error")
            .expect("the failure state root should render");
        assert!(
            wrapped.size.width <= panel.size.width,
            "a 2000-character message widened the message area ({}px) past the panel ({}px)",
            f32::from(wrapped.size.width),
            f32::from(panel.size.width)
        );
        assert!(
            f32::from(panel.size.width) <= ERROR_PANEL_WIDTH,
            "the failure state pushed the panel past its {ERROR_PANEL_WIDTH}px frame: {}px",
            f32::from(panel.size.width)
        );
        assert!(
            wrapped.size.height > one_line * 3.,
            "a 2000-character message stayed {}px tall next to a one-line {}px: it was clipped \
             instead of wrapped",
            f32::from(wrapped.size.height),
            f32::from(one_line)
        );
    }

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
            timing: None,
            raw_documents: None,
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

    #[test]
    fn thumb_range_does_not_panic_when_viewport_is_smaller_than_the_minimum_thumb_size() {
        // Reconstructs a real crash: `f32::clamp` panics if `min > max`, which
        // happened when `viewport_len` (5.0) was smaller than
        // `SCROLLBAR_THUMB_MIN` (36.0). With no room to enforce a minimum
        // thumb size, the thumb must just fill the whole tiny viewport.
        let (start, end) = thumb_range(0.0, 5.0, 1000.0, 0.0);
        assert!(start.is_finite() && end.is_finite());
        assert!(start >= 0.0);
        assert!(end - start <= 5.0 + 1e-3);
    }

    #[test]
    fn drag_scroll_offset_does_not_panic_when_viewport_is_smaller_than_the_minimum_thumb_size() {
        let result = drag_scroll_offset(0.0, 0.0, 1.0, 5.0, 1000.0);
        assert!(result.is_some_and(|offset| offset.is_finite()));
    }

    #[test]
    fn thumb_range_at_the_minimum_thumb_size_boundary_is_unchanged() {
        // viewport_len == SCROLLBAR_THUMB_MIN exactly: the clamp's lower bound
        // still equals SCROLLBAR_THUMB_MIN, so behavior must be identical to
        // before the fix -- the thumb fills the whole (tiny) viewport.
        let (start, end) = thumb_range(0.0, SCROLLBAR_THUMB_MIN, 1000.0, 0.0);
        assert!((end - start - SCROLLBAR_THUMB_MIN).abs() < 1e-3);
        assert!((start - 0.0).abs() < 1e-3);
    }

    #[test]
    fn thumb_range_above_the_minimum_thumb_size_is_unaffected_by_the_fix() {
        // viewport_len well above SCROLLBAR_THUMB_MIN: the fix is a no-op here,
        // so this must produce the same thumb_len as before the change.
        let (start, end) = thumb_range(0.0, 200.0, 1000.0, 0.0);
        let expected_thumb_len = (200.0f32 * 200.0 / 1000.0).max(SCROLLBAR_THUMB_MIN);
        assert!((end - start - expected_thumb_len).abs() < 1e-2);
        assert!((start - 0.0).abs() < 1e-3);
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
            timing: None,
            raw_documents: None,
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
            timing: None,
            raw_documents: None,
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
            timing: None,
            raw_documents: None,
        };
        let sql = ResultView::export_sql_update(&result, "users");
        assert!(sql.contains("UPDATE users SET name = 'alice', val = NULL WHERE id = 1;"));
        assert!(sql.contains("UPDATE users SET name = 'bob', val = 42 WHERE id = 2;"));
    }

    #[test]
    fn export_sql_multi_insert_uses_one_insert_with_all_value_tuples() {
        let result = QueryResult {
            columns: vec!["id".to_string(), "name".to_string()],
            rows: vec![
                vec![Some("1".to_string()), Some("Alice".to_string())],
                vec![Some("2".to_string()), None],
            ],
            rows_affected: 0,
            execution_time_ms: 0,
            timing: None,
            raw_documents: None,
        };
        let sql = ResultView::export_sql_multi_insert(&result, "users", '`');
        assert_eq!(
            sql,
            "INSERT INTO `users` (`id`, `name`) VALUES\n  (1, 'Alice'),\n  (2, NULL);\n"
        );
    }

    #[test]
    fn export_xlsx_produces_a_valid_workbook_with_header_and_cell_values() {
        use std::io::Read;

        let result = QueryResult {
            columns: vec!["id".to_string(), "name".to_string()],
            rows: vec![
                vec![Some("1".to_string()), Some("Alice".to_string())],
                vec![Some("2".to_string()), None],
            ],
            rows_affected: 0,
            execution_time_ms: 0,
            timing: None,
            raw_documents: None,
        };
        let bytes = ResultView::export_xlsx(&result).expect("xlsx export should succeed");

        let mut archive =
            zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("output must be a valid zip");
        let mut sheet_xml = String::new();
        archive
            .by_name("xl/worksheets/sheet1.xml")
            .expect("the worksheet part must exist in the workbook")
            .read_to_string(&mut sheet_xml)
            .expect("the worksheet part must be valid UTF-8 XML");

        assert!(
            sheet_xml.contains("<is><t>id</t></is>"),
            "header row must include the id column: {sheet_xml}"
        );
        assert!(
            sheet_xml.contains("<is><t>name</t></is>"),
            "header row must include the name column: {sheet_xml}"
        );
        assert!(
            sheet_xml.contains("<v>1</v>"),
            "a numeric-looking cell must be written as a numeric xlsx cell: {sheet_xml}"
        );
        assert!(
            sheet_xml.contains("<is><t>Alice</t></is>"),
            "a text cell must carry its value: {sheet_xml}"
        );
        assert!(
            sheet_xml.contains("<is><t></t></is>"),
            "a NULL cell must be written as an empty inline string, not dropped: {sheet_xml}"
        );
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
            timing: None,
            raw_documents: None,
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
            timing: None,
            raw_documents: None,
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
    fn project_lat_lon_maps_corners_and_center() {
        // Equirectangular projection: (-180, 90) is the top-left corner, (180,
        // -90) is the bottom-right corner, and the origin is the center.
        assert_eq!(project_lat_lon(90.0, -180.0), Some((0.0, 0.0)));
        assert_eq!(project_lat_lon(-90.0, 180.0), Some((1.0, 1.0)));
        assert_eq!(project_lat_lon(0.0, 0.0), Some((0.5, 0.5)));
    }

    #[test]
    fn project_lat_lon_rejects_out_of_range_and_non_finite() {
        assert_eq!(project_lat_lon(91.0, 0.0), None);
        assert_eq!(project_lat_lon(-91.0, 0.0), None);
        assert_eq!(project_lat_lon(0.0, 181.0), None);
        assert_eq!(project_lat_lon(0.0, -181.0), None);
        assert_eq!(project_lat_lon(f64::NAN, 0.0), None);
        assert_eq!(project_lat_lon(0.0, f64::INFINITY), None);
    }

    #[test]
    fn first_numeric_column_finds_numeric() {
        let result = export_fixture();
        // Column 0 (id) is numeric; column 1 (name) is not.
        assert_eq!(ResultView::first_numeric_column(&result), Some(0));
    }

    #[test]
    fn heatmap_ratio_maps_value_position_in_range() {
        assert_eq!(heatmap_ratio(0.0, 0.0, 10.0), 0.0);
        assert_eq!(heatmap_ratio(10.0, 0.0, 10.0), 1.0);
        assert_eq!(heatmap_ratio(5.0, 0.0, 10.0), 0.5);
        // A column with no spread (every value equal) must not divide by zero
        // and must render every cell with the same neutral tint.
        assert_eq!(heatmap_ratio(5.0, 5.0, 5.0), 0.5);
        // Values outside [min, max] (shouldn't normally happen, since min/max are
        // derived from the same data) still clamp instead of over/undershooting.
        assert_eq!(heatmap_ratio(-5.0, 0.0, 10.0), 0.0);
        assert_eq!(heatmap_ratio(15.0, 0.0, 10.0), 1.0);
    }

    #[gpui::test]
    fn toggle_chart_opens_and_picks_numeric_column(cx: &mut gpui::TestAppContext) {
        // Drives the real ToggleChart action (the "Show Chart" view-menu entry
        // dispatches this same action) instead of calling toggle_chart directly,
        // so this also proves the .on_action(ToggleChart) wiring in render().
        let (window, view, mut cx) = plain_result_window(cx, export_fixture());
        view.update(&mut cx, |view, _cx| assert!(!view.chart_open));
        let first_cell = debug_center(&mut cx, "CELL-0-0");
        cx.simulate_click(first_cell, gpui::Modifiers::none());
        draw_result_view(window, &mut cx);

        cx.dispatch_action(ToggleChart);
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, _cx| {
            assert!(
                view.chart_open,
                "a real ToggleChart action dispatch must open the chart"
            );
            assert_eq!(view.chart_value_column, Some(0));
        });

        cx.dispatch_action(ToggleChart);
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, _cx| assert!(!view.chart_open));
    }

    #[gpui::test]
    fn toggle_heatmap_action_tints_numeric_cells_by_value(cx: &mut gpui::TestAppContext) {
        // Drives the real ToggleHeatmap action (the "Heatmap" view-menu entry
        // dispatches this same action) instead of flipping the field directly,
        // proving the .on_action(ToggleHeatmap) wiring in render().
        let (window, view, mut cx) = plain_result_window(cx, export_fixture());
        let (base, tint) = view.update(&mut cx, |view, cx| {
            let base = cx.theme().colors().editor_background;
            let tint = cx.theme().colors().text_accent;
            assert!(!view.heatmap_enabled);
            // id column (0) ranges 1..=2 (export_fixture's two rows) once loaded.
            assert_eq!(
                view.heatmap_ranges.get(0).copied().flatten(),
                Some((1.0, 2.0))
            );
            // With heatmap mode off, the renderer's own decision function must
            // never tint a cell, regardless of the column's range.
            assert_eq!(view.heatmap_cell_bg(0, Some("1"), base, tint), None);
            (base, tint)
        });
        let first_cell = debug_center(&mut cx, "CELL-0-0");
        cx.simulate_click(first_cell, gpui::Modifiers::none());
        draw_result_view(window, &mut cx);

        cx.dispatch_action(ToggleHeatmap);
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, _cx| {
            assert!(
                view.heatmap_enabled,
                "a real ToggleHeatmap action dispatch must turn heatmap mode on"
            );
            let min_bg = view.heatmap_cell_bg(0, Some("1"), base, tint);
            let max_bg = view.heatmap_cell_bg(0, Some("2"), base, tint);
            assert_ne!(
                min_bg, max_bg,
                "the low and high end of a column's range must get visibly different tints"
            );
            assert!(min_bg.is_some() && max_bg.is_some());
            // A non-numeric column (name, index 1) must never be tinted.
            assert_eq!(view.heatmap_cell_bg(1, Some("Alice"), base, tint), None);
            // A NULL cell in a numeric column must never be tinted either.
            assert_eq!(view.heatmap_cell_bg(0, None, base, tint), None);
        });

        cx.dispatch_action(ToggleHeatmap);
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, _cx| {
            assert!(!view.heatmap_enabled);
            assert_eq!(view.heatmap_cell_bg(0, Some("1"), base, tint), None);
        });
    }

    #[gpui::test]
    fn a_statement_without_a_result_set_draws_no_grid(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let window = cx.add_window(|_window, cx| {
            let mut view = ResultView::new("statement", cx);
            // The statement matters as much as the result: without it a result
            // with no columns is taken for a query that found nothing.
            view.base_sql = Some("CREATE TABLE t (id int)".to_string());
            view.set_result(statement_result(0), cx);
            view
        });
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        draw_result_view(window, &mut visual);

        let outcome = visual
            .debug_bounds("STATEMENT_OUTCOME")
            .expect("a statement that returns no rows has to say what it did");
        assert!(
            outcome.size.width > px(0.) && outcome.size.height > px(0.),
            "the panel painted {:?}, which is no area at all",
            outcome.size
        );
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
            timing: None,
            raw_documents: None,
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
            timing: None,
            raw_documents: None,
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
            timing: None,
            raw_documents: None,
        }
    }

    // Every document shares the same top-level fields, so no cell is `None`
    // once projected into columns.
    fn mongo_homogeneous_result() -> QueryResult {
        QueryResult {
            columns: vec!["_id".to_string(), "name".to_string()],
            rows: vec![
                vec![Some("1".to_string()), Some("Ada".to_string())],
                vec![Some("2".to_string()), Some("Grace".to_string())],
            ],
            rows_affected: 0,
            execution_time_ms: 0,
            timing: None,
            raw_documents: Some(vec![
                "{ \"_id\": 1, \"name\": \"Ada\" }".to_string(),
                "{ \"_id\": 2, \"name\": \"Grace\" }".to_string(),
            ]),
        }
    }

    // The second document lacks "extra", so its projected cell is `None`.
    fn mongo_ragged_result() -> QueryResult {
        QueryResult {
            columns: vec!["_id".to_string(), "extra".to_string()],
            rows: vec![
                vec![Some("1".to_string()), Some("yes".to_string())],
                vec![Some("2".to_string()), None],
            ],
            rows_affected: 0,
            execution_time_ms: 0,
            timing: None,
            raw_documents: Some(vec![
                "{ \"_id\": 1, \"extra\": \"yes\" }".to_string(),
                "{ \"_id\": 2 }".to_string(),
            ]),
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
            timing: None,
            raw_documents: None,
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
        table_backed_result_window_with(cx, sample_table_result())
    }

    fn table_backed_result_window_with(
        cx: &mut gpui::TestAppContext,
        result: QueryResult,
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
                view.set_result(result, cx);
                view
            }
        });
        let view = window.root(cx).unwrap();
        let mut cx = gpui::VisualTestContext::from_window(window.into(), cx);
        draw_result_view(window, &mut cx);
        (window, view, cx)
    }

    // Same shape as `sample_table_result`, but row 0's "name" cell is SQL
    // NULL instead of a loaded string, for testing that clearing/leaving a
    // cell empty never coerces a NULL original into an empty string.
    fn table_result_with_null_cell() -> QueryResult {
        QueryResult {
            columns: vec!["id".to_string(), "name".to_string()],
            rows: vec![
                vec![Some("1".to_string()), None],
                vec![Some("2".to_string()), Some("Bob".to_string())],
                vec![Some("3".to_string()), Some("Claire".to_string())],
            ],
            rows_affected: 0,
            execution_time_ms: 0,
            timing: None,
            raw_documents: None,
        }
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

    // The real shape a MySQL failure has by the time it reaches the panel:
    // anyhow's generic context over sqlx's wrapper over the server's own
    // sentence, repeated once per link of the chain.
    const MYSQL_MISSING_TABLE_ERROR: &str = concat!(
        "Query execution failed\n\nCaused by:\n",
        "    0: error returned from database: 1146 (42S02): ",
        "Table 'instruments.icon_objects_1' doesn't exist\n",
        "    1: 1146 (42S02): Table 'instruments.icon_objects_1' doesn't exist"
    );
    const MYSQL_MISSING_TABLE_SENTENCE: &str = "Table 'instruments.icon_objects_1' doesn't exist";

    // A window root whose size is `auto` is stretched to fill the window, so
    // hosting the view as the root gives the panel a known 700x500 and makes
    // the geometry assertions in the failure-state tests mean something.
    const ERROR_PANEL_WIDTH: f32 = 700.;

    fn error_window(
        cx: &mut gpui::TestAppContext,
        error: &str,
    ) -> (
        gpui::WindowHandle<ResultView>,
        Entity<ResultView>,
        gpui::VisualTestContext,
    ) {
        init_result_view_test(cx);
        let error = error.to_string();
        let window = cx.open_window(
            gpui::size(px(ERROR_PANEL_WIDTH), px(500.)),
            move |_window, cx| {
                let mut view = ResultView::new("query", cx);
                view.set_error(error, cx);
                view
            },
        );
        let view = window
            .root(cx)
            .expect("test window should contain a result view");
        let mut cx = gpui::VisualTestContext::from_window(window.into(), cx);
        draw_result_view(window, &mut cx);
        (window, view, cx)
    }

    fn error_message_editor(
        view: &Entity<ResultView>,
        cx: &mut gpui::VisualTestContext,
    ) -> Entity<Editor> {
        view.update(cx, |view, _cx| {
            view.error_view
                .as_ref()
                .expect("the failure state should be prepared during render")
                .message
                .clone()
        })
    }

    fn error_detail_editor(
        view: &Entity<ResultView>,
        cx: &mut gpui::VisualTestContext,
    ) -> Option<Entity<Editor>> {
        view.update(cx, |view, _cx| {
            view.error_view
                .as_ref()
                .and_then(|error_view| error_view.detail.clone())
        })
    }

    fn selected_error_text(editor: &Entity<Editor>, cx: &mut gpui::VisualTestContext) -> String {
        use editor::ToOffset as _;
        editor.update(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let selection = editor.selections.newest_anchor();
            let start = selection.start.to_offset(&snapshot).0;
            let end = selection.end.to_offset(&snapshot).0;
            let text = editor.text(cx);
            text.get(start.min(end)..start.max(end))
                .unwrap_or_default()
                .to_string()
        })
    }

    // A real press-drag-release along the middle of `bounds`, with both x
    // offsets measured from its left edge.
    fn drag_across_error_message(
        cx: &mut gpui::VisualTestContext,
        bounds: gpui::Bounds<Pixels>,
        from_x: Pixels,
        to_x: Pixels,
    ) {
        let y = bounds.origin.y + bounds.size.height * 0.5;
        cx.simulate_mouse_down(
            gpui::point(bounds.origin.x + from_x, y),
            MouseButton::Left,
            gpui::Modifiers::none(),
        );
        cx.simulate_mouse_move(
            gpui::point(bounds.origin.x + to_x, y),
            Some(MouseButton::Left),
            gpui::Modifiers::none(),
        );
        cx.simulate_mouse_up(
            gpui::point(bounds.origin.x + to_x, y),
            MouseButton::Left,
            gpui::Modifiers::none(),
        );
        cx.run_until_parked();
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

    fn statement_result(rows_affected: u64) -> QueryResult {
        QueryResult {
            columns: vec![],
            rows: vec![],
            rows_affected,
            execution_time_ms: 860,
            timing: None,
            raw_documents: None,
        }
    }

    #[test]
    fn a_statement_without_a_result_set_is_told_apart_from_a_query_that_found_nothing() {
        use super::returns_no_result_set;
        assert!(
            returns_no_result_set(&statement_result(0), Some("CREATE TABLE t (id int)")),
            "a CREATE names no columns and was never going to return any"
        );
        let found_nothing = QueryResult {
            columns: vec!["id".to_string()],
            rows: vec![],
            rows_affected: 0,
            execution_time_ms: 1,
            timing: None,
            raw_documents: None,
        };
        assert!(
            !returns_no_result_set(&found_nothing, Some("SELECT id FROM t")),
            "a query that matched no rows belongs in a grid with a header and no rows"
        );
        // The regression this guards: a provider that learns its columns from the
        // first row has none to report for a query that matched nothing, and that
        // query must still be shown as a query.
        assert!(
            !returns_no_result_set(&statement_result(0), Some("SELECT * FROM t")),
            "a SELECT is a SELECT even when it comes back with neither rows nor columns"
        );
        for query in [
            "  select * from t",
            "WITH x AS (SELECT 1) SELECT * FROM x",
            "SHOW TABLES",
            "EXPLAIN SELECT 1",
        ] {
            assert!(
                !returns_no_result_set(&statement_result(0), Some(query)),
                "{query} asks for rows"
            );
        }
        assert!(
            !returns_no_result_set(&statement_result(0), None),
            "with no statement to judge, a grid is the safe answer"
        );
    }

    #[test]
    fn a_statement_is_reported_by_what_it_did_and_never_as_zero_rows() {
        use super::statement_outcome;
        let (headline, detail) = statement_outcome(&statement_result(0));
        assert_eq!(headline, "Statement completed");
        assert!(
            !headline.contains("row") && !headline.contains("0"),
            "a CREATE reported as rows reads as a query that found none: {headline:?}"
        );
        assert!(
            detail.contains("860 ms"),
            "the time it took is worth keeping"
        );

        assert_eq!(statement_outcome(&statement_result(1)).0, "1 row affected");
        assert_eq!(
            statement_outcome(&statement_result(20_000)).0,
            "20000 rows affected"
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

        // A plain tabular EXPLAIN (MySQL's default: many columns) is not a tree
        // plan. Flattening its columns would render a meaningless
        // one-value-per-line list, so it must fall back to the normal grid.
        let mysql_explain_columns: Vec<String> = [
            "id",
            "select_type",
            "table",
            "type",
            "possible_keys",
            "key",
            "key_len",
            "ref",
            "rows",
            "filtered",
            "Extra",
        ]
        .iter()
        .map(|column| column.to_string())
        .collect();
        assert_eq!(
            detect_special_result(Some("EXPLAIN SELECT * FROM t"), &mysql_explain_columns),
            SpecialResult::None
        );

        // A tree-shaped EXPLAIN still comes back as a single plan column.
        assert_eq!(
            detect_special_result(
                Some("EXPLAIN FORMAT=TREE SELECT * FROM t"),
                &["EXPLAIN".to_string()],
            ),
            SpecialResult::ExplainPlan
        );
    }

    #[test]
    fn default_mongo_view_picks_table_for_a_homogeneous_result() {
        use super::{MongoResultView, default_mongo_view_for_result};
        assert_eq!(
            default_mongo_view_for_result(&mongo_homogeneous_result()),
            MongoResultView::Table
        );
    }

    #[test]
    fn default_mongo_view_picks_documents_for_a_ragged_result() {
        use super::{MongoResultView, default_mongo_view_for_result};
        assert_eq!(
            default_mongo_view_for_result(&mongo_ragged_result()),
            MongoResultView::Documents
        );
    }

    #[test]
    fn default_mongo_view_picks_table_for_an_empty_result() {
        use super::{MongoResultView, default_mongo_view_for_result};
        let result = QueryResult {
            columns: vec!["_id".to_string()],
            rows: vec![],
            rows_affected: 0,
            execution_time_ms: 0,
            timing: None,
            raw_documents: Some(vec![]),
        };
        assert_eq!(
            default_mongo_view_for_result(&result),
            MongoResultView::Table
        );
    }

    #[test]
    fn mongo_documents_display_text_joins_documents_with_a_blank_line_in_row_order() {
        use super::mongo_documents_display_text;
        let result = mongo_homogeneous_result();
        let text = mongo_documents_display_text(&result);
        assert_eq!(
            text,
            "{ \"_id\": 1, \"name\": \"Ada\" }\n\n{ \"_id\": 2, \"name\": \"Grace\" }"
        );
    }

    #[test]
    fn mongo_documents_display_text_is_empty_for_a_non_document_result() {
        use super::mongo_documents_display_text;
        assert_eq!(mongo_documents_display_text(&sample_table_result()), "");
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
            timing: None,
            raw_documents: None,
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
            timing: None,
            raw_documents: None,
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
            timing: None,
            raw_documents: None,
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
                timing: None,
                raw_documents: None,
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
    fn mongo_ragged_result_opens_in_documents_view_by_default(cx: &mut gpui::TestAppContext) {
        let (_window, _view, mut cx) = plain_result_window(cx, mongo_ragged_result());
        assert!(
            cx.debug_bounds("MONGO_DOCUMENTS_VIEW").is_some(),
            "a ragged Mongo result must open as documents, not a table with holes"
        );
        assert!(cx.debug_bounds("CELL-0-0").is_none());
    }

    #[gpui::test]
    fn mongo_homogeneous_result_opens_in_table_view_by_default(cx: &mut gpui::TestAppContext) {
        let (_window, _view, mut cx) = plain_result_window(cx, mongo_homogeneous_result());
        assert!(
            cx.debug_bounds("MONGO_DOCUMENTS_VIEW").is_none(),
            "a homogeneous Mongo result must open as a table by default"
        );
        assert!(cx.debug_bounds("CELL-0-0").is_some());
    }

    #[gpui::test]
    fn mongo_view_toggle_is_absent_for_non_mongo_results(cx: &mut gpui::TestAppContext) {
        let (_window, _view, mut cx) = plain_result_window(cx, sample_table_result());
        assert!(cx.debug_bounds("mongo-view-table").is_none());
        assert!(cx.debug_bounds("mongo-view-documents").is_none());
    }

    #[gpui::test]
    fn clicking_table_toggle_switches_a_ragged_mongo_result_to_the_grid(
        cx: &mut gpui::TestAppContext,
    ) {
        let (window, _view, mut cx) = plain_result_window(cx, mongo_ragged_result());
        assert!(cx.debug_bounds("MONGO_DOCUMENTS_VIEW").is_some());

        let target = cx
            .debug_bounds("mongo-view-table")
            .map(|bounds| bounds.center())
            .expect("the Table toggle should render in the Documents view header");
        cx.simulate_click(target, gpui::Modifiers::none());
        draw_result_view(window, &mut cx);

        assert!(
            cx.debug_bounds("CELL-0-0").is_some(),
            "clicking Table must switch a ragged Mongo result to the grid"
        );
        assert!(cx.debug_bounds("MONGO_DOCUMENTS_VIEW").is_none());
    }

    #[gpui::test]
    fn clicking_documents_toggle_switches_a_homogeneous_mongo_result_to_documents(
        cx: &mut gpui::TestAppContext,
    ) {
        let (window, _view, mut cx) = plain_result_window(cx, mongo_homogeneous_result());
        assert!(cx.debug_bounds("CELL-0-0").is_some());

        let target = cx
            .debug_bounds("mongo-view-documents")
            .map(|bounds| bounds.center())
            .expect("the Documents toggle should render in the grid toolbar");
        cx.simulate_click(target, gpui::Modifiers::none());
        draw_result_view(window, &mut cx);

        assert!(
            cx.debug_bounds("MONGO_DOCUMENTS_VIEW").is_some(),
            "clicking Documents must switch a homogeneous Mongo result to the documents view"
        );
    }

    /// A column whose values are longer than the width a sample of the rows
    /// suggests -- an address, say -- is cut off, and the reader has to be able
    /// to take its edge and pull. Measured on the painted header, with a real
    /// press, drag and release: the width the view holds says nothing about what
    /// the reader can actually see.
    #[gpui::test]
    fn visual_dragging_a_column_edge_makes_the_column_wider(cx: &mut gpui::TestAppContext) {
        let long = "https://s2.coinmarketcap.com/static/img/coins/64x64/17106.png";
        let result = QueryResult {
            columns: vec!["id".into(), "logo_url".into()],
            rows: (0..12)
                .map(|row| {
                    vec![
                        Some(format!("{}", 3294 + row)),
                        Some(format!("{long}?v={row}")),
                    ]
                })
                .collect(),
            rows_affected: 0,
            execution_time_ms: 0,
            timing: None,
            raw_documents: None,
        };
        let (window, view, mut cx) = framed_plain_result_window(cx, result);

        let before = cx
            .debug_bounds("COL_HEADER-1")
            .expect("the address column has a header");
        let edge = cx
            .debug_bounds("COL_RESIZE-1")
            .expect("the address column has an edge to take hold of");
        assert!(
            edge.size.width > px(1.) && edge.size.height > px(1.),
            "the edge has to be there to be grabbed, not {:?}",
            edge.size
        );
        assert!(
            (edge.right() - before.right()).abs() < px(2.),
            "the edge belongs on the seam between the columns: {:?} against {:?}",
            edge.right(),
            before.right()
        );

        let took_hold_at = edge.center();
        cx.simulate_event(gpui::MouseDownEvent {
            position: took_hold_at,
            button: gpui::MouseButton::Left,
            modifiers: gpui::Modifiers::none(),
            click_count: 1,
            first_mouse: false,
        });
        // Two steps, because a drag is not one jump, and the width has to be
        // worked out from where the drag began rather than from the last step.
        for step in [90., 180.] {
            cx.simulate_event(gpui::MouseMoveEvent {
                position: gpui::point(took_hold_at.x + px(step), took_hold_at.y),
                pressed_button: Some(gpui::MouseButton::Left),
                modifiers: gpui::Modifiers::none(),
            });
        }
        cx.simulate_event(gpui::MouseUpEvent {
            position: gpui::point(took_hold_at.x + px(180.), took_hold_at.y),
            button: gpui::MouseButton::Left,
            modifiers: gpui::Modifiers::none(),
            click_count: 1,
        });
        draw_result_view_frame(window, &mut cx);

        let after = cx
            .debug_bounds("COL_HEADER-1")
            .expect("the address column still has a header");
        assert!(
            (after.size.width - before.size.width - px(180.)).abs() < px(6.),
            "the column had to follow the drag: {:?} before, {:?} after",
            before.size.width,
            after.size.width
        );
        // And the drag is over, so the page is the reader's again.
        assert!(
            cx.debug_bounds("COL_RESIZE_OVERLAY").is_none(),
            "letting go has to take the overlay off the grid"
        );
        // Taking hold of the edge must not sort the column: the press is the
        // reader reaching for the seam, not for the header.
        view.update(&mut cx, |view, _| {
            assert!(
                view.sort_columns.is_empty(),
                "grabbing the edge sorted the column instead of resizing it"
            );
        });

        // A width set by hand is the reader's decision, and running the query
        // again must not undo it.
        let same_again = QueryResult {
            columns: vec!["id".into(), "logo_url".into()],
            rows: vec![vec![Some("1".into()), Some(long.into())]],
            rows_affected: 0,
            execution_time_ms: 0,
            timing: None,
            raw_documents: None,
        };
        view.update(&mut cx, |view, cx| view.set_result(same_again, cx));
        draw_result_view_frame(window, &mut cx);
        let kept = cx
            .debug_bounds("COL_HEADER-1")
            .expect("the address column has a header after running it again");
        assert!(
            (kept.size.width - after.size.width).abs() < px(2.),
            "the width the reader set has to survive the next run: {:?} against {:?}",
            kept.size.width,
            after.size.width
        );

        // And a double click on the edge gives the column back the width its own
        // rows suggest, which is the way out of dragging it too narrow to read.
        let edge = cx
            .debug_bounds("COL_RESIZE-1")
            .expect("the edge is still there");
        let at = edge.center();
        cx.simulate_event(gpui::MouseDownEvent {
            position: at,
            button: gpui::MouseButton::Left,
            modifiers: gpui::Modifiers::none(),
            click_count: 2,
            first_mouse: false,
        });
        cx.simulate_event(gpui::MouseUpEvent {
            position: at,
            button: gpui::MouseButton::Left,
            modifiers: gpui::Modifiers::none(),
            click_count: 2,
        });
        draw_result_view_frame(window, &mut cx);
        let fitted = cx
            .debug_bounds("COL_HEADER-1")
            .expect("the address column has a header after being fitted");
        assert!(
            fitted.size.width < after.size.width,
            "a double click on the edge has to go back to what the rows ask for: \
             {:?} against {:?}",
            fitted.size.width,
            after.size.width
        );
    }

    /// A join hands us two columns of the same name. Widening one must leave the
    /// other alone, so a hand-set width is remembered per column, not per name.
    #[gpui::test]
    fn visual_same_named_columns_are_resized_separately(cx: &mut gpui::TestAppContext) {
        let result = QueryResult {
            columns: vec!["id".into(), "id".into()],
            rows: vec![vec![Some("1".into()), Some("2".into())]],
            rows_affected: 0,
            execution_time_ms: 0,
            timing: None,
            raw_documents: None,
        };
        let (window, _view, mut cx) = framed_plain_result_window(cx, result);

        let first_before = cx
            .debug_bounds("COL_HEADER-0")
            .expect("the first id column has a header");
        let second_before = cx
            .debug_bounds("COL_HEADER-1")
            .expect("the second id column has a header");
        let edge = cx
            .debug_bounds("COL_RESIZE-1")
            .expect("the second id column has an edge");

        drag_horizontally(&mut cx, edge.center(), 120.);
        draw_result_view_frame(window, &mut cx);

        let first_after = cx
            .debug_bounds("COL_HEADER-0")
            .expect("the first id column still has a header");
        let second_after = cx
            .debug_bounds("COL_HEADER-1")
            .expect("the second id column still has a header");
        assert!(
            (second_after.size.width - second_before.size.width - px(120.)).abs() < px(6.),
            "the column that was dragged had to follow: {:?} against {:?}",
            second_before.size.width,
            second_after.size.width
        );
        assert!(
            (first_after.size.width - first_before.size.width).abs() < px(2.),
            "its namesake had to stay where it was: {:?} against {:?}",
            first_before.size.width,
            first_after.size.width
        );
    }

    /// Columns can be hidden while a button is held. The held position would
    /// then belong to a different column, so the drag has to let go instead of
    /// resizing whichever column moved into that slot.
    #[gpui::test]
    fn hiding_a_column_mid_drag_lets_go_instead_of_resizing_its_neighbour(
        cx: &mut gpui::TestAppContext,
    ) {
        let result = QueryResult {
            columns: vec!["id".into(), "name".into(), "url".into()],
            rows: vec![vec![
                Some("1".into()),
                Some("Alice".into()),
                Some("https://example.com/a".into()),
            ]],
            rows_affected: 0,
            execution_time_ms: 0,
            timing: None,
            raw_documents: None,
        };
        let (window, view, mut cx) = framed_plain_result_window(cx, result);

        let edge = cx
            .debug_bounds("COL_RESIZE-2")
            .expect("the last column has an edge");
        let took_hold_at = edge.center();
        cx.simulate_event(gpui::MouseDownEvent {
            position: took_hold_at,
            button: gpui::MouseButton::Left,
            modifiers: gpui::Modifiers::none(),
            click_count: 1,
            first_mouse: false,
        });

        view.update(&mut cx, |view, cx| view.toggle_column_visibility(1, cx));
        cx.simulate_event(gpui::MouseMoveEvent {
            position: gpui::point(took_hold_at.x + px(200.), took_hold_at.y),
            pressed_button: Some(gpui::MouseButton::Left),
            modifiers: gpui::Modifiers::none(),
        });
        draw_result_view_frame(window, &mut cx);

        view.update(&mut cx, |view, _| {
            assert!(
                view.column_resize.is_none(),
                "the drag had to let go once its column left the row"
            );
            assert!(
                view.widths_by_hand.is_empty(),
                "nothing was resized, so no width was remembered: {:?}",
                view.widths_by_hand
            );
        });
    }

    /// The overlay that follows the pointer is hit-tested, so a release that
    /// lands somewhere else in the window never reaches it. The drag still has
    /// to end, or the overlay stays up and eats every later click.
    #[gpui::test]
    fn letting_go_away_from_the_grid_still_ends_the_drag(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) = table_backed_result_window(cx);

        let edge = cx
            .debug_bounds("COL_RESIZE-1")
            .expect("the second column has an edge");
        let took_hold_at = edge.center();
        cx.simulate_event(gpui::MouseDownEvent {
            position: took_hold_at,
            button: gpui::MouseButton::Left,
            modifiers: gpui::Modifiers::none(),
            click_count: 1,
            first_mouse: false,
        });
        cx.simulate_event(gpui::MouseMoveEvent {
            position: gpui::point(took_hold_at.x + px(60.), took_hold_at.y),
            pressed_button: Some(gpui::MouseButton::Left),
            modifiers: gpui::Modifiers::none(),
        });
        draw_result_view(window, &mut cx);
        let overlay = cx
            .debug_bounds("COL_RESIZE_OVERLAY")
            .expect("the drag put the overlay up");

        // Above the overlay's own top edge: inside the window, outside the grid.
        let away = gpui::point(overlay.center().x, overlay.origin.y - px(4.));
        cx.simulate_event(gpui::MouseUpEvent {
            position: away,
            button: gpui::MouseButton::Left,
            modifiers: gpui::Modifiers::none(),
            click_count: 1,
        });
        draw_result_view(window, &mut cx);

        view.update(&mut cx, |view, _| {
            assert!(
                view.column_resize.is_none(),
                "letting go away from the grid had to end the drag"
            );
        });
        assert!(
            cx.debug_bounds("COL_RESIZE_OVERLAY").is_none(),
            "the overlay had to come down with the drag"
        );
    }

    fn drag_horizontally(
        cx: &mut gpui::VisualTestContext,
        from: gpui::Point<gpui::Pixels>,
        by: f32,
    ) {
        cx.simulate_event(gpui::MouseDownEvent {
            position: from,
            button: gpui::MouseButton::Left,
            modifiers: gpui::Modifiers::none(),
            click_count: 1,
            first_mouse: false,
        });
        for step in [by / 2., by] {
            cx.simulate_event(gpui::MouseMoveEvent {
                position: gpui::point(from.x + px(step), from.y),
                pressed_button: Some(gpui::MouseButton::Left),
                modifiers: gpui::Modifiers::none(),
            });
        }
        cx.simulate_event(gpui::MouseUpEvent {
            position: gpui::point(from.x + px(by), from.y),
            button: gpui::MouseButton::Left,
            modifiers: gpui::Modifiers::none(),
            click_count: 1,
        });
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

    // Regression test for a bug that survived direct-function-call and
    // element-tree/text-content assertions (like the two tests above) across
    // four investigation rounds, and only reproduced under a real compiled
    // build driven by genuine OS-level input: the cell editor's wrapping div
    // had no size directive, so during layout its size_full child editor
    // resolved to a zero-area box and painted nothing, even though the
    // editor's own text content was always correct. Checking real computed
    // bounds (not just model state) is what would have caught this.
    #[gpui::test]
    fn cell_editor_body_has_nonzero_bounds_while_editing(cx: &mut gpui::TestAppContext) {
        let (window, _view, mut cx) = table_backed_result_window(cx);
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

        let bounds = cx
            .debug_bounds("CELL_EDITOR_BODY")
            .expect("the cell editor body should be present and measurable while editing");
        assert!(
            bounds.size.width > px(0.) && bounds.size.height > px(0.),
            "the live cell editor must occupy real, non-zero screen area, not just exist in the \
             element tree: got {:?}",
            bounds.size
        );
    }

    // Real double-click + real click-away, matching the reported bug exactly:
    // opening a NULL cell's editor and leaving it untouched must never turn
    // the cell into an empty string.
    #[gpui::test]
    fn committing_an_empty_editor_on_a_null_cell_keeps_it_null(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) =
            table_backed_result_window_with(cx, table_result_with_null_cell());
        let null_cell = debug_center(&mut cx, "CELL-0-1");

        cx.simulate_event(gpui::MouseDownEvent {
            position: null_cell,
            button: gpui::MouseButton::Left,
            modifiers: gpui::Modifiers::none(),
            click_count: 2,
            first_mouse: false,
        });
        cx.simulate_event(gpui::MouseUpEvent {
            position: null_cell,
            button: gpui::MouseButton::Left,
            modifiers: gpui::Modifiers::none(),
            click_count: 2,
        });
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, cx| {
            assert_eq!(
                view.cell_edit
                    .as_ref()
                    .map(|edit| edit.editor.read(cx).text(cx)),
                Some(String::new()),
                "a NULL cell's editor should start empty"
            );
        });

        let other_cell = debug_center(&mut cx, "CELL-1-1");
        cx.simulate_event(gpui::MouseDownEvent {
            position: other_cell,
            button: gpui::MouseButton::Left,
            modifiers: gpui::Modifiers::none(),
            click_count: 1,
            first_mouse: false,
        });
        cx.simulate_event(gpui::MouseUpEvent {
            position: other_cell,
            button: gpui::MouseButton::Left,
            modifiers: gpui::Modifiers::none(),
            click_count: 1,
        });
        draw_result_view(window, &mut cx);

        view.update(&mut cx, |view, _cx| {
            assert!(
                !view.pending_edits.contains_key(&(0, 1)),
                "leaving a NULL cell's editor empty must not buffer an edit that turns it into \
                 an empty string"
            );
        });
    }

    // Same as above, but the user actually types something first and then
    // erases it back to nothing before clicking away — the net effect at
    // commit time is still "empty", so the original NULL must be kept.
    #[gpui::test]
    fn typing_then_clearing_a_null_cell_before_commit_keeps_it_null(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) =
            table_backed_result_window_with(cx, table_result_with_null_cell());
        let null_cell = debug_center(&mut cx, "CELL-0-1");

        cx.simulate_event(gpui::MouseDownEvent {
            position: null_cell,
            button: gpui::MouseButton::Left,
            modifiers: gpui::Modifiers::none(),
            click_count: 2,
            first_mouse: false,
        });
        cx.simulate_event(gpui::MouseUpEvent {
            position: null_cell,
            button: gpui::MouseButton::Left,
            modifiers: gpui::Modifiers::none(),
            click_count: 2,
        });
        draw_result_view(window, &mut cx);

        // Real typing, then a real delete action (mirrors how the Escape fix
        // test above dispatches `editor::actions::Cancel` directly: this test
        // harness loads no production keymap, so a raw "backspace" keystroke
        // never resolves to the Editor's delete-backward action; dispatching
        // the action itself still exercises the real deletion path).
        cx.simulate_keystrokes("x");
        cx.dispatch_action(editor::actions::Backspace);
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, cx| {
            assert_eq!(
                view.cell_edit
                    .as_ref()
                    .map(|edit| edit.editor.read(cx).text(cx)),
                Some(String::new()),
                "typing then erasing back to nothing should leave the editor empty"
            );
        });

        let other_cell = debug_center(&mut cx, "CELL-1-1");
        cx.simulate_event(gpui::MouseDownEvent {
            position: other_cell,
            button: gpui::MouseButton::Left,
            modifiers: gpui::Modifiers::none(),
            click_count: 1,
            first_mouse: false,
        });
        cx.simulate_event(gpui::MouseUpEvent {
            position: other_cell,
            button: gpui::MouseButton::Left,
            modifiers: gpui::Modifiers::none(),
            click_count: 1,
        });
        draw_result_view(window, &mut cx);

        view.update(&mut cx, |view, _cx| {
            assert!(
                !view.pending_edits.contains_key(&(0, 1)),
                "typing then clearing a NULL cell before committing must not turn it into an \
                 empty string"
            );
        });
    }

    // Regression proof that the empty-commit no-op does not swallow a real
    // edit: replacing a non-empty value with a different non-empty value must
    // still buffer normally.
    #[gpui::test]
    fn committing_a_new_nonempty_value_still_buffers_the_edit(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) =
            table_backed_result_window_with(cx, table_result_with_null_cell());
        let bob_cell = debug_center(&mut cx, "CELL-1-1");

        // A single click selects the cell (no edit yet); typing on a selected,
        // non-editing cell replaces its value (type-to-replace), giving a
        // clean, unambiguous new value to assert on.
        cx.simulate_click(bob_cell, gpui::Modifiers::none());
        draw_result_view(window, &mut cx);
        cx.simulate_input("bydan");

        let other_cell = debug_center(&mut cx, "CELL-0-0");
        cx.simulate_event(gpui::MouseDownEvent {
            position: other_cell,
            button: gpui::MouseButton::Left,
            modifiers: gpui::Modifiers::none(),
            click_count: 1,
            first_mouse: false,
        });
        cx.simulate_event(gpui::MouseUpEvent {
            position: other_cell,
            button: gpui::MouseButton::Left,
            modifiers: gpui::Modifiers::none(),
            click_count: 1,
        });
        draw_result_view(window, &mut cx);

        view.update(&mut cx, |view, _cx| {
            let edit = view
                .pending_edits
                .get(&(1, 1))
                .expect("a real, non-empty edit must still be buffered");
            assert!(matches!(&edit.new_value, CellValue::Text(t) if t == "bydan"));
        });
    }

    // The only way to deliberately store a real empty string: distinct from
    // both NULL and leaving the cell untouched.
    #[gpui::test]
    fn set_empty_value_action_stores_a_real_empty_string(cx: &mut gpui::TestAppContext) {
        // Real ctrl-alt-e keystroke through the production binding
        // (assets/keymaps/default-linux.json: "DbResultView" -> SetEmptyValue)
        // instead of calling set_selected_cell_value directly.
        cx.update(|cx| {
            cx.bind_keys([gpui::KeyBinding::new(
                "ctrl-alt-e",
                SetEmptyValue,
                Some("DbResultView"),
            )]);
        });
        let (window, view, mut cx) =
            table_backed_result_window_with(cx, table_result_with_null_cell());

        let cell = debug_center(&mut cx, "CELL-0-1");
        cx.simulate_click(cell, gpui::Modifiers::none());
        draw_result_view(window, &mut cx);

        cx.simulate_keystrokes("ctrl-alt-e");
        draw_result_view(window, &mut cx);

        view.update(&mut cx, |view, _cx| {
            let edit = view
                .pending_edits
                .get(&(0, 1))
                .expect("Set Empty Value must buffer a real change from NULL");
            assert_eq!(edit.new_value, CellValue::Text(String::new()));
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
    fn single_click_leaves_cell_value_and_data_untouched(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) = table_backed_result_window(cx);
        let cell_center = debug_center(&mut cx, "CELL-0-1");

        cx.simulate_click(cell_center, gpui::Modifiers::none());
        draw_result_view(window, &mut cx);

        view.update(&mut cx, |view, _cx| {
            assert_eq!(view.selected_cell, Some((0, 1)));
            assert!(
                view.cell_edit.is_none(),
                "a plain single click must not open the inline editor"
            );
            assert!(
                !view.value_editor_open,
                "a plain single click on a short value must not open the value editor popup"
            );
            assert_eq!(
                view.result.as_ref().unwrap().rows[0][1].as_deref(),
                Some("Alice"),
                "the underlying cell data must be unchanged by a single click"
            );
        });
    }

    #[gpui::test]
    fn double_click_keeps_cloned_row_cell_text(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) = table_backed_result_window(cx);
        view.update(&mut cx, |view, cx| {
            view.clone_row_after(0, 0, cx);
        });
        draw_result_view(window, &mut cx);
        let cell_center = debug_center(&mut cx, "ADDED_CELL-0-1");

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
                "double-click on a cloned added-row cell must keep its value, not wipe it"
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
            timing: None,
            raw_documents: None,
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
            timing: None,
            raw_documents: None,
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
            timing: None,
            raw_documents: None,
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
    fn visual_second_single_click_on_selected_cell_does_not_start_editing(
        cx: &mut gpui::TestAppContext,
    ) {
        let (window, view, mut cx) = table_backed_result_window(cx);
        let cell_center = debug_center(&mut cx, "CELL-0-1");

        cx.simulate_click(cell_center, gpui::Modifiers::none());
        draw_result_view(window, &mut cx);
        cx.simulate_click(cell_center, gpui::Modifiers::none());
        draw_result_view(window, &mut cx);

        view.update(&mut cx, |view, _cx| {
            assert_eq!(view.selected_cell, Some((0, 1)));
            assert!(
                view.cell_edit.is_none(),
                "a second single click on an already-selected cell must only select it, like Excel — only a double-click or F2 should start editing"
            );
            let text = view.loaded_cell_value(0, 1).unwrap_or_default();
            assert_eq!(
                text, "Alice",
                "the cell's value must stay visible in read mode after repeated single clicks"
            );
        });
    }

    #[gpui::test]
    fn ctrl_click_on_a_cell_selects_it_like_a_plain_click(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) = table_backed_result_window(cx);
        let first_cell = debug_center(&mut cx, "CELL-0-0");
        let other_cell = debug_center(&mut cx, "CELL-1-1");
        cx.simulate_click(first_cell, gpui::Modifiers::none());
        draw_result_view(window, &mut cx);

        // The cell selection model only tracks one contiguous range, not a
        // discontiguous set, so Ctrl-click on a cell is not additive: it must
        // behave exactly like a plain click, replacing the selection.
        cx.simulate_click(other_cell, gpui::Modifiers::control());
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, _| {
            assert_eq!(view.selected_cell, Some((1, 1)));
            assert_eq!(
                view.selected_cell_range, None,
                "ctrl-click must not create or extend a multi-cell range"
            );
        });
    }

    #[gpui::test]
    fn keyboard_arrows_move_active_cell_with_clamp(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) = table_backed_result_window(cx);
        let first_cell = debug_center(&mut cx, "CELL-0-0");
        cx.simulate_click(first_cell, gpui::Modifiers::none());
        draw_result_view(window, &mut cx);

        cx.simulate_keystrokes("down");
        view.update(&mut cx, |view, _| {
            assert_eq!(view.selected_cell, Some((1, 0)))
        });
        cx.simulate_keystrokes("right");
        view.update(&mut cx, |view, _| {
            assert_eq!(view.selected_cell, Some((1, 1)))
        });
        cx.simulate_keystrokes("up");
        view.update(&mut cx, |view, _| {
            assert_eq!(view.selected_cell, Some((0, 1)))
        });
        cx.simulate_keystrokes("left");
        view.update(&mut cx, |view, _| {
            assert_eq!(view.selected_cell, Some((0, 0)))
        });
        // Clamps at the top-left edge.
        cx.simulate_keystrokes("up left");
        view.update(&mut cx, |view, _| {
            assert_eq!(view.selected_cell, Some((0, 0)))
        });
    }

    #[gpui::test]
    fn keyboard_enter_tab_navigate_in_ready_mode(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) = table_backed_result_window(cx);
        let first_cell = debug_center(&mut cx, "CELL-0-0");
        cx.simulate_click(first_cell, gpui::Modifiers::none());
        draw_result_view(window, &mut cx);

        cx.simulate_keystrokes("enter");
        view.update(&mut cx, |view, _| {
            assert_eq!(view.selected_cell, Some((1, 0)))
        });
        cx.simulate_keystrokes("tab");
        view.update(&mut cx, |view, _| {
            assert_eq!(view.selected_cell, Some((1, 1)))
        });
        cx.simulate_keystrokes("shift-enter");
        view.update(&mut cx, |view, _| {
            assert_eq!(view.selected_cell, Some((0, 1)))
        });
        cx.simulate_keystrokes("shift-tab");
        view.update(&mut cx, |view, _| {
            assert_eq!(view.selected_cell, Some((0, 0)))
        });
    }

    #[gpui::test]
    fn keyboard_f2_opens_editor_keeping_value(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) = table_backed_result_window(cx);
        let cell = debug_center(&mut cx, "CELL-0-1");
        cx.simulate_click(cell, gpui::Modifiers::none());
        draw_result_view(window, &mut cx);

        cx.simulate_keystrokes("f2");
        view.update(&mut cx, |view, cx| {
            let text = view
                .cell_edit
                .as_ref()
                .map(|edit| edit.editor.read(cx).text(cx));
            assert!(
                view.cell_edit
                    .as_ref()
                    .is_some_and(|edit| edit.abs_idx == 0 && edit.col_idx == 1),
                "F2 should open the inline editor on the active cell"
            );
            assert!(
                text.is_some_and(|t| !t.is_empty()),
                "F2 must keep the existing cell value, not clear it"
            );
        });
    }

    #[gpui::test]
    fn type_to_replace_overwrites_active_cell(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) = table_backed_result_window(cx);
        let cell = debug_center(&mut cx, "CELL-0-0");
        cx.simulate_click(cell, gpui::Modifiers::none());
        draw_result_view(window, &mut cx);

        view.update_in(&mut cx, |view, window, cx| {
            view.type_to_replace_active_cell("9", window, cx);
        });
        view.update(&mut cx, |view, cx| {
            assert!(view.cell_edit.is_some(), "typing should start an edit");
            assert_eq!(
                view.cell_edit
                    .as_ref()
                    .map(|edit| edit.editor.read(cx).text(cx)),
                Some("9".to_string()),
                "type-to-replace must overwrite the cell with the typed text"
            );
        });
    }

    #[gpui::test]
    fn real_keystrokes_accumulate_in_the_live_editor_while_typing(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) = table_backed_result_window(cx);
        let cell = debug_center(&mut cx, "CELL-0-0");
        cx.simulate_click(cell, gpui::Modifiers::none());
        draw_result_view(window, &mut cx);

        // First keystroke goes through the real on_key_down dispatch (not a
        // direct function call) and should start type-to-replace.
        cx.simulate_keystrokes("h");
        view.update(&mut cx, |view, cx| {
            assert!(
                view.cell_edit.is_some(),
                "typing a printable character should start an edit"
            );
            assert_eq!(
                view.cell_edit
                    .as_ref()
                    .map(|edit| edit.editor.read(cx).text(cx)),
                Some("h".to_string())
            );
        });

        // Second keystroke, while already editing, must reach the live editor
        // and accumulate rather than being swallowed or restarting the edit.
        cx.simulate_keystrokes("i");
        view.update(&mut cx, |view, cx| {
            assert_eq!(
                view.cell_edit
                    .as_ref()
                    .map(|edit| edit.editor.read(cx).text(cx)),
                Some("hi".to_string()),
                "a second real keystroke must append to the live editor, not be lost"
            );
        });
    }

    #[test]
    fn parse_tsv_grid_splits_rows_and_columns() {
        assert_eq!(
            ResultView::parse_tsv_grid("a\tb\nc\td"),
            vec![
                vec!["a".to_string(), "b".to_string()],
                vec!["c".to_string(), "d".to_string()],
            ]
        );
        // A trailing newline is dropped, not turned into an empty row.
        assert_eq!(
            ResultView::parse_tsv_grid("a\tb\n"),
            vec![vec!["a".to_string(), "b".to_string()]]
        );
        // CRLF line endings (Excel on Windows) parse the same as LF.
        assert_eq!(
            ResultView::parse_tsv_grid("a\tb\r\nc\td"),
            vec![
                vec!["a".to_string(), "b".to_string()],
                vec!["c".to_string(), "d".to_string()],
            ]
        );
        assert!(ResultView::parse_tsv_grid("").is_empty());
    }

    #[gpui::test]
    fn real_home_end_and_page_keystrokes_navigate_through_the_production_keymap_binding(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            cx.bind_keys([
                gpui::KeyBinding::new("home", SelectRowStart, Some("DbResultView")),
                gpui::KeyBinding::new("end", SelectRowEnd, Some("DbResultView")),
                gpui::KeyBinding::new("ctrl-home", SelectFirstCell, Some("DbResultView")),
                gpui::KeyBinding::new("ctrl-end", SelectLastCell, Some("DbResultView")),
                gpui::KeyBinding::new("pageup", SelectPageUp, Some("DbResultView")),
                gpui::KeyBinding::new("pagedown", SelectPageDown, Some("DbResultView")),
            ]);
        });

        // 3 rows (id, name) — sample_table_result, identity display order.
        let (window, view, mut cx) = table_backed_result_window(cx);
        let middle_id_cell = debug_center(&mut cx, "CELL-1-0");
        cx.simulate_click(middle_id_cell, gpui::Modifiers::none());
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, _| {
            assert_eq!(view.selected_cell, Some((1, 0)));
        });

        cx.simulate_keystrokes("end");
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, _| {
            assert_eq!(
                view.selected_cell,
                Some((1, 1)),
                "End must jump to the last column of the same row"
            );
        });

        cx.simulate_keystrokes("home");
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, _| {
            assert_eq!(
                view.selected_cell,
                Some((1, 0)),
                "Home must jump to the first column of the same row"
            );
        });

        cx.simulate_keystrokes("ctrl-end");
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, _| {
            assert_eq!(
                view.selected_cell,
                Some((2, 1)),
                "Ctrl+End must jump to the last cell of the whole grid"
            );
        });

        cx.simulate_keystrokes("ctrl-home");
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, _| {
            assert_eq!(
                view.selected_cell,
                Some((0, 0)),
                "Ctrl+Home must jump to the first cell of the whole grid"
            );
        });

        // With only 3 rows, a page-sized jump clamps to the last/first row
        // rather than overshooting past the grid.
        cx.simulate_keystrokes("pagedown");
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, _| {
            assert_eq!(
                view.selected_cell,
                Some((2, 0)),
                "Page Down must clamp to the last row when the grid is shorter than a page"
            );
        });

        cx.simulate_keystrokes("pageup");
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, _| {
            assert_eq!(
                view.selected_cell,
                Some((0, 0)),
                "Page Up must clamp to the first row when the grid is shorter than a page"
            );
        });
    }

    #[gpui::test]
    fn keyboard_shift_arrow_extends_selection(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) = table_backed_result_window(cx);
        let first_cell = debug_center(&mut cx, "CELL-0-0");
        cx.simulate_click(first_cell, gpui::Modifiers::none());
        draw_result_view(window, &mut cx);

        cx.simulate_keystrokes("shift-right");
        cx.simulate_keystrokes("shift-down");
        view.update(&mut cx, |view, _| {
            assert_eq!(
                view.selected_cell,
                Some((0, 0)),
                "the active cell stays put while Shift+Arrow grows the range"
            );
            assert_eq!(view.selected_cell_range, Some(((0, 0), (1, 1))));
        });

        // Shrinking back onto the anchor collapses the range.
        cx.simulate_keystrokes("shift-up");
        cx.simulate_keystrokes("shift-left");
        view.update(&mut cx, |view, _| {
            assert_eq!(view.selected_cell, Some((0, 0)));
            assert_eq!(
                view.selected_cell_range, None,
                "collapsing onto the anchor clears the range"
            );
        });
    }

    #[gpui::test]
    fn select_all_cells_covers_whole_grid(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) = table_backed_result_window(cx);
        let first_cell = debug_center(&mut cx, "CELL-0-0");
        cx.simulate_click(first_cell, gpui::Modifiers::none());
        draw_result_view(window, &mut cx);

        view.update(&mut cx, |view, cx| view.select_all_cells(cx));
        view.update(&mut cx, |view, _| {
            assert_eq!(
                view.selected_cell,
                Some((0, 0)),
                "select-all anchors the active cell at the top-left"
            );
            assert_eq!(view.selected_cell_range, Some(((0, 0), (2, 1))));
        });
    }

    #[gpui::test]
    fn fill_down_copies_top_cell_value_down_a_range(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) = table_backed_result_window(cx);
        let top = debug_center(&mut cx, "CELL-0-1");
        let bottom = debug_center(&mut cx, "CELL-2-1");
        cx.simulate_click(top, gpui::Modifiers::none());
        cx.simulate_click(bottom, gpui::Modifiers::shift());
        draw_result_view(window, &mut cx);

        view.update(&mut cx, |view, cx| view.fill_down(cx));
        view.update(&mut cx, |view, _| {
            assert_eq!(
                view.pending_cell_value(1, 1),
                Some(&CellValue::Text("Alice".to_string())),
                "fill down must copy the top cell value into the rows below"
            );
            assert_eq!(
                view.pending_cell_value(2, 1),
                Some(&CellValue::Text("Alice".to_string()))
            );
            assert_eq!(
                view.pending_cell_value(0, 1),
                None,
                "the source (top) cell must stay untouched"
            );
        });
    }

    #[gpui::test]
    fn fill_down_lone_active_cell_fills_the_cell_below(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) = table_backed_result_window(cx);
        let cell = debug_center(&mut cx, "CELL-0-0");
        cx.simulate_click(cell, gpui::Modifiers::none());
        draw_result_view(window, &mut cx);

        view.update(&mut cx, |view, cx| view.fill_down(cx));
        view.update(&mut cx, |view, _| {
            assert_eq!(
                view.pending_cell_value(1, 0),
                Some(&CellValue::Text("1".to_string())),
                "with no range, fill down still copies into the cell directly below"
            );
        });
    }

    #[gpui::test]
    fn fill_right_copies_left_cell_value_across_a_range(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) = table_backed_result_window(cx);
        let left = debug_center(&mut cx, "CELL-0-0");
        let right = debug_center(&mut cx, "CELL-0-1");
        cx.simulate_click(left, gpui::Modifiers::none());
        cx.simulate_click(right, gpui::Modifiers::shift());
        draw_result_view(window, &mut cx);

        view.update(&mut cx, |view, cx| view.fill_right(cx));
        view.update(&mut cx, |view, _| {
            assert_eq!(
                view.pending_cell_value(0, 1),
                Some(&CellValue::Text("1".to_string())),
                "fill right must copy the left cell value across the row"
            );
            assert_eq!(
                view.pending_cell_value(0, 0),
                None,
                "the source (left) cell must stay untouched"
            );
        });
    }

    #[gpui::test]
    fn edit_undo_then_redo_roundtrips_a_cell_write(cx: &mut gpui::TestAppContext) {
        let (_window, view, mut cx) = table_backed_result_window(cx);

        view.update(&mut cx, |view, cx| {
            view.write_cell_value(0, 1, CellValue::Text("Zed".to_string()), cx);
        });
        view.update(&mut cx, |view, _| {
            assert_eq!(
                view.pending_cell_value(0, 1),
                Some(&CellValue::Text("Zed".to_string()))
            );
        });

        view.update(&mut cx, |view, cx| view.undo_edit(cx));
        view.update(&mut cx, |view, _| {
            assert_eq!(
                view.pending_cell_value(0, 1),
                None,
                "undo must revert the buffered cell edit"
            );
            assert!(view.edit_undo_stack.is_empty());
            assert_eq!(view.edit_redo_stack.len(), 1);
        });

        view.update(&mut cx, |view, cx| view.redo_edit(cx));
        view.update(&mut cx, |view, _| {
            assert_eq!(
                view.pending_cell_value(0, 1),
                Some(&CellValue::Text("Zed".to_string())),
                "redo must re-apply the undone edit"
            );
            assert!(view.edit_redo_stack.is_empty());
        });
    }

    #[gpui::test]
    fn fill_down_is_a_single_undo_unit(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) = table_backed_result_window(cx);
        let top = debug_center(&mut cx, "CELL-0-1");
        let bottom = debug_center(&mut cx, "CELL-2-1");
        cx.simulate_click(top, gpui::Modifiers::none());
        cx.simulate_click(bottom, gpui::Modifiers::shift());
        draw_result_view(window, &mut cx);

        view.update(&mut cx, |view, cx| view.fill_down(cx));
        view.update(&mut cx, |view, cx| view.undo_edit(cx));
        view.update(&mut cx, |view, _| {
            assert_eq!(view.pending_cell_value(1, 1), None);
            assert_eq!(
                view.pending_cell_value(2, 1),
                None,
                "one undo must clear the whole fill batch, not just one cell"
            );
        });

        view.update(&mut cx, |view, cx| view.redo_edit(cx));
        view.update(&mut cx, |view, _| {
            assert_eq!(
                view.pending_cell_value(1, 1),
                Some(&CellValue::Text("Alice".to_string()))
            );
            assert_eq!(
                view.pending_cell_value(2, 1),
                Some(&CellValue::Text("Alice".to_string()))
            );
        });
    }

    // edit_undo_then_redo_roundtrips_a_cell_write and fill_down_is_a_single_undo_unit
    // above each undo/redo a single kind of edit. This proves the undo/redo stack
    // is shared across DIFFERENT edit types (a cell write, then a real row
    // delete through the gutter context menu) and unwinds/replays them in the
    // correct LIFO order, using real ctrl-z/ctrl-y keystrokes (already proven to
    // reach undo_edit/redo_edit in real_keymap_editor_bindings_...) rather than
    // calling undo_edit/redo_edit directly.
    #[gpui::test]
    fn undo_redo_unwinds_a_mixed_sequence_of_cell_edit_and_row_delete(
        cx: &mut gpui::TestAppContext,
    ) {
        let (window, view, mut cx) = table_backed_result_window(cx);

        view.update(&mut cx, |view, cx| {
            view.write_cell_value(0, 1, CellValue::Text("Zed".to_string()), cx);
        });
        draw_result_view(window, &mut cx);

        open_gutter_menu_and_click(window, &mut cx, "GUTTER-2", "MENU_ITEM-Delete Row");
        view.update(&mut cx, |view, _| {
            assert_eq!(
                view.pending_cell_value(0, 1),
                Some(&CellValue::Text("Zed".to_string())),
                "the earlier cell write must still be buffered after the row delete"
            );
            assert!(
                view.deleted_rows.contains(&2),
                "Delete Row from the gutter menu must mark the row"
            );
        });

        // The gutter's right-click menu does not itself focus the grid's own
        // focus handle, so a real click is needed first -- otherwise the
        // ctrl-z/ctrl-y keystrokes below would not even reach on_key_down.
        let first_cell = debug_center(&mut cx, "CELL-0-0");
        cx.simulate_click(first_cell, gpui::Modifiers::none());
        draw_result_view(window, &mut cx);

        cx.simulate_keystrokes("ctrl-z");
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, _| {
            assert!(
                !view.deleted_rows.contains(&2),
                "the first undo must revert the most recent op (the delete), LIFO"
            );
            assert_eq!(
                view.pending_cell_value(0, 1),
                Some(&CellValue::Text("Zed".to_string())),
                "the older cell write must survive undoing the newer delete"
            );
        });

        cx.simulate_keystrokes("ctrl-z");
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, _| {
            assert_eq!(
                view.pending_cell_value(0, 1),
                None,
                "the second undo must revert the cell write too"
            );
        });

        cx.simulate_keystrokes("ctrl-y");
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, _| {
            assert_eq!(
                view.pending_cell_value(0, 1),
                Some(&CellValue::Text("Zed".to_string())),
                "the first redo must replay the cell write, in the original order"
            );
            assert!(!view.deleted_rows.contains(&2));
        });

        cx.simulate_keystrokes("ctrl-y");
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, _| {
            assert!(
                view.deleted_rows.contains(&2),
                "the second redo must replay the row delete"
            );
        });
    }

    #[test]
    fn column_values_look_numeric_requires_all_values_numeric() {
        let all_numbers = [Some("1"), Some("2.5"), Some("-3")];
        assert!(column_values_look_numeric(all_numbers.into_iter()));

        let mixed = [Some("1"), Some("two"), Some("3")];
        assert!(!column_values_look_numeric(mixed.into_iter()));

        // Nulls and empties are skipped, but at least one real number is needed.
        let with_nulls = [None, Some(""), Some("42")];
        assert!(column_values_look_numeric(with_nulls.into_iter()));

        let only_nulls = [None, Some("")];
        assert!(!column_values_look_numeric(only_nulls.into_iter()));

        let empty: [Option<&str>; 0] = [];
        assert!(!column_values_look_numeric(empty.into_iter()));
    }

    #[test]
    fn column_is_numeric_prefers_type_over_values() {
        // A known numeric type is numeric even before any values are seen.
        let no_values: [Option<&str>; 0] = [];
        assert!(column_is_numeric(Some("int"), no_values.into_iter()));
        assert!(column_is_numeric(
            Some("decimal(10,2)"),
            no_values.into_iter()
        ));

        // A known text type is not numeric even when its values look like numbers.
        let numeric_looking = [Some("1"), Some("2")];
        assert!(!column_is_numeric(
            Some("varchar(255)"),
            numeric_looking.into_iter()
        ));

        // With no type metadata, the values decide.
        assert!(column_is_numeric(None, [Some("1"), Some("2")].into_iter()));
        assert!(!column_is_numeric(None, [Some("1"), Some("x")].into_iter()));
    }

    #[test]
    fn compute_column_aggregates_compares_numerics_numerically_not_lexicographically() {
        // "959" > "5110" lexicographically but 5110 is the real numeric maximum; a
        // NULL is present and must be counted, not treated as a value.
        let result = QueryResult {
            columns: vec!["balance".to_string()],
            rows: vec![
                vec![Some("959".to_string())],
                vec![Some("5110".to_string())],
                vec![Some("40".to_string())],
                vec![None],
            ],
            rows_affected: 0,
            execution_time_ms: 0,
            timing: None,
            raw_documents: None,
        };
        let display_order = [0, 1, 2, 3];
        let summary = ResultView::compute_column_aggregates(&result, 0, &display_order);
        assert!(
            summary.contains("MIN 40"),
            "expected numeric MIN 40, got: {summary}"
        );
        assert!(
            summary.contains("MAX 5110"),
            "expected numeric MAX 5110 (not lexicographic 959), got: {summary}"
        );
        assert!(
            summary.contains("NULLS 1"),
            "expected the NULL row to be counted, got: {summary}"
        );
    }

    // Verifies the EXISTING column-header sort (sort_columns + recompute_layout +
    // header on_click) actually works end to end via a real click, and that the
    // comparator is numeric-aware (a lexicographic-only comparator would order
    // "9" after "10"). A plain (non-table-backed) view is required so the click
    // takes the local `recompute_layout` path instead of `refresh_table_data`
    // (which would need a live connection).
    #[gpui::test]
    fn header_click_sorts_a_numeric_column_ascending_then_descending(
        cx: &mut gpui::TestAppContext,
    ) {
        let result = QueryResult {
            columns: vec!["id".to_string(), "balance".to_string()],
            rows: vec![
                vec![Some("1".to_string()), Some("959".to_string())],
                vec![Some("2".to_string()), Some("40".to_string())],
                vec![Some("3".to_string()), Some("5110".to_string())],
                vec![Some("4".to_string()), Some("9".to_string())],
            ],
            rows_affected: 0,
            execution_time_ms: 0,
            timing: None,
            raw_documents: None,
        };
        let (window, view, mut cx) = plain_result_window(cx, result);
        view.update(&mut cx, |view, _| assert!(view.sort_columns.is_empty()));

        let header_center = debug_center(&mut cx, "COL_HEADER-1");
        cx.simulate_click(header_center, gpui::Modifiers::none());
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, _| {
            // Numeric-aware ascending: 9 < 40 < 959 < 5110. A lexicographic
            // comparator would instead put "40" before "5110" before "9" before "959".
            assert_eq!(
                view.order,
                vec![3, 1, 0, 2],
                "ascending sort is not numeric-aware (or sorting did not run)"
            );
        });

        cx.simulate_click(header_center, gpui::Modifiers::none());
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, _| {
            assert_eq!(
                view.order,
                vec![2, 0, 1, 3],
                "a second click on the same header should reverse to descending"
            );
        });
    }

    // header_click_sorts_a_numeric_column_ascending_then_descending (above) only
    // proves single-column sort. Shift-click adds a secondary sort key (the
    // header on_click handler branches on modifiers.shift) -- this had no
    // coverage at all, so nothing would catch a broken tie-breaker.
    #[gpui::test]
    fn shift_click_header_adds_a_secondary_sort_column(cx: &mut gpui::TestAppContext) {
        let result = QueryResult {
            columns: vec!["team".to_string(), "score".to_string()],
            rows: vec![
                vec![Some("b".to_string()), Some("2".to_string())],
                vec![Some("a".to_string()), Some("1".to_string())],
                vec![Some("a".to_string()), Some("3".to_string())],
                vec![Some("b".to_string()), Some("1".to_string())],
            ],
            rows_affected: 0,
            execution_time_ms: 0,
            timing: None,
            raw_documents: None,
        };
        let (window, view, mut cx) = plain_result_window(cx, result);

        let team_header = debug_center(&mut cx, "COL_HEADER-0");
        cx.simulate_click(team_header, gpui::Modifiers::none());
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, _| {
            assert_eq!(
                view.sort_columns,
                vec![SortColumn {
                    col_idx: 0,
                    ascending: true
                }]
            );
        });

        let score_header = debug_center(&mut cx, "COL_HEADER-1");
        cx.simulate_click(score_header, gpui::Modifiers::shift());
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, _| {
            assert_eq!(
                view.sort_columns,
                vec![
                    SortColumn {
                        col_idx: 0,
                        ascending: true
                    },
                    SortColumn {
                        col_idx: 1,
                        ascending: true
                    },
                ],
                "shift-click must add a secondary sort key, not replace the primary one"
            );
            // team ascending (a, a, b, b), tie-broken by score ascending within
            // each team: rows 1 (a,1), 2 (a,3), 3 (b,1), 0 (b,2).
            assert_eq!(
                view.order,
                vec![1, 2, 3, 0],
                "rows must sort by team first, then by score within each team"
            );
        });

        // Shift-clicking the already-secondary column again toggles its direction
        // in place instead of adding a third entry or moving it to primary.
        cx.simulate_click(score_header, gpui::Modifiers::shift());
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, _| {
            assert_eq!(
                view.sort_columns,
                vec![
                    SortColumn {
                        col_idx: 0,
                        ascending: true
                    },
                    SortColumn {
                        col_idx: 1,
                        ascending: false
                    },
                ],
                "shift-clicking an existing secondary sort column must toggle its direction"
            );
            assert_eq!(view.order, vec![2, 1, 0, 3]);
        });
    }

    // export_tsv/compute_column_aggregates already treat a NULL cell distinctly
    // from a real value, but recompute_layout's sort comparator had no coverage
    // at all for how it orders NULLs -- it maps a NULL cell to "" before
    // comparing, so this locks in the resulting (undocumented) behavior:
    // NULLs sort first ascending, last descending, exactly like an empty string
    // would.
    #[gpui::test]
    fn sort_orders_null_cells_like_an_empty_string(cx: &mut gpui::TestAppContext) {
        let result = QueryResult {
            columns: vec!["id".to_string(), "note".to_string()],
            rows: vec![
                vec![Some("1".to_string()), Some("b".to_string())],
                vec![Some("2".to_string()), None],
                vec![Some("3".to_string()), Some("a".to_string())],
            ],
            rows_affected: 0,
            execution_time_ms: 0,
            timing: None,
            raw_documents: None,
        };
        let (window, view, mut cx) = plain_result_window(cx, result);

        let header_center = debug_center(&mut cx, "COL_HEADER-1");
        cx.simulate_click(header_center, gpui::Modifiers::none());
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, _| {
            assert_eq!(
                view.order,
                vec![1, 2, 0],
                "ascending sort must place the NULL row first"
            );
        });

        cx.simulate_click(header_center, gpui::Modifiers::none());
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, _| {
            assert_eq!(
                view.order,
                vec![0, 2, 1],
                "descending sort must place the NULL row last"
            );
        });
    }

    #[gpui::test]
    fn recompute_layout_flags_numeric_columns_by_value(cx: &mut gpui::TestAppContext) {
        let (_window, view, mut cx) = table_backed_result_window(cx);
        view.update(&mut cx, |view, _| {
            // sample_table_result: column 0 (id) is all numbers, column 1 (name) is text.
            assert_eq!(view.numeric_columns, vec![true, false]);
        });
    }

    // Grid-audit item 4: a long value must clip on one line, not wrap and grow the
    // row past its fixed height.
    #[gpui::test]
    fn long_cell_text_never_wraps_onto_a_second_line(cx: &mut gpui::TestAppContext) {
        let result = QueryResult {
            columns: vec!["id".to_string(), "notes".to_string()],
            rows: vec![vec![
                // Not 0/1 so it stays unambiguously numeric-text, not the boolean fallback.
                Some("42".to_string()),
                Some(
                    "a very long note value that would wrap onto a second collided line inside \
                     the fixed-height row if the label were allowed to wrap"
                        .to_string(),
                ),
            ]],
            rows_affected: 0,
            execution_time_ms: 0,
            timing: None,
            raw_documents: None,
        };
        let (_window, _view, mut cx) = table_backed_result_window_with(cx, result);
        // The outer CELL bounds are pinned to GRID_ROW_H and clip overflow, so they
        // stay fixed-height even if the text wraps underneath. Measure the inner
        // text wrapper instead -- that is what actually grows taller when the label
        // wraps onto a second line.
        let text_bounds = cx
            .debug_bounds("CELL_TEXT-0-1")
            .expect("expected the long-text cell's text wrapper to be measurable");
        let single_line_bounds = cx
            .debug_bounds("CELL_TEXT-0-0")
            .expect("expected the short id cell's text wrapper to be measurable");
        assert!(
            text_bounds.size.height <= single_line_bounds.size.height + px(1.),
            "a long cell value must render on a single line: expected height close to a \
             single line's height ({:?}), got {:?}",
            single_line_bounds.size.height,
            text_bounds.size.height
        );
    }

    // Grid-audit item 5: shift-click a 2x2 range and confirm the tint decision the
    // renderer actually uses (`cell_receives_selection_tint`) excludes only the
    // last-clicked corner (the real active cell after a shift-click, per
    // `select_cell_from_click`), tinting the other three cells.
    #[gpui::test]
    fn range_selection_tints_every_cell_except_the_active_corner(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) = table_backed_result_window(cx);
        let first_cell = debug_center(&mut cx, "CELL-0-0");
        let last_cell = debug_center(&mut cx, "CELL-1-1");
        cx.simulate_click(first_cell, gpui::Modifiers::none());
        cx.simulate_click(last_cell, gpui::Modifiers::shift());
        draw_result_view(window, &mut cx);

        view.update(&mut cx, |view, _| {
            // A shift-click moves the active cell to the newly-clicked corner (real
            // spreadsheet behavior), not the original anchor.
            assert_eq!(view.selected_cell, Some((1, 1)));
            for &(abs_idx, display_idx, cell_idx) in
                &[(0usize, 0usize, 0usize), (0, 0, 1), (1, 1, 0)]
            {
                assert!(
                    view.cell_receives_selection_tint(abs_idx, display_idx, cell_idx),
                    "cell ({abs_idx},{cell_idx}) is part of the range and must be tinted"
                );
            }
            assert!(
                !view.cell_receives_selection_tint(1, 1, 1),
                "the active corner (1,1) must not be tinted -- its own border marks it"
            );
        });
    }

    /// Only the declared type decides. A number column holding nothing but zeros
    /// and ones looks exactly like a boolean, and calling it one would show
    /// "false" where the row holds 0 -- stating something the database never said.
    #[test]
    fn only_a_declared_boolean_type_is_shown_as_a_boolean() {
        assert!(column_is_boolean(Some("boolean")));
        assert!(column_is_boolean(Some("bool")));
        assert!(column_is_boolean(Some("tinyint(1)")));
        assert!(column_is_boolean(Some("bit(1)")));

        // Every other declared type keeps its own rendering, whatever it holds.
        assert!(!column_is_boolean(Some("varchar(255)")));
        assert!(!column_is_boolean(Some("mediumint")));
        assert!(!column_is_boolean(Some("smallint unsigned")));
        assert!(!column_is_boolean(Some("tinyint")));
        assert!(!column_is_boolean(Some("int")));

        // No type is no reason to guess.
        assert!(!column_is_boolean(None));
    }

    /// A column of zeros and ones with no type behind it stays a number. Reading
    /// "false" out of a `mediumint` holding 0 is the database being paraphrased,
    /// which is worse than showing the 0 it actually holds.
    #[gpui::test]
    fn a_column_of_zeros_and_ones_stays_a_number_until_a_type_says_otherwise(
        cx: &mut gpui::TestAppContext,
    ) {
        let result = QueryResult {
            columns: vec!["id".to_string(), "override_country_ID".to_string()],
            rows: vec![
                vec![Some("1".to_string()), Some("0".to_string())],
                vec![Some("2".to_string()), Some("1".to_string())],
            ],
            rows_affected: 0,
            execution_time_ms: 0,
            timing: None,
            raw_documents: None,
        };
        let (_window, view, mut cx) = table_backed_result_window_with(cx, result);
        view.update(&mut cx, |view, _| {
            assert!(
                view.column_infos.is_none(),
                "the point of this test is the state before describe_table has loaded"
            );
            assert!(
                !matches!(view.column_kind_at(1), CellEditorKind::Boolean),
                "a column with no declared type must not be read as a boolean"
            );
        });
    }

    // Item 15: a Boolean value maps to plain "true"/"false" text; NULL/Default
    // keep their dim markers rather than a per-row icon.
    #[test]
    fn bool_cell_display_maps_to_true_false_text() {
        assert_eq!(
            bool_cell_display(&CellValue::Text("1".to_string())).0,
            "true"
        );
        assert_eq!(
            bool_cell_display(&CellValue::Text("true".to_string())).0,
            "true"
        );
        assert_eq!(
            bool_cell_display(&CellValue::Text("0".to_string())).0,
            "false"
        );
        assert_eq!(
            bool_cell_display(&CellValue::Text("no".to_string())).0,
            "false"
        );
        assert_eq!(bool_cell_display(&CellValue::Null).0, NULL_MARKER);
        assert_eq!(bool_cell_display(&CellValue::Default).0, DEFAULT_MARKER);
    }

    // Item 15: a Boolean column renders its value through the shared text body
    // (CELL_TEXT), not a per-row icon. Before the fix the Boolean branch emitted
    // an `Icon` with no CELL_TEXT wrapper, so this bound was absent.
    #[gpui::test]
    fn boolean_column_renders_as_text_not_icon(cx: &mut gpui::TestAppContext) {
        let result = QueryResult {
            columns: vec!["id".to_string(), "is_active".to_string()],
            rows: vec![
                vec![Some("1".to_string()), Some("1".to_string())],
                vec![Some("2".to_string()), Some("0".to_string())],
            ],
            rows_affected: 0,
            execution_time_ms: 0,
            timing: None,
            raw_documents: None,
        };
        let (_window, view, mut cx) = table_backed_result_window_with(cx, result);
        // The column is a boolean because the table says so, which is the only
        // thing that makes one.
        view.update(&mut cx, |view, cx| {
            view.column_infos = Some(vec![
                ColumnInfo {
                    name: "id".to_string(),
                    data_type: "int".to_string(),
                    is_nullable: false,
                    column_key: None,
                    default_value: None,
                    extra: String::new(),
                },
                ColumnInfo {
                    name: "is_active".to_string(),
                    data_type: "tinyint(1)".to_string(),
                    is_nullable: false,
                    column_key: None,
                    default_value: None,
                    extra: String::new(),
                },
            ]);
            cx.notify();
        });
        cx.run_until_parked();
        view.update(&mut cx, |view, _| {
            assert!(matches!(view.column_kind_at(1), CellEditorKind::Boolean));
        });
        let text_bounds = cx.debug_bounds("CELL_TEXT-0-1").expect(
            "a Boolean cell must render through the shared text body (CELL_TEXT), not an icon",
        );
        assert!(
            f32::from(text_bounds.size.width) > 0.0 && f32::from(text_bounds.size.height) > 0.0,
            "the Boolean cell's text must paint a real, non-zero area, got {:?}",
            text_bounds.size
        );
    }

    // Item 10: the grid uses DataGrip-tight density -- 22px data rows, a 24px
    // header, and 6px (px_1p5) horizontal cell padding.
    #[gpui::test]
    fn grid_density_matches_datagrip_row_and_header_heights(cx: &mut gpui::TestAppContext) {
        let (_window, _view, mut cx) = table_backed_result_window(cx);
        let row_height = f32::from(
            cx.debug_bounds("CELL-0-0")
                .expect("expected a data cell to be measurable")
                .size
                .height,
        );
        assert!(
            (row_height - 22.0).abs() <= 1.0,
            "data rows must be ~22px tall (DataGrip density), got {row_height}px"
        );
        let header_height = f32::from(
            cx.debug_bounds("COL_HEADER-0")
                .expect("expected a header cell to be measurable")
                .size
                .height,
        );
        assert!(
            (header_height - 24.0).abs() <= 1.0,
            "the header row must be ~24px tall (DataGrip density), got {header_height}px"
        );
        // Column 1 ("name") is left-aligned text, so its inner text wrapper's left
        // inset from the cell's left edge equals the horizontal padding.
        let cell_left = f32::from(
            cx.debug_bounds("CELL-0-1")
                .expect("expected the text cell to be measurable")
                .origin
                .x,
        );
        let text_left = f32::from(
            cx.debug_bounds("CELL_TEXT-0-1")
                .expect("expected the text cell's inner wrapper to be measurable")
                .origin
                .x,
        );
        let left_pad = text_left - cell_left;
        assert!(
            (left_pad - 6.0).abs() <= 1.0,
            "cell horizontal padding must tighten to px_1p5 (6px), got {left_pad}px"
        );
    }

    // Grid-audit item 8: a numeric column's header label sits near the right edge
    // of its header cell (matching its right-aligned data below it); a text
    // column's header label sits near the left edge (unchanged layout).
    #[gpui::test]
    fn numeric_column_headers_hug_the_right_edge(cx: &mut gpui::TestAppContext) {
        let (_window, _view, mut cx) = table_backed_result_window(cx);
        // sample_table_result: column 0 (id) is numeric, column 1 (name) is text.
        let numeric_header = cx
            .debug_bounds("COL_HEADER-0")
            .expect("expected the numeric column's header to be measurable");
        let numeric_content = cx
            .debug_bounds("COL_HEADER_CONTENT-0")
            .expect("expected the numeric column's header label to be measurable");
        let text_header = cx
            .debug_bounds("COL_HEADER-1")
            .expect("expected the text column's header to be measurable");
        let text_content = cx
            .debug_bounds("COL_HEADER_CONTENT-1")
            .expect("expected the text column's header label to be measurable");

        let numeric_right_gap = numeric_header.right() - numeric_content.right();
        let numeric_left_gap = numeric_content.left() - numeric_header.left();
        assert!(
            numeric_right_gap < numeric_left_gap,
            "numeric column header must hug the right edge: right gap {numeric_right_gap:?} \
             should be smaller than left gap {numeric_left_gap:?}"
        );

        let text_left_gap = text_content.left() - text_header.left();
        let text_right_gap = text_header.right() - text_content.right();
        assert!(
            text_left_gap < text_right_gap,
            "text column header must stay left-aligned: left gap {text_left_gap:?} should be \
             smaller than right gap {text_right_gap:?}"
        );
    }

    #[gpui::test]
    fn active_cell_row_and_column_track_the_selected_cell(cx: &mut gpui::TestAppContext) {
        let (_window, view, mut cx) = table_backed_result_window(cx);
        view.update(&mut cx, |view, _| {
            assert_eq!(view.active_cell_row(), None);
            assert_eq!(view.active_cell_column(), None);

            view.selected_cell = Some((2, 1));
            assert_eq!(view.active_cell_row(), Some(2));
            assert_eq!(view.active_cell_column(), Some(1));
        });
    }

    #[gpui::test]
    fn paste_single_value_fills_selection(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) = table_backed_result_window(cx);
        let first_cell = debug_center(&mut cx, "CELL-0-0");
        let last_cell = debug_center(&mut cx, "CELL-1-1");
        cx.simulate_click(first_cell, gpui::Modifiers::none());
        cx.simulate_click(last_cell, gpui::Modifiers::shift());
        draw_result_view(window, &mut cx);

        cx.update(|_, cx| cx.write_to_clipboard(ClipboardItem::new_string("Z".to_string())));
        view.update(&mut cx, |view, cx| view.paste_from_clipboard(cx));

        view.update(&mut cx, |view, _| {
            for &(row, col) in &[(0usize, 0usize), (0, 1), (1, 0), (1, 1)] {
                assert_eq!(
                    view.pending_cell_value(row, col),
                    Some(&CellValue::Text("Z".to_string())),
                    "a single copied value must fill every selected cell ({row},{col})"
                );
            }
            assert_eq!(
                view.pending_cell_value(2, 0),
                None,
                "cells outside the selection must stay untouched"
            );
        });
    }

    #[gpui::test]
    fn paste_grid_fills_right_and_down(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) = table_backed_result_window(cx);
        let first_cell = debug_center(&mut cx, "CELL-0-0");
        cx.simulate_click(first_cell, gpui::Modifiers::none());
        draw_result_view(window, &mut cx);

        cx.update(|_, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string("X\tY\nZ\tW".to_string()))
        });
        view.update(&mut cx, |view, cx| view.paste_from_clipboard(cx));

        view.update(&mut cx, |view, _| {
            assert_eq!(
                view.pending_cell_value(0, 0),
                Some(&CellValue::Text("X".to_string()))
            );
            assert_eq!(
                view.pending_cell_value(0, 1),
                Some(&CellValue::Text("Y".to_string()))
            );
            assert_eq!(
                view.pending_cell_value(1, 0),
                Some(&CellValue::Text("Z".to_string()))
            );
            assert_eq!(
                view.pending_cell_value(1, 1),
                Some(&CellValue::Text("W".to_string()))
            );
        });
    }

    #[gpui::test]
    fn cut_selection_copies_then_clears(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) = table_backed_result_window(cx);
        let cell = debug_center(&mut cx, "CELL-0-1");
        cx.simulate_click(cell, gpui::Modifiers::none());
        draw_result_view(window, &mut cx);

        view.update(&mut cx, |view, cx| view.cut_selection(cx));

        let copied = cx
            .read_from_clipboard()
            .and_then(|clipboard| clipboard.text())
            .expect("cut should write the cut value to the clipboard");
        assert!(
            copied.contains("Alice"),
            "cut must copy the value before clearing it, got {copied:?}"
        );

        view.update(&mut cx, |view, _| {
            assert_eq!(
                view.pending_cell_value(0, 1),
                Some(&CellValue::Text(String::new())),
                "cut must clear the cut cell to an empty value"
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

    // find_matches_locate_substring_across_cells (above) re-implements the match
    // logic inline as a pure function -- it never dispatches the real
    // db_result_view::ToggleFind action, so it would not catch a broken
    // ctrl-f-to-open-find wiring. This test binds the real keymap entry
    // (assets/keymaps/default-linux.json: "ctrl-f": "db_result_view::ToggleFind",
    // context "DbResultView", matching this view's own key_context) and dispatches
    // a genuine ctrl-f keystroke through it.
    #[gpui::test]
    fn real_ctrl_f_keystroke_opens_find_through_the_production_keymap_binding(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            cx.bind_keys([gpui::KeyBinding::new(
                "ctrl-f",
                ToggleFind,
                Some("DbResultView"),
            )]);
        });

        let (window, view, mut cx) = table_backed_result_window(cx);
        let first_cell = debug_center(&mut cx, "CELL-0-0");
        cx.simulate_click(first_cell, gpui::Modifiers::none());
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, _| {
            assert!(view.find_query.is_none(), "find must start closed");
        });

        cx.simulate_keystrokes("ctrl-f");
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, _| {
            assert!(
                view.find_query.is_some(),
                "a real ctrl-f keystroke must open find through the actual production keymap \
                 binding and action dispatch, not just by calling toggle_find directly"
            );
        });
    }

    // find_filter_rows_hides_non_matching_rows/quick_filter_by_cell_narrows_visible_rows
    // (below) set `find_query`/call `recompute_local_filter_inner` directly --
    // they never prove a real user can actually type into the find box and see
    // rows filter. open_find focuses `find_editor`, so a real keystroke typed
    // right after ctrl-f should reach it and drive the same
    // EditorEvent::BufferEdited -> update_find_matches pipeline.
    #[gpui::test]
    fn typing_into_the_find_editor_after_ctrl_f_filters_rows(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            cx.bind_keys([gpui::KeyBinding::new(
                "ctrl-f",
                ToggleFind,
                Some("DbResultView"),
            )]);
        });

        let (window, view, mut cx) = table_backed_result_window(cx);
        view.update(&mut cx, |view, _cx| {
            view.find_filter_rows = true;
        });
        let first_cell = debug_center(&mut cx, "CELL-0-0");
        cx.simulate_click(first_cell, gpui::Modifiers::none());
        draw_result_view(window, &mut cx);

        cx.simulate_keystrokes("ctrl-f");
        draw_result_view(window, &mut cx);

        cx.simulate_keystrokes("b");
        cx.simulate_keystrokes("o");
        cx.simulate_keystrokes("b");
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, _| {
            assert_eq!(
                view.find_query.as_deref(),
                Some("bob"),
                "real keystrokes typed after ctrl-f must reach the focused find editor"
            );
            assert_eq!(
                view.filtered_display_order,
                vec![1],
                "typing a query with row filtering on must actually filter the visible rows, \
                 through the real editor -> BufferEdited -> update_find_matches pipeline"
            );
        });
    }

    // visual_copy_selection_uses_selected_copy_format (below) and
    // copy_selection_uses_insert_copy_format call copy_selected_to_clipboard
    // directly -- they never prove a real ctrl-c keystroke reaches it. The
    // production keymap binds ctrl-c to THREE different actions across contexts
    // (menu::Cancel, editor::Copy in context "Editor", db_result_view::CopySelection
    // in context "DbResultView") -- this test binds the real db_result_view entry
    // and confirms a genuine keystroke, with no cell being edited (no "Editor"
    // context in the focus chain), resolves to the grid's own copy action.
    #[gpui::test]
    fn real_ctrl_c_keystroke_copies_the_selection_through_the_production_keymap_binding(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            cx.bind_keys([gpui::KeyBinding::new(
                "ctrl-c",
                CopySelection,
                Some("DbResultView"),
            )]);
        });

        let (window, view, mut cx) = table_backed_result_window(cx);
        let row_number = debug_center(&mut cx, "GUTTER-1");
        cx.simulate_click(row_number, gpui::Modifiers::none());
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, _cx| {
            view.copy_format = CopyFormat::Csv;
        });

        cx.simulate_keystrokes("ctrl-c");
        draw_result_view(window, &mut cx);

        let copied = cx
            .read_from_clipboard()
            .and_then(|clipboard| clipboard.text())
            .expect(
                "a real ctrl-c keystroke must write the selection to the clipboard through \
                     the production keymap binding and action dispatch",
            );
        assert_eq!(copied, "id,name\n2,Bob\n");
    }

    // TSV is `copy_format`'s default (never overridden here, unlike the sibling
    // tests above) and the one Excel/Sheets round-trips through paste -- yet it
    // had no coverage: nothing exercised export_tsv at all. This selects a 2x2
    // CELL RANGE (not a full-row selection, which the sibling tests already
    // cover), so it also proves selected_columns_for_copy/export_tsv only emit
    // the selected columns' header, not every column in the result.
    #[gpui::test]
    fn real_ctrl_c_keystroke_copies_a_cell_range_as_tsv_by_default(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            cx.bind_keys([gpui::KeyBinding::new(
                "ctrl-c",
                CopySelection,
                Some("DbResultView"),
            )]);
        });

        let (window, view, mut cx) = table_backed_result_window(cx);
        let first_cell = debug_center(&mut cx, "CELL-0-0");
        let last_cell = debug_center(&mut cx, "CELL-1-1");
        cx.simulate_click(first_cell, gpui::Modifiers::none());
        cx.simulate_click(last_cell, gpui::Modifiers::shift());
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, _cx| {
            assert_eq!(view.copy_format, CopyFormat::Tsv);
        });

        cx.simulate_keystrokes("ctrl-c");
        draw_result_view(window, &mut cx);

        let copied = cx
            .read_from_clipboard()
            .and_then(|clipboard| clipboard.text())
            .expect(
                "a real ctrl-c keystroke must write the selected range to the clipboard \
                     as TSV by default",
            );
        assert_eq!(copied, "id\tname\n1\tAlice\n2\tBob\n");
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
            "INSERT INTO `users` (`id`, `name`) VALUES (2, 'Bob');\n"
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
            "INSERT INTO `users` (`id`, `name`) VALUES\n  (1, 'Alice'),\n  (2, 'Bob');\n"
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
            timing: None,
            raw_documents: None,
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
            timing: None,
            raw_documents: None,
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
            timing: None,
            raw_documents: None,
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

    struct ReadOnlyProbeProvider;

    #[async_trait::async_trait]
    impl db_client::provider::DbProvider for ReadOnlyProbeProvider {
        async fn ping(&self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn list_databases(&self) -> anyhow::Result<Vec<db_client::schema::DatabaseInfo>> {
            Ok(Vec::new())
        }
        async fn list_tables(
            &self,
            _database: &str,
        ) -> anyhow::Result<Vec<db_client::schema::TableInfo>> {
            Ok(Vec::new())
        }
        async fn describe_table(
            &self,
            _database: &str,
            _table: &str,
        ) -> anyhow::Result<Vec<ColumnInfo>> {
            Ok(Vec::new())
        }
        async fn execute_query(&self, _database: &str, _sql: &str) -> anyhow::Result<QueryResult> {
            Ok(QueryResult {
                columns: Vec::new(),
                rows: Vec::new(),
                rows_affected: 0,
                execution_time_ms: 0,
                timing: None,
                raw_documents: None,
            })
        }
        async fn get_table_ddl(&self, _database: &str, _table: &str) -> anyhow::Result<String> {
            Ok(String::new())
        }
    }

    // A real cell edit (double-click, clear, type, Enter to commit-and-move)
    // followed by the real SubmitEdits action must be rejected on a read-only
    // connection instead of silently reaching the database. This builds its
    // own window (instead of `table_backed_result_window`) because that
    // helper's `DatabaseStore` is a local variable dropped once it returns,
    // whereas this test needs a store entity that stays alive so it can be
    // read after the connection is added.
    #[gpui::test]
    fn submit_edits_is_rejected_with_a_clear_error_on_a_read_only_connection(
        cx: &mut gpui::TestAppContext,
    ) {
        init_result_view_test(cx);
        let conn_id = uuid::Uuid::new_v4();
        let store = cx.update(|cx| {
            let store = cx.new(DatabaseStore::new);
            store.update(cx, |store, cx| {
                let mut config = db_client::ConnectionConfig::default();
                config.id = conn_id;
                config.label = "prod".to_string();
                config.read_only = true;
                store.add_connected_for_test(config, Arc::new(ReadOnlyProbeProvider), cx);
            });
            store
        });

        let result = sample_table_result();
        let window = cx.add_window({
            let store = store.downgrade();
            move |window, cx| {
                let mut view = ResultView::new("users", cx).with_table_context(
                    store,
                    conn_id,
                    "public".to_string(),
                    "users".to_string(),
                    window,
                    cx,
                );
                view.set_result(result, cx);
                view
            }
        });
        let view = window.root(cx).unwrap();
        let mut cx = gpui::VisualTestContext::from_window(window.into(), cx);
        draw_result_view(window, &mut cx);

        view.update(&mut cx, |view, _cx| {
            view.primary_key_columns = Some(vec!["id".to_string()]);
        });

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

        for _ in 0.."Alice".len() {
            cx.dispatch_action(editor::actions::Backspace);
        }
        cx.simulate_keystrokes("z e d");
        cx.simulate_keystrokes("enter");
        draw_result_view(window, &mut cx);

        view.update(&mut cx, |view, _cx| {
            assert_eq!(
                view.pending_cell_value(0, 1),
                Some(&CellValue::Text("zed".to_string())),
                "the real edit must be buffered as a pending change before Submit"
            );
        });

        cx.dispatch_action(SubmitEdits);
        cx.run_until_parked();
        draw_result_view(window, &mut cx);

        view.update(&mut cx, |view, _cx| {
            let error = view
                .error
                .as_deref()
                .expect("a read-only connection must surface a clear rejection error");
            assert!(
                error.contains("read-only"),
                "the error should explain the write was blocked: {error}"
            );
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
        // Real ctrl-shift-c keystroke through the production binding
        // (assets/keymaps/default-linux.json: "DbResultView" -> CopyAggregation)
        // instead of calling copy_aggregation_to_clipboard directly.
        cx.update(|cx| {
            cx.bind_keys([gpui::KeyBinding::new(
                "ctrl-shift-c",
                CopyAggregation,
                Some("DbResultView"),
            )]);
        });
        let (window, _view, mut cx) = table_backed_result_window(cx);
        let cell = debug_center(&mut cx, "CELL-0-0");
        cx.simulate_click(cell, gpui::Modifiers::none());
        draw_result_view(window, &mut cx);

        cx.simulate_keystrokes("ctrl-shift-c");
        draw_result_view(window, &mut cx);

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
        // Drives the real ToggleTranspose action (the "Transpose" view-menu
        // entry dispatches this same action) instead of calling
        // toggle_transpose directly, proving the .on_action wiring.
        let (window, view, mut cx) = table_backed_result_window(cx);
        view.update(&mut cx, |view, _cx| assert!(!view.transposed));
        let first_cell = debug_center(&mut cx, "CELL-0-0");
        cx.simulate_click(first_cell, gpui::Modifiers::none());
        draw_result_view(window, &mut cx);

        cx.dispatch_action(ToggleTranspose);
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, _cx| assert!(view.transposed));
        assert!(
            cx.debug_bounds("TRANSPOSE_VIEW").is_some(),
            "transposed view should render"
        );
    }

    // The normal grid and the transposed view are two independent rendering
    // paths over the same `result`/`pending_edits` state (see .rules on twin
    // render paths). The normal grid always shows a pending edit over the
    // loaded value; this proves the transposed view does too, instead of
    // silently falling back to the stale loaded row once a fix regresses.
    #[gpui::test]
    fn transposed_view_shows_pending_edit_not_stale_loaded_value(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) = table_backed_result_window(cx);
        view.update(&mut cx, |view, cx| {
            view.write_cell_value(0, 1, CellValue::Text("Edited".to_string()), cx);
            view.toggle_transpose(cx);
        });
        draw_result_view(window, &mut cx);

        assert!(
            cx.debug_bounds("TCELL-0-1-Edited").is_some(),
            "the transposed view must show the pending edit (\"Edited\"), not the stale \
             loaded value (\"Alice\"), for a cell that was edited before switching views"
        );
        assert!(
            cx.debug_bounds("TCELL-0-1-Alice").is_none(),
            "the transposed view must not still be rendering the pre-edit loaded value"
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

    // A results tab is reused across unrelated queries (one tab per
    // connection, per `show_result_in_pane`'s doc comment in panel.rs).
    // Hiding a column while browsing one query's result must not silently
    // hide a same-indexed but semantically unrelated column of the next,
    // completely different query run in that same tab.
    #[gpui::test]
    fn set_query_result_clears_hidden_columns_from_a_previous_unrelated_query(
        cx: &mut gpui::TestAppContext,
    ) {
        let (_window, view, mut cx) = table_backed_result_window(cx);
        let connection_id = uuid::Uuid::new_v4();
        view.update(&mut cx, |view, cx| {
            let store = cx.new(DatabaseStore::new);
            view.hidden_columns.insert(1);
            view.recompute_layout();
            assert_eq!(
                view.visible_columns,
                vec![0],
                "sanity check: hiding column 1 of the 2-column sample result leaves only column 0 visible"
            );

            view.set_query_result(
                store.downgrade(),
                connection_id,
                "public".to_string(),
                "SELECT * FROM some_other_table".to_string(),
                wide_table_result(),
                cx,
            );

            assert!(
                view.hidden_columns.is_empty(),
                "a brand-new, unrelated query's result must show all of its own columns, \
                 not have columns silently hidden by a stale index left over from a \
                 previous, different query's hidden-columns selection"
            );
            assert_eq!(view.visible_columns, (0..18).collect::<Vec<_>>());
        });
    }

    // FK navigation (`navigate_to_fk_row`) reuses the same `ResultView` for an
    // unrelated jump to a different table via `run_sql`, not `set_query_result`
    // -- it needs the identical hidden-columns reset for the same reason.
    #[gpui::test]
    fn run_sql_clears_hidden_columns_from_a_previous_unrelated_query(
        cx: &mut gpui::TestAppContext,
    ) {
        let (_window, view, mut cx) = table_backed_result_window(cx);
        let connection_id = uuid::Uuid::new_v4();
        view.update(&mut cx, |view, cx| {
            let store = cx.new(DatabaseStore::new);
            view.hidden_columns.insert(1);

            view.run_sql(
                store.downgrade(),
                connection_id,
                "public".to_string(),
                "SELECT * FROM some_other_table".to_string(),
                cx,
            );

            assert!(
                view.hidden_columns.is_empty(),
                "an FK-navigation jump to a different table must not carry over a \
                 hidden-column index from whatever table was shown before"
            );
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

    // Large, uniquely-identifiable dataset for scroll/virtualization regression
    // tests: `row_{n}` in column 0 and `pair-{n:04}` in column 1, so a rendered
    // value can be checked against its row index without any ambiguity.
    fn many_row_result(count: usize) -> QueryResult {
        QueryResult {
            columns: vec!["id".to_string(), "pair".to_string()],
            rows: (0..count)
                .map(|row| vec![Some(format!("row_{row}")), Some(format!("pair-{row:04}"))])
                .collect(),
            rows_affected: 0,
            execution_time_ms: 0,
            timing: None,
            raw_documents: None,
        }
    }

    // Standing regression harness for the "active cell renders blank" class of
    // bug (reported live in the running app; two rounds of small, unscrolled
    // `#[gpui::test]`s failed to reproduce it). Scans `probe_range` for every
    // currently on-screen cell in `col_idx` (via its debug selector, so this
    // only inspects rows the virtualized list actually painted) and asserts
    // each one's read-mode value is exactly its known-good data — not blank,
    // not another row's value. Call this after any click/scroll/edit sequence
    // to assert the grid never desyncs from its underlying data. Extend
    // `probe_range` and the click/scroll sequence around the call site to
    // cover new interactions; the assertion logic itself should not need to
    // change.
    fn assert_visible_cells_match_data(
        cx: &mut gpui::VisualTestContext,
        view: &Entity<ResultView>,
        col_idx: usize,
        probe_range: std::ops::Range<usize>,
        expected: impl Fn(usize) -> Option<String>,
    ) -> usize {
        let mut checked = 0;
        for abs_idx in probe_range {
            let Some(expected_value) = expected(abs_idx) else {
                continue;
            };
            let selector: &'static str = format!("CELL-{abs_idx}-{col_idx}").leak();
            if cx.debug_bounds(selector).is_none() {
                continue;
            }
            checked += 1;
            view.update(cx, |view, _cx| {
                let rendered = view
                    .pending_cell_value(abs_idx, col_idx)
                    .map(|value| match value {
                        CellValue::Text(text) => text.clone(),
                        CellValue::Null | CellValue::Default => String::new(),
                    })
                    .or_else(|| view.loaded_cell_value(abs_idx, col_idx))
                    .unwrap_or_default();
                assert_eq!(
                    rendered, expected_value,
                    "visible cell ({abs_idx}, {col_idx}) does not match its known data"
                );
            });
        }
        checked
    }

    #[gpui::test]
    fn scrolled_click_selects_the_correct_row_and_renders_its_real_value(
        cx: &mut gpui::TestAppContext,
    ) {
        let (window, view, mut cx) = framed_plain_result_window(cx, many_row_result(200));

        view.update(&mut cx, |view, cx| {
            view.scroll_handle
                .set_offset(gpui::point(px(0.), px(-1300.)));
            cx.notify();
        });
        draw_result_view_frame(window, &mut cx);

        // Find a row the virtualized list actually painted after scrolling;
        // don't assume a fixed viewport size.
        let visible_idx = (0..200)
            .find(|idx| {
                cx.debug_bounds(format!("CELL-{idx}-1").leak() as &str)
                    .is_some()
            })
            .expect("scrolling should bring some row other than 0 into view");
        assert!(
            visible_idx > 0,
            "the scroll offset should have moved the viewport past row 0, got row {visible_idx}"
        );

        let cell_center = debug_center(&mut cx, format!("CELL-{visible_idx}-1").leak());
        cx.simulate_click(cell_center, gpui::Modifiers::none());
        draw_result_view_frame(window, &mut cx);

        view.update(&mut cx, |view, _cx| {
            assert_eq!(
                view.selected_cell,
                Some((visible_idx, 1)),
                "clicking a cell after scrolling must select the row actually under the cursor"
            );
            let rendered = view.loaded_cell_value(visible_idx, 1).unwrap_or_default();
            assert_eq!(rendered, format!("pair-{visible_idx:04}"));
        });

        assert_visible_cells_match_data(&mut cx, &view, 1, 0..200, |idx| {
            Some(format!("pair-{idx:04}"))
        });
    }

    #[gpui::test]
    fn chained_scroll_edit_commit_click_sequence_never_blanks_a_cell(
        cx: &mut gpui::TestAppContext,
    ) {
        let (window, view, mut cx) = framed_plain_result_window(cx, many_row_result(150));

        view.update(&mut cx, |view, cx| {
            view.scroll_handle
                .set_offset(gpui::point(px(0.), px(-780.)));
            cx.notify();
        });
        draw_result_view_frame(window, &mut cx);

        let visible_rows: Vec<usize> = (0..150)
            .filter(|idx| {
                cx.debug_bounds(format!("CELL-{idx}-1").leak() as &str)
                    .is_some()
            })
            .collect();
        assert!(
            visible_rows.len() >= 3,
            "need at least 3 on-screen rows to chain edit/commit/click, saw {}",
            visible_rows.len()
        );
        let row_a = visible_rows[0];
        let row_b = visible_rows[1];
        let row_c = visible_rows[2];

        // Edit row A, commit by clicking away to row B.
        let cell_a = debug_center(&mut cx, format!("CELL-{row_a}-1").leak());
        cx.simulate_click(cell_a, gpui::Modifiers::none());
        draw_result_view_frame(window, &mut cx);
        cx.simulate_keystrokes("f2");
        cx.simulate_keystrokes("x y z");
        let cell_b = debug_center(&mut cx, format!("CELL-{row_b}-1").leak());
        cx.simulate_click(cell_b, gpui::Modifiers::none());
        draw_result_view_frame(window, &mut cx);

        view.update(&mut cx, |view, _cx| {
            assert!(
                view.cell_edit.is_none(),
                "clicking away from an editing cell must commit and close the editor"
            );
        });

        // Edit row B, commit by clicking away to row C.
        cx.simulate_click(cell_b, gpui::Modifiers::none());
        draw_result_view_frame(window, &mut cx);
        cx.simulate_keystrokes("f2");
        cx.simulate_keystrokes("q r");
        let cell_c = debug_center(&mut cx, format!("CELL-{row_c}-1").leak());
        cx.simulate_click(cell_c, gpui::Modifiers::none());
        draw_result_view_frame(window, &mut cx);

        // Click back to row A: its committed edit must render, not blank out.
        cx.simulate_click(cell_a, gpui::Modifiers::none());
        draw_result_view_frame(window, &mut cx);

        view.update(&mut cx, |view, _cx| {
            assert_eq!(view.selected_cell, Some((row_a, 1)));
            let rendered_a = view
                .pending_cell_value(row_a, 1)
                .map(|value| match value {
                    CellValue::Text(text) => text.clone(),
                    CellValue::Null | CellValue::Default => String::new(),
                })
                .or_else(|| view.loaded_cell_value(row_a, 1))
                .unwrap_or_default();
            assert!(
                !rendered_a.is_empty(),
                "row A must not render blank after an edit/commit/click-away/click-back cycle"
            );
            assert!(
                rendered_a.starts_with(&format!("pair-{row_a:04}")),
                "row A's committed edit should extend its original value, got {rendered_a:?}"
            );

            let rendered_c = view.loaded_cell_value(row_c, 1).unwrap_or_default();
            assert_eq!(
                rendered_c,
                format!("pair-{row_c:04}"),
                "row C, never edited, must still show its untouched data"
            );
        });

        // Full-sweep invariant: every currently-visible cell in the column
        // still matches its known data (edited rows extend the original
        // value; untouched rows are unchanged).
        assert_visible_cells_match_data(&mut cx, &view, 1, 0..150, |idx| {
            if idx == row_a || idx == row_b {
                None // checked individually above with their edited values
            } else {
                Some(format!("pair-{idx:04}"))
            }
        });
    }

    #[gpui::test]
    fn double_click_type_and_escape_on_a_numeric_column(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) = table_backed_result_window(cx);
        // `column_kind_at` only returns `Numeric` when schema metadata is
        // loaded (unlike `numeric_columns`, which is value-sampled and drives
        // read-mode right-alignment independently) — set it directly so this
        // test exercises the real `CellEditorKind::Numeric` editor path
        // (icon + placeholder) that all prior investigations, which only used
        // the "name" text column, never touched.
        view.update(&mut cx, |view, _cx| {
            view.column_infos = Some(vec![
                ColumnInfo {
                    name: "id".to_string(),
                    data_type: "int".to_string(),
                    is_nullable: false,
                    column_key: None,
                    default_value: None,
                    extra: String::new(),
                },
                ColumnInfo {
                    name: "name".to_string(),
                    data_type: "varchar(255)".to_string(),
                    is_nullable: true,
                    column_key: None,
                    default_value: None,
                    extra: String::new(),
                },
            ]);
        });
        view.update(&mut cx, |view, _cx| {
            assert!(matches!(view.column_kind_at(0), CellEditorKind::Numeric));
        });

        let cell_center = debug_center(&mut cx, "CELL-0-0");
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
                .map(|edit| edit.editor.read(cx).text(cx));
            assert_eq!(
                text,
                Some("1".to_string()),
                "double-clicking a numeric cell must open the editor pre-filled with its value, not blank"
            );
        });

        cx.simulate_keystrokes("5");
        view.update(&mut cx, |view, cx| {
            let text = view
                .cell_edit
                .as_ref()
                .map(|edit| edit.editor.read(cx).text(cx));
            assert_eq!(
                text,
                Some("15".to_string()),
                "a real keystroke into a numeric cell's live editor must appear immediately"
            );
        });

        cx.simulate_keystrokes("escape");
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, _cx| {
            assert!(
                view.cell_edit.is_none(),
                "escape must cancel the in-progress edit"
            );
            let rendered = view.loaded_cell_value(0, 0).unwrap_or_default();
            assert_eq!(
                rendered, "1",
                "after escape the cell must show only its original value, not the typed text or a concatenation of both"
            );
        });
    }

    // The test above passes even against the pre-fix code, because this
    // crate's test keymap has no binding for "escape" at all, so the
    // keystroke falls straight through to the raw key capture/bubble path --
    // it does not exercise what actually happens once "escape" resolves to
    // the `editor::Cancel` action (its real, production keymap binding in the
    // focused Editor's own "Editor" context; see
    // assets/keymaps/default-linux.json). Dispatching that resolved action
    // directly to the focused node (bypassing keystroke-to-action resolution
    // entirely, exactly like `Window::dispatch_action` does once a keymap
    // match is found) reproduces the real gap: the Editor's own `cancel()`
    // handler finds nothing internal to dismiss for a plain single-line cell
    // editor and calls `cx.propagate()`, and without a listener on our
    // wrapper the action is never seen again by anything that would cancel
    // the cell edit.
    #[gpui::test]
    fn escape_action_cancels_edit_even_when_the_editor_claims_it_first(
        cx: &mut gpui::TestAppContext,
    ) {
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
            assert!(
                view.cell_edit.is_some(),
                "double-click must open the inline editor before escape is tested"
            );
        });

        cx.simulate_keystrokes("x");
        cx.dispatch_action(editor::actions::Cancel);
        draw_result_view(window, &mut cx);

        view.update(&mut cx, |view, _cx| {
            assert!(
                view.cell_edit.is_none(),
                "the editor::Cancel action must cancel the cell edit even though the focused \
                 Editor's own cancel handler claims (and then re-propagates) it first"
            );
            let rendered = view.loaded_cell_value(0, 1).unwrap_or_default();
            assert_eq!(
                rendered, "Alice",
                "after cancelling, the cell must show only its original value, not the typed text"
            );
        });
    }

    fn table_result_with_status_column() -> QueryResult {
        QueryResult {
            columns: vec!["id".to_string(), "status".to_string()],
            rows: vec![
                vec![Some("1".to_string()), Some("red".to_string())],
                vec![Some("2".to_string()), Some("green".to_string())],
                vec![Some("3".to_string()), Some("red".to_string())],
            ],
            rows_affected: 0,
            execution_time_ms: 0,
            timing: None,
            raw_documents: None,
        }
    }

    fn table_result_with_customer_fk_column() -> QueryResult {
        QueryResult {
            columns: vec!["id".to_string(), "customer_id".to_string()],
            rows: vec![
                vec![Some("1".to_string()), Some("42".to_string())],
                vec![Some("2".to_string()), Some("43".to_string())],
            ],
            rows_affected: 0,
            execution_time_ms: 0,
            timing: None,
            raw_documents: None,
        }
    }

    // navigate_to_fk_row used to hardcode MySQL-style backtick quoting for the
    // generated lookup query, which produced invalid SQL against PostgreSQL
    // (and SQLite) connections, whose identifier quote character is `"`.
    #[gpui::test]
    fn navigate_to_fk_row_quotes_identifiers_for_the_active_driver(cx: &mut gpui::TestAppContext) {
        init_result_view_test(cx);
        let connection_id = uuid::Uuid::new_v4();
        let store = cx.update(|cx| {
            let store = cx.new(DatabaseStore::new);
            store.update(cx, |store, cx| {
                let mut config = db_client::ConnectionConfig::default();
                config.id = connection_id;
                config.driver = DatabaseDriver::PostgreSQL;
                // A connected fake provider, not `add_connection`, so
                // `run_sql`'s background fill never attempts a real network
                // connection using this driver's default host/port.
                store.add_connected_for_test(config, Arc::new(ReadOnlyProbeProvider), cx);
            });
            store
        });

        let window = cx.add_window({
            let store = store.downgrade();
            move |window, cx| {
                let mut view = ResultView::new("orders", cx).with_table_context(
                    store,
                    connection_id,
                    "public".to_string(),
                    "orders".to_string(),
                    window,
                    cx,
                );
                view.set_result(table_result_with_customer_fk_column(), cx);
                view.fk_columns.insert(
                    1,
                    FkInfo {
                        name: "orders_customer_id_fkey".to_string(),
                        from_column: "customer_id".to_string(),
                        to_table: "customers".to_string(),
                        to_column: "id".to_string(),
                    },
                );
                view
            }
        });
        let view = window.root(cx).unwrap();
        let mut cx = gpui::VisualTestContext::from_window(window.into(), cx);
        draw_result_view(window, &mut cx);

        let fk_cell = debug_center(&mut cx, "CELL-0-1");
        cx.simulate_click(fk_cell, gpui::Modifiers::none());
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, _cx| {
            assert_eq!(
                view.selected_cell,
                Some((0, 1)),
                "a real click on the FK cell should select it"
            );
        });

        cx.dispatch_action(NavigateToFkRow);
        draw_result_view(window, &mut cx);

        view.update(&mut cx, |view, _cx| {
            assert_eq!(
                view.base_sql.as_deref(),
                Some(r#"SELECT * FROM "customers" WHERE "id" = '42'"#),
                "a real NavigateToFkRow dispatch must quote identifiers using the active \
                 connection's driver (double quotes for PostgreSQL), not hardcoded MySQL backticks"
            );
        });
    }

    // Registers the same "enter"/"shift-enter" -> ConfirmCompletion/ConfirmCompletionReplace
    // bindings the production keymap uses in the "Editor && showing_completions"
    // context (assets/keymaps/default-linux.json), since this crate's test
    // keymap has none of the editor completion bindings by default (see the
    // comment above escape_action_cancels_edit_even_when_the_editor_claims_it_first
    // for why that gap matters and how prior tests worked around it).
    fn bind_editor_completion_keys(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            cx.bind_keys([
                gpui::KeyBinding::new(
                    "enter",
                    editor::actions::ConfirmCompletion::default(),
                    Some("Editor && showing_completions"),
                ),
                gpui::KeyBinding::new(
                    "shift-enter",
                    editor::actions::ConfirmCompletionReplace,
                    Some("Editor && showing_completions"),
                ),
            ]);
        });
    }

    #[gpui::test]
    fn cell_editor_offers_completions_from_the_loaded_column_values(cx: &mut gpui::TestAppContext) {
        bind_editor_completion_keys(cx);
        let (window, view, mut cx) =
            table_backed_result_window_with(cx, table_result_with_status_column());

        // Row 1's "status" cell is "green"; double-click opens the editor with
        // the caret at the end of that text (CellEditEntry::CursorEnd).
        let cell_center = debug_center(&mut cx, "CELL-1-1");
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
                .map(|edit| edit.editor.read(cx).text(cx));
            assert_eq!(text, Some("green".to_string()));
        });

        // Clear "green" with the real delete-backward action (this test
        // harness loads no production keymap, so a raw "backspace" keystroke
        // never resolves to it -- see the identical note on
        // typing_then_clearing_a_null_cell_before_commit_keeps_it_null above),
        // then type "r" as a real keystroke into the focused editor,
        // exercising its normal input/completion pipeline (not a direct
        // model mutation).
        for _ in 0.."green".len() {
            cx.dispatch_action(editor::actions::Backspace);
        }
        cx.simulate_keystrokes("r");
        draw_result_view(window, &mut cx);

        let editor = view.update(&mut cx, |view, _cx| {
            view.cell_edit
                .as_ref()
                .expect("cell editor should still be open")
                .editor
                .clone()
        });
        editor.update(&mut cx, |editor, _cx| {
            let labels: Vec<String> = editor
                .current_completions()
                .expect("typing 'r' should open a completion menu")
                .iter()
                .map(|completion| completion.label.text.clone())
                .collect();
            assert_eq!(
                labels,
                vec!["red".to_string()],
                "only the loaded column value starting with 'r' should be offered"
            );
        });

        // First Enter: the production keymap resolves "enter" to
        // editor::ConfirmCompletion while the menu is open, so it must accept
        // the completion, not commit the cell edit.
        cx.simulate_keystrokes("enter");
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, cx| {
            assert!(
                view.cell_edit.is_some(),
                "accepting a completion must not itself commit and close the cell edit"
            );
            let text = view
                .cell_edit
                .as_ref()
                .map(|edit| edit.editor.read(cx).text(cx));
            assert_eq!(
                text,
                Some("red".to_string()),
                "accepting the completion should fill in the full matched value"
            );
        });
        editor.update(&mut cx, |editor, _cx| {
            assert!(
                editor.current_completions().is_none(),
                "the completion menu must close once a completion is accepted"
            );
        });

        // Second Enter: no completion menu is showing anymore, so "enter" now
        // falls through to this view's own raw key handler, which commits the
        // edit and moves to the next cell (Excel-style commit-and-move) --
        // it must not still be editing the just-accepted (1, 1) cell.
        cx.simulate_keystrokes("enter");
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, _cx| {
            assert_ne!(
                view.cell_edit
                    .as_ref()
                    .map(|edit| (edit.abs_idx, edit.col_idx)),
                Some((1, 1)),
                "the second enter must commit the accepted completion and move off the cell"
            );
            let rendered = view.pending_cell_value(1, 1).map(|value| match value {
                CellValue::Text(text) => text.clone(),
                other => panic!("expected a text edit, got {other:?}"),
            });
            assert_eq!(rendered, Some("red".to_string()));
        });
    }

    #[gpui::test]
    fn cell_editor_does_not_offer_value_completion_on_numeric_columns(
        cx: &mut gpui::TestAppContext,
    ) {
        let (window, view, mut cx) = table_backed_result_window(cx);
        view.update(&mut cx, |view, _cx| {
            view.column_infos = Some(vec![
                ColumnInfo {
                    name: "id".to_string(),
                    data_type: "int".to_string(),
                    is_nullable: false,
                    column_key: None,
                    default_value: None,
                    extra: String::new(),
                },
                ColumnInfo {
                    name: "name".to_string(),
                    data_type: "varchar(255)".to_string(),
                    is_nullable: true,
                    column_key: None,
                    default_value: None,
                    extra: String::new(),
                },
            ]);
        });

        let cell_center = debug_center(&mut cx, "CELL-0-0");
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

        let editor = view.update(&mut cx, |view, _cx| {
            view.cell_edit
                .as_ref()
                .expect("double-click should open the numeric cell editor")
                .editor
                .clone()
        });
        editor.update(&mut cx, |editor, _cx| {
            assert!(
                editor.completion_provider().is_none(),
                "numeric columns must not get value-completion (doc: TEXT-kind only)"
            );
        });
    }

    // The Escape bug (escape_action_cancels_edit_even_when_the_editor_claims_it_first
    // above) existed because a real focused `Editor` entity let the production
    // keymap's "Editor"-context binding claim the key before this view's own raw
    // handler ever saw it. The grid's select-all/undo/fill/paste/cut shortcuts are
    // matched the same raw way (see the "a"/"z"/"d"/"r" branches in this view's
    // key handler) and the production keymap binds several of those same letters
    // to "Editor"-context actions (ctrl-a -> editor::SelectAll, ctrl-z ->
    // editor::Undo, per assets/keymaps/default-linux.json) -- so the same failure
    // mode is structurally possible here too, and deserves the same real-keymap
    // proof rather than an internal-function-call test. This view's own root
    // declares key_context("DbResultView"), never "Editor", and no cell is being
    // edited (no focused Editor entity exists) in this scenario, so this test
    // proves the raw handler correctly wins when nothing else is focused.
    #[gpui::test]
    fn real_keymap_editor_bindings_do_not_shadow_grid_shortcuts_when_no_editor_is_focused(
        cx: &mut gpui::TestAppContext,
    ) {
        // Mirrors the real Linux/Windows default keymap's "Editor"-context
        // bindings for the same keys (assets/keymaps/default-linux.json:
        // ctrl-d -> editor::SelectNext, ctrl-y -> editor::Redo) plus the two
        // most collision-prone letters (ctrl-v paste, ctrl-x cut), so every
        // raw "<letter> if primary" branch in this view's key handler gets the
        // same real-keymap proof ctrl-a/ctrl-z already had.
        cx.update(|cx| {
            cx.bind_keys([
                gpui::KeyBinding::new("ctrl-a", editor::actions::SelectAll, Some("Editor")),
                gpui::KeyBinding::new("ctrl-z", editor::actions::Undo, Some("Editor")),
                gpui::KeyBinding::new("ctrl-y", editor::actions::Redo, Some("Editor")),
                gpui::KeyBinding::new(
                    "ctrl-d",
                    editor::actions::SelectNext::default(),
                    Some("Editor"),
                ),
                gpui::KeyBinding::new("ctrl-v", editor::actions::Paste, Some("Editor")),
                gpui::KeyBinding::new("ctrl-x", editor::actions::Cut, Some("Editor")),
            ]);
        });

        let (window, view, mut cx) = table_backed_result_window(cx);
        let first_cell = debug_center(&mut cx, "CELL-0-0");
        cx.simulate_click(first_cell, gpui::Modifiers::none());
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, _cx| {
            assert!(
                view.cell_edit.is_none(),
                "selecting (not editing) a cell must not open the inline editor"
            );
        });

        cx.simulate_keystrokes("ctrl-a");
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, _| {
            assert_eq!(
                view.selected_cell_range,
                Some(((0, 0), (2, 1))),
                "a real ctrl-a keystroke must reach the grid's own select-all handler, not the \
                 competing editor::SelectAll keymap binding, when no cell editor is focused"
            );
        });

        view.update(&mut cx, |view, cx| {
            view.write_cell_value(0, 1, CellValue::Text("Zed".to_string()), cx);
        });
        view.update(&mut cx, |view, _| {
            assert_eq!(
                view.pending_cell_value(0, 1),
                Some(&CellValue::Text("Zed".to_string()))
            );
        });

        cx.simulate_keystrokes("ctrl-z");
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, _| {
            assert_eq!(
                view.pending_cell_value(0, 1),
                None,
                "a real ctrl-z keystroke must reach the grid's own undo handler, not the \
                 competing editor::Undo keymap binding, when no cell editor is focused"
            );
        });

        cx.simulate_keystrokes("ctrl-y");
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, _| {
            assert_eq!(
                view.pending_cell_value(0, 1),
                Some(&CellValue::Text("Zed".to_string())),
                "a real ctrl-y keystroke must reach the grid's own redo handler, not the \
                 competing editor::Redo keymap binding, when no cell editor is focused"
            );
        });

        let top_cell = debug_center(&mut cx, "CELL-0-1");
        let bottom_cell = debug_center(&mut cx, "CELL-2-1");
        cx.simulate_click(top_cell, gpui::Modifiers::none());
        cx.simulate_click(bottom_cell, gpui::Modifiers::shift());
        draw_result_view(window, &mut cx);

        cx.simulate_keystrokes("ctrl-d");
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, _| {
            assert_eq!(
                view.pending_cell_value(1, 1),
                Some(&CellValue::Text("Zed".to_string())),
                "a real ctrl-d keystroke must reach the grid's own fill-down handler, not the \
                 competing editor::SelectNext keymap binding, when no cell editor is focused"
            );
            assert_eq!(
                view.pending_cell_value(2, 1),
                Some(&CellValue::Text("Zed".to_string()))
            );
        });

        let left_cell = debug_center(&mut cx, "CELL-0-0");
        let right_cell = debug_center(&mut cx, "CELL-0-1");
        cx.simulate_click(left_cell, gpui::Modifiers::none());
        cx.simulate_click(right_cell, gpui::Modifiers::shift());
        draw_result_view(window, &mut cx);

        cx.simulate_keystrokes("ctrl-r");
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, _| {
            assert_eq!(
                view.pending_cell_value(0, 1),
                Some(&CellValue::Text("1".to_string())),
                "a real ctrl-r keystroke must reach the grid's own fill-right handler when no \
                 cell editor is focused"
            );
        });

        // A plain single click also copies the clicked cell's own value to the
        // clipboard (see single_click_leaves_cell_value_and_data_untouched),
        // so the paste payload must be written *after* selecting the target
        // cell, not before, or the click silently clobbers it.
        let single_cell = debug_center(&mut cx, "CELL-0-0");
        cx.simulate_click(single_cell, gpui::Modifiers::none());
        draw_result_view(window, &mut cx);
        cx.update(|_, cx| cx.write_to_clipboard(ClipboardItem::new_string("Pasted".to_string())));
        cx.simulate_keystrokes("ctrl-v");
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, _| {
            assert_eq!(
                view.pending_cell_value(0, 0),
                Some(&CellValue::Text("Pasted".to_string())),
                "a real ctrl-v keystroke must reach the grid's own paste handler, not the \
                 competing editor::Paste keymap binding, when no cell editor is focused"
            );
        });

        cx.simulate_keystrokes("ctrl-x");
        draw_result_view(window, &mut cx);
        let cut = cx
            .read_from_clipboard()
            .and_then(|clipboard| clipboard.text())
            .expect(
                "a real ctrl-x keystroke must reach the grid's own cut handler and write to \
                     the clipboard, not the competing editor::Cut keymap binding",
            );
        assert!(cut.contains("Pasted"));
        view.update(&mut cx, |view, _| {
            assert_eq!(
                view.pending_cell_value(0, 0),
                Some(&CellValue::Text(String::new())),
                "cut must clear the cell after copying it"
            );
        });
    }

    // Investigates a report of a numeric cell showing a doubled-looking value
    // ("1010" for an underlying "10") "out of nowhere". The two structural
    // hypotheses -- (a) the read-mode label and the live editor both painting
    // at once, (b) a second numeric-alignment render path missing the
    // `flex_none` wrapping fix -- do not hold up against the source: the
    // read-mode/edit-mode branches are a mutually exclusive if/else-if/else
    // chain (only one AnyElement is ever built per cell per render), and both
    // numeric-alignment call sites (`recompute_layout`'s data-column labels
    // and the added-row labels) funnel through the same fixed
    // `render_typed_cell_body`. This test walks a "10"-valued numeric cell
    // through select -> edit -> type -> commit-via-click-elsewhere and
    // asserts the rendered value is never anything but "10" or "105" (the
    // deliberately-typed result), never a concatenation/doubling -- it passes
    // cleanly, so the symptom was not reproduced through this path.
    #[gpui::test]
    fn numeric_cell_never_renders_a_doubled_value_across_select_edit_commit(
        cx: &mut gpui::TestAppContext,
    ) {
        let (window, view, mut cx) = table_backed_result_window(cx);
        view.update(&mut cx, |view, _cx| {
            view.column_infos = Some(vec![
                ColumnInfo {
                    name: "id".to_string(),
                    data_type: "int".to_string(),
                    is_nullable: false,
                    column_key: None,
                    default_value: None,
                    extra: String::new(),
                },
                ColumnInfo {
                    name: "name".to_string(),
                    data_type: "varchar(255)".to_string(),
                    is_nullable: true,
                    column_key: None,
                    default_value: None,
                    extra: String::new(),
                },
            ]);
            if let Some(result) = view.result.as_mut() {
                result.rows[0][0] = Some("10".to_string());
            }
            view.recompute_layout();
        });

        let cell_center = debug_center(&mut cx, "CELL-0-0");
        cx.simulate_event(gpui::MouseDownEvent {
            position: cell_center,
            button: gpui::MouseButton::Left,
            modifiers: gpui::Modifiers::none(),
            click_count: 1,
            first_mouse: false,
        });
        cx.simulate_event(gpui::MouseUpEvent {
            position: cell_center,
            button: gpui::MouseButton::Left,
            modifiers: gpui::Modifiers::none(),
            click_count: 1,
        });
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, _cx| {
            assert_eq!(
                view.loaded_cell_value(0, 0).as_deref(),
                Some("10"),
                "a plain single click must show the value as-is, never doubled"
            );
        });

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
                .map(|edit| edit.editor.read(cx).text(cx));
            assert_eq!(
                text,
                Some("10".to_string()),
                "double-clicking must pre-fill the editor with the exact value, not doubled"
            );
        });

        cx.simulate_keystrokes("5");
        view.update(&mut cx, |view, cx| {
            let text = view
                .cell_edit
                .as_ref()
                .map(|edit| edit.editor.read(cx).text(cx));
            assert_eq!(
                text,
                Some("105".to_string()),
                "typing must append to the real value, never duplicate it"
            );
        });

        let other_cell_center = debug_center(&mut cx, "CELL-1-0");
        cx.simulate_event(gpui::MouseDownEvent {
            position: other_cell_center,
            button: gpui::MouseButton::Left,
            modifiers: gpui::Modifiers::none(),
            click_count: 1,
            first_mouse: false,
        });
        cx.simulate_event(gpui::MouseUpEvent {
            position: other_cell_center,
            button: gpui::MouseButton::Left,
            modifiers: gpui::Modifiers::none(),
            click_count: 1,
        });
        draw_result_view(window, &mut cx);
        view.update(&mut cx, |view, _cx| {
            assert_eq!(
                view.pending_cell_value(0, 0)
                    .map(|v| render_cell_value(v).0),
                Some("105".to_string()),
                "the committed value must be exactly what was typed, never doubled or concatenated \
                 with the original"
            );
        });
    }

    #[gpui::test]
    fn horizontal_mouse_wheel_scrolls_the_grid(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) = framed_plain_result_window(cx, wide_table_result());
        let position = debug_center(&mut cx, "CELL-0-0");

        let offset_before = view.update(&mut cx, |view, _cx| f32::from(view.h_scroll.offset().x));
        assert_eq!(offset_before, 0.0, "grid should start unscrolled");

        cx.simulate_event(gpui::ScrollWheelEvent {
            position,
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(-120.), px(0.))),
            modifiers: gpui::Modifiers::none(),
            touch_phase: gpui::TouchPhase::Moved,
        });
        draw_result_view_frame(window, &mut cx);

        view.update(&mut cx, |view, _cx| {
            let offset_after = f32::from(view.h_scroll.offset().x);
            assert!(
                offset_after < offset_before,
                "a horizontal mouse-wheel/trackpad gesture over the grid must move the \
                 horizontal scroll offset, not just the drag-thumb; offset_before={offset_before}, \
                 offset_after={offset_after}"
            );
        });
    }

    #[test]
    fn parse_date_prefix_extracts_calendar_date_and_rejects_malformed_input() {
        let format = time::macros::format_description!("[year]-[month]-[day]");
        assert_eq!(
            parse_date_prefix("2026-03-15"),
            Some(time::Date::parse("2026-03-15", &format).unwrap())
        );
        assert_eq!(
            parse_date_prefix("2026-03-15 08:30:00"),
            Some(time::Date::parse("2026-03-15", &format).unwrap()),
            "a datetime value's trailing time portion must not prevent parsing its date prefix"
        );
        assert_eq!(parse_date_prefix(""), None);
        assert_eq!(parse_date_prefix("not a date"), None);
        assert_eq!(
            parse_date_prefix("2026-13-40"),
            None,
            "an invalid calendar date must not parse"
        );
    }

    #[test]
    fn format_date_ymd_matches_the_grids_date_placeholder_format() {
        let date = time::Date::from_calendar_date(2026, time::Month::March, 5).unwrap();
        assert_eq!(format_date_ymd(date), "2026-03-05");
    }

    fn table_result_with_date_column(datetime: bool) -> QueryResult {
        let (first, second) = if datetime {
            ("2026-03-15 09:00:00", "2026-03-20 17:45:00")
        } else {
            ("2026-03-15", "2026-03-20")
        };
        QueryResult {
            columns: vec!["id".to_string(), "signup_date".to_string()],
            rows: vec![
                vec![Some("1".to_string()), Some(first.to_string())],
                vec![Some("2".to_string()), Some(second.to_string())],
            ],
            rows_affected: 0,
            execution_time_ms: 0,
            timing: None,
            raw_documents: None,
        }
    }

    fn set_date_column_infos(view: &mut ResultView, datetime: bool) {
        view.column_infos = Some(vec![
            ColumnInfo {
                name: "id".to_string(),
                data_type: "int".to_string(),
                is_nullable: false,
                column_key: None,
                default_value: None,
                extra: String::new(),
            },
            ColumnInfo {
                name: "signup_date".to_string(),
                data_type: if datetime {
                    "datetime".to_string()
                } else {
                    "date".to_string()
                },
                is_nullable: true,
                column_key: None,
                default_value: None,
                extra: String::new(),
            },
        ]);
    }

    fn double_click_cell(cx: &mut gpui::VisualTestContext, selector: &'static str) {
        let center = debug_center(cx, selector);
        cx.simulate_event(gpui::MouseDownEvent {
            position: center,
            button: gpui::MouseButton::Left,
            modifiers: gpui::Modifiers::none(),
            click_count: 2,
            first_mouse: false,
        });
        cx.simulate_event(gpui::MouseUpEvent {
            position: center,
            button: gpui::MouseButton::Left,
            modifiers: gpui::Modifiers::none(),
            click_count: 2,
        });
    }

    #[gpui::test]
    fn date_cell_editor_opens_calendar_popup_and_day_click_writes_the_date(
        cx: &mut gpui::TestAppContext,
    ) {
        let (window, view, mut cx) =
            table_backed_result_window_with(cx, table_result_with_date_column(false));
        view.update(&mut cx, |view, _cx| set_date_column_infos(view, false));

        double_click_cell(&mut cx, "CELL-0-1");
        draw_result_view(window, &mut cx);

        assert!(
            cx.debug_bounds("DATE_POPUP").is_some(),
            "double-clicking a DATE cell must open the calendar popup"
        );
        assert!(
            cx.debug_bounds("DATE_POPUP_DAY-2026-03-15").is_some(),
            "the cell's own date must be represented in the rendered day grid"
        );

        let day_20 = debug_center(&mut cx, "DATE_POPUP_DAY-2026-03-20");
        cx.simulate_click(day_20, gpui::Modifiers::none());
        draw_result_view(window, &mut cx);

        view.update(&mut cx, |view, cx| {
            let text = view
                .cell_edit
                .as_ref()
                .map(|edit| edit.editor.read(cx).text(cx));
            assert_eq!(
                text,
                Some("2026-03-20".to_string()),
                "clicking a day in the calendar must write that date into the cell editor"
            );
            assert!(
                view.date_popup.is_none(),
                "picking a day on a DATE (non-datetime) column must close the popup"
            );
        });
    }

    #[gpui::test]
    fn date_popup_next_month_navigates_the_displayed_calendar(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) =
            table_backed_result_window_with(cx, table_result_with_date_column(false));
        view.update(&mut cx, |view, _cx| set_date_column_infos(view, false));

        double_click_cell(&mut cx, "CELL-0-1");
        draw_result_view(window, &mut cx);
        assert!(
            cx.debug_bounds("DATE_POPUP_HEADER-2026-03").is_some(),
            "the popup must open on the cell's own month"
        );

        let next_button = debug_center(&mut cx, "date-popup-next");
        cx.simulate_click(next_button, gpui::Modifiers::none());
        draw_result_view(window, &mut cx);

        assert!(
            cx.debug_bounds("DATE_POPUP_HEADER-2026-04").is_some(),
            "clicking next must advance the displayed calendar to the following month"
        );
        view.update(&mut cx, |view, cx| {
            let text = view
                .cell_edit
                .as_ref()
                .map(|edit| edit.editor.read(cx).text(cx));
            assert_eq!(
                text,
                Some("2026-03-15".to_string()),
                "browsing months must not touch the cell editor's text"
            );
        });
    }

    #[gpui::test]
    fn datetime_cell_day_click_preserves_the_existing_time_of_day(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) =
            table_backed_result_window_with(cx, table_result_with_date_column(true));
        view.update(&mut cx, |view, _cx| set_date_column_infos(view, true));

        double_click_cell(&mut cx, "CELL-0-1");
        draw_result_view(window, &mut cx);

        let day_20 = debug_center(&mut cx, "DATE_POPUP_DAY-2026-03-20");
        cx.simulate_click(day_20, gpui::Modifiers::none());
        draw_result_view(window, &mut cx);

        view.update(&mut cx, |view, cx| {
            let text = view
                .cell_edit
                .as_ref()
                .map(|edit| edit.editor.read(cx).text(cx));
            assert_eq!(
                text,
                Some("2026-03-20 09:00:00".to_string()),
                "picking a new day on a DATETIME column must keep the time already in the cell"
            );
        });
    }

    #[gpui::test]
    fn typing_a_date_manually_still_commits_via_the_text_fallback(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) =
            table_backed_result_window_with(cx, table_result_with_date_column(false));
        view.update(&mut cx, |view, _cx| set_date_column_infos(view, false));

        double_click_cell(&mut cx, "CELL-0-1");
        draw_result_view(window, &mut cx);
        assert!(
            cx.debug_bounds("DATE_POPUP").is_some(),
            "the calendar popup opens alongside the text editor, not instead of it"
        );

        for _ in 0.."2026-03-15".len() {
            cx.dispatch_action(editor::actions::Backspace);
        }
        cx.simulate_keystrokes("2 0 2 7 - 0 1 - 0 5");
        cx.simulate_keystrokes("enter");
        draw_result_view(window, &mut cx);

        view.update(&mut cx, |view, _cx| {
            assert_eq!(
                view.pending_cell_value(0, 1),
                Some(&CellValue::Text("2027-01-05".to_string())),
                "typing a date by hand, bypassing the calendar entirely, must still commit \
                 normally on Enter"
            );
            // Enter is Excel-style commit-and-move: it commits row 0 then opens
            // editing on row 1's own date cell, which correctly opens a fresh
            // popup for that cell rather than leaving the old one behind.
            assert_eq!(
                view.date_popup.as_ref().map(|popup| popup.abs_idx),
                Some(1),
                "commit-and-move must close the old cell's popup and open a new one for the cell \
                 it moved to, not leave the previous popup dangling"
            );
        });
    }
}
