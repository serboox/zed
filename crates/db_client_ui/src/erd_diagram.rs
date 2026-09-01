use gpui::{
    Context, CursorStyle, DismissEvent, EventEmitter, FocusHandle, Focusable, Hsla, Modifiers,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Point, ScrollDelta, ScrollHandle,
    ScrollWheelEvent, Window, actions, canvas, point, prelude::*, px,
};
use ui::{ScrollAxes, Scrollbars, Tooltip, WithScrollbar, cyberpunk, prelude::*, rems_from_px};
use workspace::{Item, item::ItemEvent};

actions!(
    erd_view,
    [
        /// Copies the diagram as a Mermaid erDiagram document.
        CopyMermaid,
        /// Copies the diagram as a Graphviz DOT document.
        CopyDot,
        /// Copies the diagram as a standalone SVG image document.
        CopySvg,
    ]
);

#[derive(Debug, Clone, PartialEq)]
pub struct ErdColumn {
    pub name: String,
    pub data_type: String,
    pub is_primary_key: bool,
    pub is_foreign_key: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ErdTable {
    pub name: String,
    pub columns: Vec<ErdColumn>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ErdRelationship {
    pub from_table: String,
    pub from_column: String,
    pub to_table: String,
    pub to_column: String,
}

const BOX_WIDTH: f32 = 220.0;
const HEADER_HEIGHT: f32 = 28.0;
const ROW_HEIGHT: f32 = 20.0;
const HORIZONTAL_GAP: f32 = 56.0;
const VERTICAL_GAP: f32 = 44.0;
const OUTER_PADDING: f32 = 24.0;
const DEFAULT_COLUMNS_PER_ROW: usize = 3;
// Cap the rows drawn per table so every box fits one uniform grid slot; this
// keeps box positions (and therefore the relationship lines) deterministic.
const MAX_DISPLAY_COLUMNS: usize = 10;

const MIN_ZOOM: f32 = 0.25;
const MAX_ZOOM: f32 = 3.0;
const ZOOM_STEP: f32 = 0.25;
// Line-based wheel deltas carry small magnitudes; scale them up to feel like pixel scrolls.
const SCROLL_LINE_MULTIPLIER: f32 = 20.0;
const WHEEL_ZOOM_SENSITIVITY: f32 = 0.005;
// Fit-to-viewport only ever zooms OUT to frame a large graph; it never enlarges
// a small schema past 1.0 into oversized boxes.
const MAX_FIT_ZOOM: f32 = 1.0;
// Bounded retries for the one-shot on-open fit while the scroll container has
// not been measured yet, so a zero-size viewport can never animate forever.
const MAX_FIT_ATTEMPTS: u8 = 30;

// Base (zoom = 1.0) pixel sizes for everything inside a box. Zoom multiplies
// each of these by the same factor so the diagram scales like an image: text,
// icons, padding, gaps and line strokes all grow and shrink together with the
// box geometry, instead of the box growing while fixed-size text drifts.
const HEADER_FONT: f32 = 13.0;
const COLUMN_FONT: f32 = 11.0;
const CELL_PADDING: f32 = 8.0;
const ROW_GAP: f32 = 4.0;
const ICON_PX: f32 = 12.0;
const LINE_STROKE: f32 = 1.5;
const ARROW_SIZE: f32 = 8.0;
// Perpendicular half-spread of a crow's-foot toe / "one" bar, as a fraction of
// the marker size. 0.75 is exactly representable in f32, so marker geometry is
// bit-stable and unit-testable with exact equality.
const MARKER_SPREAD: f32 = 0.75;
// Graph-paper dot grid painted behind the diagram.
const GRID_SPACING: f32 = 24.0;
const GRID_DOT: f32 = 1.5;
// Upper bound on dots painted per frame; past this the grid is skipped so a huge
// zoomed-out canvas can never stall a frame drawing millions of quads.
const MAX_GRID_DOTS: i64 = 20_000;

fn clamp_zoom(zoom: f32) -> f32 {
    zoom.clamp(MIN_ZOOM, MAX_ZOOM)
}

/// Multiply a base pixel value by the zoom factor. Every dimension in the
/// diagram goes through this so scaling stays uniform.
fn scaled(value: f32, zoom: f32) -> f32 {
    value * zoom
}

/// Scale a box's position and size by a single zoom factor. Position and size
/// use the same multiplier so a box at (x, y, w, h) maps to (x*z, y*z, w*z, h*z)
/// and never drifts relative to its neighbours.
pub fn scaled_box(x: f32, y: f32, width: f32, height: f32, zoom: f32) -> (f32, f32, f32, f32) {
    (
        scaled(x, zoom),
        scaled(y, zoom),
        scaled(width, zoom),
        scaled(height, zoom),
    )
}

/// Grid coordinate (row, column) for each table index, wrapping after
/// `columns_per_row`. Pure so the layout can be unit-tested without a window.
pub fn layout_positions(table_count: usize, columns_per_row: usize) -> Vec<(usize, usize)> {
    let columns_per_row = columns_per_row.max(1);
    (0..table_count)
        .map(|index| (index / columns_per_row, index % columns_per_row))
        .collect()
}

fn displayed_column_count(column_count: usize) -> usize {
    column_count.min(MAX_DISPLAY_COLUMNS)
}

/// Pixel height of a table box given its column count (capped for layout).
pub fn box_height(column_count: usize) -> f32 {
    let visible = displayed_column_count(column_count);
    let overflow_row = if column_count > MAX_DISPLAY_COLUMNS {
        ROW_HEIGHT
    } else {
        0.0
    };
    HEADER_HEIGHT + visible as f32 * ROW_HEIGHT + overflow_row
}

fn slot_height() -> f32 {
    HEADER_HEIGHT + MAX_DISPLAY_COLUMNS as f32 * ROW_HEIGHT + ROW_HEIGHT + VERTICAL_GAP
}

/// Top-left pixel origin of a box at the given grid coordinate.
pub fn box_origin(row: usize, column: usize) -> (f32, f32) {
    let x = OUTER_PADDING + column as f32 * (BOX_WIDTH + HORIZONTAL_GAP);
    let y = OUTER_PADDING + row as f32 * slot_height();
    (x, y)
}

/// Point on a box's perimeter (one of its four edge midpoints) that faces
/// `toward`, so a relationship line anchors to the box's border instead of
/// passing through its center and the table contents drawn inside it.
pub fn edge_anchor(box_rect: (f32, f32, f32, f32), toward: (f32, f32)) -> (f32, f32) {
    let (x, y, w, h) = box_rect;
    let center = (x + w / 2.0, y + h / 2.0);
    let dx = toward.0 - center.0;
    let dy = toward.1 - center.1;
    if dx.abs() >= dy.abs() {
        if dx >= 0.0 {
            (x + w, center.1)
        } else {
            (x, center.1)
        }
    } else if dy >= 0.0 {
        (center.0, y + h)
    } else {
        (center.0, y)
    }
}

/// Right-angle ("elbow") waypoints connecting two boxes' facing edges instead
/// of a straight center-to-center diagonal that would cut through both boxes'
/// contents. Bends at the horizontal midpoint between the two edge anchors;
/// this collapses to a single straight segment when the anchors already
/// share an axis (e.g. two boxes in the same grid row), which is the correct
/// degenerate case, not a bug.
pub fn elbow_route(
    from_box: (f32, f32, f32, f32),
    to_box: (f32, f32, f32, f32),
) -> Vec<(f32, f32)> {
    let from_center = (from_box.0 + from_box.2 / 2.0, from_box.1 + from_box.3 / 2.0);
    let to_center = (to_box.0 + to_box.2 / 2.0, to_box.1 + to_box.3 / 2.0);
    let anchor_from = edge_anchor(from_box, to_center);
    let anchor_to = edge_anchor(to_box, from_center);
    let mid_x = (anchor_from.0 + anchor_to.0) / 2.0;
    vec![
        anchor_from,
        (mid_x, anchor_from.1),
        (mid_x, anchor_to.1),
        anchor_to,
    ]
}

/// Crow's-foot ("many") marker for a relationship's foreign-key end. `to` is the
/// point on the entity edge where the three toes spread; the apex sits `size`
/// back along the line toward `from`. Returns `[apex, toe_left, toe_center,
/// toe_right]` -- painting a segment from `apex` to each toe draws the foot.
/// A zero-length segment degenerates to every point at `to` rather than dividing
/// by zero.
pub fn crows_foot(from: (f32, f32), to: (f32, f32), size: f32) -> [(f32, f32); 4] {
    let dx = to.0 - from.0;
    let dy = to.1 - from.1;
    let len = (dx * dx + dy * dy).sqrt();
    if len < f32::EPSILON {
        return [to, to, to, to];
    }
    let ux = dx / len;
    let uy = dy / len;
    let perp_x = -uy;
    let perp_y = ux;
    let apex = (to.0 - ux * size, to.1 - uy * size);
    let half = size * MARKER_SPREAD;
    let toe_left = (to.0 + perp_x * half, to.1 + perp_y * half);
    let toe_right = (to.0 - perp_x * half, to.1 - perp_y * half);
    [apex, toe_left, to, toe_right]
}

/// "One" bar marker for a relationship's referenced end: a short tick drawn
/// perpendicular to the line, `size` back from the entity edge `to`. Returns the
/// bar's two endpoints. Degenerates to `[to, to]` for a zero-length segment.
pub fn one_bar(from: (f32, f32), to: (f32, f32), size: f32) -> [(f32, f32); 2] {
    let dx = to.0 - from.0;
    let dy = to.1 - from.1;
    let len = (dx * dx + dy * dy).sqrt();
    if len < f32::EPSILON {
        return [to, to];
    }
    let ux = dx / len;
    let uy = dy / len;
    let perp_x = -uy;
    let perp_y = ux;
    let center = (to.0 - ux * size, to.1 - uy * size);
    let half = size * MARKER_SPREAD;
    [
        (center.0 + perp_x * half, center.1 + perp_y * half),
        (center.0 - perp_x * half, center.1 - perp_y * half),
    ]
}

/// Orient an end marker: return `(neighbor, anchor)` for one end of a route,
/// where `anchor` is the endpoint the marker sits on and `neighbor` is the
/// nearest waypoint distinct from it (scanning inward). Returns `None` only when
/// every waypoint coincides (a fully degenerate route). A straight vertical or
/// horizontal route collapses its elbow onto the anchor, making the immediate
/// neighbor a duplicate; skipping to the first distinct point keeps the marker
/// correctly pointed instead of degenerating to an invisible zero-length mark.
fn marker_direction(points: &[(f32, f32)], from_start: bool) -> Option<((f32, f32), (f32, f32))> {
    if from_start {
        let anchor = *points.first()?;
        let neighbor = points.iter().copied().find(|point| *point != anchor)?;
        Some((neighbor, anchor))
    } else {
        let anchor = *points.last()?;
        let neighbor = points
            .iter()
            .rev()
            .copied()
            .find(|point| *point != anchor)?;
        Some((neighbor, anchor))
    }
}

/// Split a possibly schema-qualified table name into `(schema, table)`.
/// `"public.users"` -> `(Some("public"), "users")`; `"users"` -> `(None, "users")`.
/// A trailing or leading dot leaves the whole string as the table name.
fn split_qualified_name(name: &str) -> (Option<&str>, &str) {
    match name.rsplit_once('.') {
        Some((schema, table)) if !schema.is_empty() && !table.is_empty() => (Some(schema), table),
        _ => (None, name),
    }
}

fn escape_mermaid_type(data_type: &str) -> String {
    // Mermaid attribute types must be a single token; collapse whitespace and
    // strip parentheses that would otherwise break the erDiagram grammar.
    data_type
        .chars()
        .map(|character| match character {
            ' ' | '(' | ')' | ',' => '_',
            other => other,
        })
        .collect()
}

/// Render the schema as a Mermaid `erDiagram` document.
pub fn to_mermaid(tables: &[ErdTable], relationships: &[ErdRelationship]) -> String {
    let mut out = String::from("erDiagram\n");
    for table in tables {
        out.push_str(&format!("    {} {{\n", table.name));
        for column in &table.columns {
            let key = if column.is_primary_key {
                " PK"
            } else if column.is_foreign_key {
                " FK"
            } else {
                ""
            };
            out.push_str(&format!(
                "        {} {}{}\n",
                escape_mermaid_type(&column.data_type),
                column.name,
                key
            ));
        }
        out.push_str("    }\n");
    }
    for relationship in relationships {
        out.push_str(&format!(
            "    {} ||--o{{ {} : \"{}\"\n",
            relationship.to_table, relationship.from_table, relationship.from_column
        ));
    }
    out
}

fn escape_dot(value: &str) -> String {
    value.replace('"', "\\\"")
}

/// Render the schema as a Graphviz DOT document.
pub fn to_dot(tables: &[ErdTable], relationships: &[ErdRelationship]) -> String {
    let mut out = String::from("digraph erd {\n    rankdir=LR;\n    node [shape=record];\n");
    for table in tables {
        let columns = table
            .columns
            .iter()
            .map(|column| {
                let marker = if column.is_primary_key {
                    "PK "
                } else if column.is_foreign_key {
                    "FK "
                } else {
                    ""
                };
                format!("{}{} : {}", marker, column.name, column.data_type)
            })
            .collect::<Vec<_>>()
            .join("\\l");
        out.push_str(&format!(
            "    \"{}\" [label=\"{}|{}\\l\"];\n",
            escape_dot(&table.name),
            escape_dot(&table.name),
            escape_dot(&columns)
        ));
    }
    for relationship in relationships {
        out.push_str(&format!(
            "    \"{}\" -> \"{}\" [label=\"{}\"];\n",
            escape_dot(&relationship.from_table),
            escape_dot(&relationship.to_table),
            escape_dot(&relationship.from_column)
        ));
    }
    out.push_str("}\n");
    out
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// SVG is a standalone document viewed outside the running app (an exported
// file, not app chrome), so it uses fixed colors rather than `cx.theme()`
// tokens -- there is no theme to read at export time and the file must still
// render correctly when opened later, in another viewer, on another machine.
const SVG_BORDER: &str = "#666666";
const SVG_HEADER_FILL: &str = "#e0e0e0";
const SVG_BODY_FILL: &str = "#ffffff";
const SVG_TEXT: &str = "#1a1a1a";
const SVG_MUTED_TEXT: &str = "#666666";
const SVG_LINE: &str = "#888888";
const SVG_PK: &str = "#b8860b";
const SVG_FK: &str = "#4169aa";

/// Render the schema as a standalone SVG document: one rect+text block per
/// table (mirroring `render_table_box`'s layout) and one polyline+arrowhead
/// per relationship (mirroring the on-screen elbow routing), all at zoom 1.0
/// so the exported file matches the diagram's un-zoomed proportions.
pub fn to_svg(tables: &[ErdTable], relationships: &[ErdRelationship]) -> String {
    let positions = layout_positions(tables.len(), DEFAULT_COLUMNS_PER_ROW);
    let mut boxes: Vec<(String, f32, f32, f32, f32)> = Vec::with_capacity(tables.len());
    let mut total_width: f32 = 1.0;
    let mut total_height: f32 = 1.0;
    for (table, (row, column)) in tables.iter().zip(&positions) {
        let (origin_x, origin_y) = box_origin(*row, *column);
        let height = box_height(table.columns.len());
        total_width = total_width.max(origin_x + BOX_WIDTH + OUTER_PADDING);
        total_height = total_height.max(origin_y + height + OUTER_PADDING);
        boxes.push((table.name.clone(), origin_x, origin_y, BOX_WIDTH, height));
    }
    let find_box = |name: &str| {
        boxes
            .iter()
            .find(|(n, ..)| n == name)
            .map(|(_, x, y, w, h)| (*x, *y, *w, *h))
    };

    let mut out = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{total_width}\" height=\"{total_height}\" \
         viewBox=\"0 0 {total_width} {total_height}\" font-family=\"sans-serif\">\n\
         <rect x=\"0\" y=\"0\" width=\"{total_width}\" height=\"{total_height}\" fill=\"{SVG_BODY_FILL}\"/>\n"
    );

    for relationship in relationships {
        let (Some(from_box), Some(to_box)) = (
            find_box(&relationship.from_table),
            find_box(&relationship.to_table),
        ) else {
            continue;
        };
        let waypoints = elbow_route(from_box, to_box);
        let points = waypoints
            .iter()
            .map(|(x, y)| format!("{x},{y}"))
            .collect::<Vec<_>>()
            .join(" ");
        out.push_str(&format!(
            "<polyline points=\"{points}\" fill=\"none\" stroke=\"{SVG_LINE}\" stroke-width=\"{LINE_STROKE}\"/>\n"
        ));
        // Crow's-foot ("many") at the FK end (route start); "one" bar at the
        // referenced end (route end). Orient from the nearest distinct waypoint
        // so a straight vertical/horizontal route still gets visible markers.
        if let Some((many_from, many_to)) = marker_direction(&waypoints, true) {
            let foot = crows_foot(many_from, many_to, ARROW_SIZE);
            for toe in &foot[1..] {
                out.push_str(&format!(
                    "<line x1=\"{}\" y1=\"{}\" x2=\"{}\" y2=\"{}\" stroke=\"{SVG_LINE}\" stroke-width=\"{LINE_STROKE}\"/>\n",
                    foot[0].0, foot[0].1, toe.0, toe.1
                ));
            }
        }
        if let Some((one_from, one_to)) = marker_direction(&waypoints, false) {
            let bar = one_bar(one_from, one_to, ARROW_SIZE);
            out.push_str(&format!(
                "<line x1=\"{}\" y1=\"{}\" x2=\"{}\" y2=\"{}\" stroke=\"{SVG_LINE}\" stroke-width=\"{LINE_STROKE}\"/>\n",
                bar[0].0, bar[0].1, bar[1].0, bar[1].1
            ));
        }
    }

    for table in tables {
        let Some((x, y, w, h)) = find_box(&table.name) else {
            continue;
        };
        out.push_str(&format!(
            "<rect x=\"{x}\" y=\"{y}\" width=\"{w}\" height=\"{h}\" fill=\"{SVG_BODY_FILL}\" \
             stroke=\"{SVG_BORDER}\" stroke-width=\"1\" rx=\"4\"/>\n"
        ));
        out.push_str(&format!(
            "<rect x=\"{x}\" y=\"{y}\" width=\"{w}\" height=\"{HEADER_HEIGHT}\" fill=\"{SVG_HEADER_FILL}\" \
             stroke=\"{SVG_BORDER}\" stroke-width=\"1\"/>\n"
        ));
        out.push_str(&format!(
            "<text x=\"{}\" y=\"{}\" font-size=\"{HEADER_FONT}\" font-weight=\"bold\" fill=\"{SVG_TEXT}\">{}</text>\n",
            x + CELL_PADDING,
            y + HEADER_HEIGHT / 2.0 + HEADER_FONT / 3.0,
            escape_xml(&table.name)
        ));
        let visible = displayed_column_count(table.columns.len());
        for (index, column) in table.columns.iter().take(visible).enumerate() {
            let row_y = y + HEADER_HEIGHT + index as f32 * ROW_HEIGHT;
            let text_y = row_y + ROW_HEIGHT / 2.0 + COLUMN_FONT / 3.0;
            let (marker, marker_color) = if column.is_primary_key {
                ("PK ", SVG_PK)
            } else if column.is_foreign_key {
                ("FK ", SVG_FK)
            } else {
                ("", SVG_MUTED_TEXT)
            };
            out.push_str(&format!(
                "<text x=\"{}\" y=\"{text_y}\" font-size=\"{COLUMN_FONT}\" fill=\"{marker_color}\">{}</text>\n",
                x + CELL_PADDING,
                escape_xml(marker)
            ));
            out.push_str(&format!(
                "<text x=\"{}\" y=\"{text_y}\" font-size=\"{COLUMN_FONT}\" fill=\"{SVG_TEXT}\">{}</text>\n",
                x + CELL_PADDING + 16.0,
                escape_xml(&column.name)
            ));
            out.push_str(&format!(
                "<text x=\"{}\" y=\"{text_y}\" font-size=\"{COLUMN_FONT}\" fill=\"{SVG_MUTED_TEXT}\" text-anchor=\"end\">{}</text>\n",
                x + w - CELL_PADDING,
                escape_xml(&column.data_type)
            ));
        }
        let hidden = table.columns.len().saturating_sub(visible);
        if hidden > 0 {
            let row_y = y + HEADER_HEIGHT + visible as f32 * ROW_HEIGHT;
            out.push_str(&format!(
                "<text x=\"{}\" y=\"{}\" font-size=\"{COLUMN_FONT}\" fill=\"{SVG_MUTED_TEXT}\">+{hidden} more</text>\n",
                x + CELL_PADDING,
                row_y + ROW_HEIGHT / 2.0 + COLUMN_FONT / 3.0
            ));
        }
    }

    out.push_str("</svg>\n");
    out
}

struct PlacedTable {
    table: ErdTable,
    origin_x: f32,
    origin_y: f32,
}

/// Unscaled (zoom = 1.0) paint geometry for one relationship: the elbow-routed
/// connector plus its crow's-foot ("many", FK side) and bar ("one", referenced
/// side) end markers.
struct RelationshipGeometry {
    waypoints: Vec<(f32, f32)>,
    crows_foot: [(f32, f32); 4],
    one_bar: [(f32, f32); 2],
}

pub struct ErdView {
    focus_handle: FocusHandle,
    title: SharedString,
    placed: Vec<PlacedTable>,
    relationships: Vec<ErdRelationship>,
    mermaid: String,
    dot: String,
    svg: String,
    total_width: f32,
    total_height: f32,
    scroll_handle: ScrollHandle,
    zoom: f32,
    // (mouse position, scroll offset) captured at the start of a click-drag pan;
    // `None` means the canvas is idle (not currently being dragged).
    pan_origin: Option<(Point<gpui::Pixels>, Point<gpui::Pixels>)>,
    // One-shot: frame the whole graph on first render, once the scroll container
    // has been measured. Cleared by the fit itself or by any manual zoom/pan so
    // user interaction is never overridden by a late auto-fit.
    pending_fit: bool,
    fit_attempts: u8,
}

impl ErdView {
    pub fn new(
        tables: Vec<ErdTable>,
        relationships: Vec<ErdRelationship>,
        title: impl Into<SharedString>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mermaid = to_mermaid(&tables, &relationships);
        let dot = to_dot(&tables, &relationships);
        let svg = to_svg(&tables, &relationships);
        let positions = layout_positions(tables.len(), DEFAULT_COLUMNS_PER_ROW);
        let mut placed = Vec::with_capacity(tables.len());
        let mut total_width: f32 = 0.0;
        let mut total_height: f32 = 0.0;
        for (table, (row, column)) in tables.into_iter().zip(positions) {
            let (origin_x, origin_y) = box_origin(row, column);
            total_width = total_width.max(origin_x + BOX_WIDTH + OUTER_PADDING);
            total_height =
                total_height.max(origin_y + box_height(table.columns.len()) + OUTER_PADDING);
            placed.push(PlacedTable {
                table,
                origin_x,
                origin_y,
            });
        }
        Self {
            focus_handle: cx.focus_handle(),
            title: title.into(),
            placed,
            relationships,
            mermaid,
            dot,
            svg,
            total_width: total_width.max(1.0),
            total_height: total_height.max(1.0),
            scroll_handle: ScrollHandle::new(),
            zoom: 1.0,
            pan_origin: None,
            pending_fit: true,
            fit_attempts: 0,
        }
    }

    fn copy_mermaid(&self, cx: &mut Context<Self>) {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(self.mermaid.clone()));
    }

    fn copy_dot(&self, cx: &mut Context<Self>) {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(self.dot.clone()));
    }

    fn copy_svg(&self, cx: &mut Context<Self>) {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(self.svg.clone()));
    }

    fn zoom_in(&mut self, cx: &mut Context<Self>) {
        self.pending_fit = false;
        self.zoom = clamp_zoom(self.zoom + ZOOM_STEP);
        cx.notify();
    }

    fn zoom_out(&mut self, cx: &mut Context<Self>) {
        self.pending_fit = false;
        self.zoom = clamp_zoom(self.zoom - ZOOM_STEP);
        cx.notify();
    }

    fn reset_zoom(&mut self, cx: &mut Context<Self>) {
        self.pending_fit = false;
        self.zoom = 1.0;
        cx.notify();
    }

    /// Zoom and scroll so the whole graph is framed within `viewport`. Only ever
    /// zooms out (capped at `MAX_FIT_ZOOM`) and resets the scroll to the origin so
    /// the framed graph starts at the top-left. A degenerate viewport is a no-op.
    fn apply_fit(&mut self, viewport: gpui::Size<gpui::Pixels>) {
        let viewport_width: f32 = viewport.width.into();
        let viewport_height: f32 = viewport.height.into();
        if viewport_width <= 0.0 || viewport_height <= 0.0 {
            return;
        }
        let fit = (viewport_width / self.total_width)
            .min(viewport_height / self.total_height)
            .min(MAX_FIT_ZOOM);
        self.zoom = clamp_zoom(fit);
        self.scroll_handle.set_offset(point(px(0.0), px(0.0)));
    }

    fn zoom_to_fit(&mut self, cx: &mut Context<Self>) {
        let viewport = self.scroll_handle.bounds().size;
        if viewport.width > px(0.0) && viewport.height > px(0.0) {
            self.apply_fit(viewport);
            self.pending_fit = false;
        } else {
            // The container has not been measured yet; defer to the on-render
            // fit path, which retries once bounds are known.
            self.pending_fit = true;
            self.fit_attempts = 0;
        }
        cx.notify();
    }

    fn begin_pan(&mut self, mouse_position: Point<gpui::Pixels>, cx: &mut Context<Self>) {
        self.pending_fit = false;
        self.pan_origin = Some((mouse_position, self.scroll_handle.offset()));
        cx.notify();
    }

    // Moves the content by exactly the mouse's own travel since `begin_pan`, so
    // the point under the cursor stays pinned to the cursor (a real "grab").
    fn update_pan(&mut self, mouse_position: Point<gpui::Pixels>, cx: &mut Context<Self>) {
        let Some((start_mouse, start_offset)) = self.pan_origin else {
            return;
        };
        let delta = mouse_position - start_mouse;
        self.scroll_handle.set_offset(start_offset + delta);
        cx.notify();
    }

    fn end_pan(&mut self, cx: &mut Context<Self>) {
        if self.pan_origin.take().is_some() {
            cx.notify();
        }
    }

    // Returns true when the wheel event was consumed as a zoom, so the caller can
    // suppress the container's own scroll. Zoom only applies with Ctrl (Linux/Windows)
    // or Cmd (macOS) held; a plain wheel falls through to normal scrolling.
    fn apply_wheel_zoom(&mut self, delta_y: f32, modifiers: &Modifiers) -> bool {
        if !(modifiers.control || modifiers.platform) || delta_y == 0.0 {
            return false;
        }
        let factor = if delta_y > 0.0 {
            1.0 + delta_y.abs() * WHEEL_ZOOM_SENSITIVITY
        } else {
            1.0 / (1.0 + delta_y.abs() * WHEEL_ZOOM_SENSITIVITY)
        };
        self.pending_fit = false;
        self.zoom = clamp_zoom(self.zoom * factor);
        true
    }

    /// Unscaled (zoom = 1.0) bounding rect (x, y, width, height) of a placed
    /// table box, for edge-anchored relationship routing.
    fn table_box(&self, table_name: &str) -> Option<(f32, f32, f32, f32)> {
        self.placed.iter().find_map(|placed| {
            if placed.table.name == table_name {
                Some((
                    placed.origin_x,
                    placed.origin_y,
                    BOX_WIDTH,
                    box_height(placed.table.columns.len()),
                ))
            } else {
                None
            }
        })
    }

    /// Unscaled elbow-routed waypoints plus crow's-foot/bar end markers for every
    /// relationship whose both endpoints are placed tables. Kept as its own
    /// method (rather than inlined in `render`) so it is directly callable
    /// and assertable from tests without a full paint pass.
    fn relationship_paths(&self) -> Vec<RelationshipGeometry> {
        self.relationships
            .iter()
            .filter_map(|relationship| {
                let from_box = self.table_box(&relationship.from_table)?;
                let to_box = self.table_box(&relationship.to_table)?;
                let waypoints = elbow_route(from_box, to_box);
                // Route runs FK side (start, "many") -> referenced side (end, "one").
                let (many_from, many_to) = marker_direction(&waypoints, true)?;
                let (one_from, one_to) = marker_direction(&waypoints, false)?;
                let crows_foot = crows_foot(many_from, many_to, ARROW_SIZE);
                let one_bar = one_bar(one_from, one_to, ARROW_SIZE);
                Some(RelationshipGeometry {
                    waypoints,
                    crows_foot,
                    one_bar,
                })
            })
            .collect()
    }

    fn render_table_box(
        &self,
        placed: &PlacedTable,
        zoom: f32,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let colors = cx.theme().colors();
        let visible = displayed_column_count(placed.table.columns.len());
        let hidden = placed.table.columns.len().saturating_sub(visible);
        let cell_padding = px(scaled(CELL_PADDING, zoom));
        let row_gap = px(scaled(ROW_GAP, zoom));
        let header_font = px(scaled(HEADER_FONT, zoom));
        let column_font = px(scaled(COLUMN_FONT, zoom));
        let icon_size = IconSize::Custom(rems_from_px(scaled(ICON_PX, zoom)));
        // A fixed-width leading gutter on every row (header, columns, overflow)
        // so key rows (icon) and non-key rows (no icon) start their text at the
        // same x, and names line up under the table title.
        let gutter_width = px(scaled(ICON_PX, zoom));
        // Accent tint gives the title strip clear separation from the body:
        // element_background vs elevated_surface_background are nearly identical.
        let header_bg = colors.text_accent.opacity(0.12);
        let (schema, table_name) = split_qualified_name(&placed.table.name);
        let (box_x, box_y, box_w, _box_h) = scaled_box(
            placed.origin_x,
            placed.origin_y,
            BOX_WIDTH,
            box_height(placed.table.columns.len()),
            zoom,
        );
        let mut box_element = v_flex()
            .absolute()
            .left(px(box_x))
            .top(px(box_y))
            .w(px(box_w))
            .border_1()
            .border_color(colors.border)
            .rounded_md()
            .bg(colors.elevated_surface_background)
            .overflow_hidden()
            .child(
                h_flex()
                    .h(px(scaled(HEADER_HEIGHT, zoom)))
                    .w_full()
                    .px(cell_padding)
                    .gap(row_gap)
                    .items_center()
                    .bg(header_bg)
                    .border_b_1()
                    .border_color(colors.border)
                    .child(
                        h_flex().w(gutter_width).flex_none().items_center().child(
                            Icon::new(IconName::ListTree)
                                .size(icon_size)
                                .color(Color::Custom(colors.text_accent)),
                        ),
                    )
                    .when_some(schema, |header, schema| {
                        header.child(
                            div()
                                .text_size(column_font)
                                .text_color(colors.text_muted)
                                .truncate()
                                .child(format!("{schema}.")),
                        )
                    })
                    .child(
                        div()
                            .text_size(header_font)
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(colors.text)
                            .truncate()
                            .child(table_name.to_string()),
                    ),
            );
        for column in placed.table.columns.iter().take(visible) {
            let key_color = if column.is_primary_key {
                Some(cx.theme().status().warning)
            } else if column.is_foreign_key {
                Some(cx.theme().status().info)
            } else {
                None
            };
            let mut gutter = h_flex().w(gutter_width).flex_none().items_center();
            if let Some(color) = key_color {
                gutter = gutter.child(
                    Icon::new(if column.is_primary_key {
                        IconName::StarFilled
                    } else {
                        IconName::Link
                    })
                    .size(icon_size)
                    .color(Color::Custom(color)),
                );
            }
            let column_name = column.name.clone();
            box_element = box_element.child(
                h_flex()
                    .h(px(scaled(ROW_HEIGHT, zoom)))
                    .w_full()
                    .px(cell_padding)
                    .gap(row_gap)
                    .items_center()
                    .child(gutter)
                    .child(
                        div()
                            .debug_selector({
                                let name = column_name.clone();
                                move || format!("ERD_COL_NAME::{name}")
                            })
                            .text_size(column_font)
                            .text_color(colors.text)
                            .truncate()
                            .child(column_name),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_size(column_font)
                            .text_color(colors.text_muted)
                            .truncate()
                            .child(column.data_type.clone()),
                    ),
            );
        }
        if hidden > 0 {
            box_element = box_element.child(
                h_flex()
                    .h(px(scaled(ROW_HEIGHT, zoom)))
                    .w_full()
                    .px(cell_padding)
                    .gap(row_gap)
                    .items_center()
                    .child(h_flex().w(gutter_width).flex_none())
                    .child(
                        div()
                            .text_size(column_font)
                            .text_color(colors.text_muted)
                            .child(format!("+{hidden} more")),
                    ),
            );
        }
        box_element
    }
}

impl Focusable for ErdView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for ErdView {}

impl Item for ErdView {
    type Event = DismissEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.title.clone()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::ListTree))
    }

    fn to_item_events(_event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(ItemEvent::CloseItem);
    }
}

impl Render for ErdView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // One-shot: frame the whole graph on open. The scroll container's size is
        // only known after it has been painted once, so on the first frame we ask
        // for another frame and fit once bounds are available.
        if self.pending_fit {
            let viewport = self.scroll_handle.bounds().size;
            if viewport.width > px(0.0) && viewport.height > px(0.0) {
                self.apply_fit(viewport);
                self.pending_fit = false;
            } else if self.fit_attempts < MAX_FIT_ATTEMPTS {
                self.fit_attempts += 1;
                window.request_animation_frame();
            } else {
                self.pending_fit = false;
            }
        }

        let colors = cx.theme().colors();
        let line_color: Hsla = colors.border_variant;
        let dot_color: Hsla = colors.border_variant.opacity(0.55);
        let zoom = self.zoom;

        let mut routes: Vec<Vec<gpui::Point<gpui::Pixels>>> = Vec::new();
        let mut crows_feet: Vec<[gpui::Point<gpui::Pixels>; 4]> = Vec::new();
        let mut one_bars: Vec<[gpui::Point<gpui::Pixels>; 2]> = Vec::new();
        let scale_point = |(x, y): (f32, f32)| point(px(scaled(x, zoom)), px(scaled(y, zoom)));
        for geometry in self.relationship_paths() {
            routes.push(geometry.waypoints.iter().map(|p| scale_point(*p)).collect());
            crows_feet.push(geometry.crows_foot.map(scale_point));
            one_bars.push(geometry.one_bar.map(scale_point));
        }

        let stroke_width = px(scaled(LINE_STROKE, zoom));
        let grid_spacing = scaled(GRID_SPACING, zoom).max(4.0);
        let dot_size = px(scaled(GRID_DOT, zoom).max(1.0));

        let mut surface = div()
            .relative()
            .w(px(scaled(self.total_width, zoom)))
            .h(px(scaled(self.total_height, zoom)))
            .child(
                canvas(
                    move |_, _, _| {},
                    move |bounds, _, window, _| {
                        // Graph-paper dot grid behind everything else.
                        let width: f32 = bounds.size.width.into();
                        let height: f32 = bounds.size.height.into();
                        let columns = (width / grid_spacing).ceil().max(0.0) as i64;
                        let rows = (height / grid_spacing).ceil().max(0.0) as i64;
                        if (columns + 1).saturating_mul(rows + 1) <= MAX_GRID_DOTS {
                            for row in 0..=rows {
                                for column in 0..=columns {
                                    let origin = point(
                                        bounds.origin.x + px(column as f32 * grid_spacing),
                                        bounds.origin.y + px(row as f32 * grid_spacing),
                                    );
                                    window.paint_quad(gpui::fill(
                                        gpui::Bounds::new(origin, gpui::size(dot_size, dot_size)),
                                        dot_color,
                                    ));
                                }
                            }
                        }
                        for route in &routes {
                            let Some((first, rest)) = route.split_first() else {
                                continue;
                            };
                            let mut builder = gpui::PathBuilder::stroke(stroke_width);
                            builder.move_to(*first);
                            for point in rest {
                                builder.line_to(*point);
                            }
                            if let Ok(path) = builder.build() {
                                window.paint_path(path, line_color);
                            }
                        }
                        // Crow's foot ("many"): three toes fanning from the apex.
                        for foot in &crows_feet {
                            for toe in foot.iter().skip(1) {
                                let mut builder = gpui::PathBuilder::stroke(stroke_width);
                                builder.move_to(foot[0]);
                                builder.line_to(*toe);
                                if let Ok(path) = builder.build() {
                                    window.paint_path(path, line_color);
                                }
                            }
                        }
                        // "One" bar: a single perpendicular tick.
                        for bar in &one_bars {
                            let mut builder = gpui::PathBuilder::stroke(stroke_width);
                            builder.move_to(bar[0]);
                            builder.line_to(bar[1]);
                            if let Ok(path) = builder.build() {
                                window.paint_path(path, line_color);
                            }
                        }
                    },
                )
                .absolute()
                .size_full(),
            );
        for placed in &self.placed {
            surface = surface.child(self.render_table_box(placed, zoom, cx));
        }

        v_flex()
            .key_context("ErdView")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(colors.editor_background)
            .on_action(cx.listener(|this, _: &CopyMermaid, _window, cx| this.copy_mermaid(cx)))
            .on_action(cx.listener(|this, _: &CopyDot, _window, cx| this.copy_dot(cx)))
            .on_action(cx.listener(|this, _: &CopySvg, _window, cx| this.copy_svg(cx)))
            .child(
                h_flex()
                    .w_full()
                    .px_2()
                    .py_1()
                    .gap_2()
                    .border_b_1()
                    .border_color(colors.border)
                    .child(Label::new(self.title.clone()).size(LabelSize::Small))
                    .child(div().flex_1())
                    .child(cyberpunk::segmented(vec![
                        IconButton::new("erd-zoom-out", IconName::Dash)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Zoom out"))
                            .on_click(cx.listener(|this, _, _, cx| this.zoom_out(cx)))
                            .into_any_element(),
                        Button::new(
                            "erd-zoom-reset",
                            format!("{}%", (zoom * 100.0).round() as i32),
                        )
                        .label_size(LabelSize::Small)
                        .tooltip(Tooltip::text("Reset zoom to 100%"))
                        .on_click(cx.listener(|this, _, _, cx| this.reset_zoom(cx)))
                        .into_any_element(),
                        IconButton::new("erd-zoom-in", IconName::Plus)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Zoom in"))
                            .on_click(cx.listener(|this, _, _, cx| this.zoom_in(cx)))
                            .into_any_element(),
                        IconButton::new("erd-zoom-fit", IconName::Maximize)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Zoom to fit the whole diagram"))
                            .on_click(cx.listener(|this, _, _, cx| this.zoom_to_fit(cx)))
                            .into_any_element(),
                    ]))
                    .child(cyberpunk::segmented(vec![
                        Button::new("erd-copy-mermaid", "Copy Mermaid")
                            .label_size(LabelSize::Small)
                            .tooltip(Tooltip::text("Copy the diagram as Mermaid erDiagram"))
                            .on_click(cx.listener(|this, _, _, cx| this.copy_mermaid(cx)))
                            .into_any_element(),
                        Button::new("erd-copy-dot", "Copy DOT")
                            .label_size(LabelSize::Small)
                            .tooltip(Tooltip::text("Copy the diagram as Graphviz DOT"))
                            .on_click(cx.listener(|this, _, _, cx| this.copy_dot(cx)))
                            .into_any_element(),
                        Button::new("erd-copy-svg", "Export SVG")
                            .label_size(LabelSize::Small)
                            .tooltip(Tooltip::text(
                                "Copy the diagram as a standalone SVG image document",
                            ))
                            .on_click(cx.listener(|this, _, _, cx| this.copy_svg(cx)))
                            .into_any_element(),
                    ])),
            )
            .child(
                div()
                    .id("erd-scroll")
                    .debug_selector(|| "ERD_SCROLL".into())
                    .flex_1()
                    .w_full()
                    .min_h_0()
                    .overflow_scroll()
                    .track_scroll(&self.scroll_handle)
                    .cursor(if self.pan_origin.is_some() {
                        CursorStyle::ClosedHand
                    } else {
                        CursorStyle::OpenHand
                    })
                    .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _window, cx| {
                        let delta_y: f32 = match event.delta {
                            ScrollDelta::Pixels(pixels) => pixels.y.into(),
                            ScrollDelta::Lines(lines) => lines.y * SCROLL_LINE_MULTIPLIER,
                        };
                        if this.apply_wheel_zoom(delta_y, &event.modifiers) {
                            cx.stop_propagation();
                            cx.notify();
                        }
                    }))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, _, cx| {
                            this.begin_pan(event.position, cx);
                        }),
                    )
                    .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                        if event.pressed_button == Some(MouseButton::Left) {
                            this.update_pan(event.position, cx);
                        }
                    }))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseUpEvent, _, cx| {
                            this.end_pan(cx);
                        }),
                    )
                    .child(surface)
                    .custom_scrollbars(
                        Scrollbars::always_visible(ScrollAxes::Both)
                            .tracked_scroll_handle(&self.scroll_handle),
                        window,
                        cx,
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn many_tables(count: usize) -> Vec<ErdTable> {
        (0..count)
            .map(|index| ErdTable {
                name: format!("table_{index}"),
                columns: vec![ErdColumn {
                    name: "id".into(),
                    data_type: "bigint".into(),
                    is_primary_key: true,
                    is_foreign_key: false,
                }],
            })
            .collect()
    }

    fn sample() -> (Vec<ErdTable>, Vec<ErdRelationship>) {
        let tables = vec![
            ErdTable {
                name: "users".into(),
                columns: vec![
                    ErdColumn {
                        name: "id".into(),
                        data_type: "bigint".into(),
                        is_primary_key: true,
                        is_foreign_key: false,
                    },
                    ErdColumn {
                        name: "email".into(),
                        data_type: "varchar(255)".into(),
                        is_primary_key: false,
                        is_foreign_key: false,
                    },
                ],
            },
            ErdTable {
                name: "orders".into(),
                columns: vec![
                    ErdColumn {
                        name: "id".into(),
                        data_type: "bigint".into(),
                        is_primary_key: true,
                        is_foreign_key: false,
                    },
                    ErdColumn {
                        name: "user_id".into(),
                        data_type: "bigint".into(),
                        is_primary_key: false,
                        is_foreign_key: true,
                    },
                ],
            },
        ];
        let relationships = vec![ErdRelationship {
            from_table: "orders".into(),
            from_column: "user_id".into(),
            to_table: "users".into(),
            to_column: "id".into(),
        }];
        (tables, relationships)
    }

    #[test]
    fn to_mermaid_emits_tables_keys_and_relationship() {
        let (tables, relationships) = sample();
        let mermaid = to_mermaid(&tables, &relationships);
        assert!(mermaid.starts_with("erDiagram\n"));
        assert!(mermaid.contains("    users {\n"));
        assert!(mermaid.contains("bigint id PK"));
        assert!(mermaid.contains("varchar_255_ email"));
        assert!(mermaid.contains("bigint user_id FK"));
        assert!(mermaid.contains("users ||--o{ orders : \"user_id\""));
    }

    #[test]
    fn to_dot_emits_records_and_edges() {
        let (tables, relationships) = sample();
        let dot = to_dot(&tables, &relationships);
        assert!(dot.starts_with("digraph erd {"));
        assert!(dot.contains("\"users\" [label=\"users|"));
        assert!(dot.contains("PK id : bigint"));
        assert!(dot.contains("FK user_id : bigint"));
        assert!(dot.contains("\"orders\" -> \"users\" [label=\"user_id\"];"));
    }

    #[test]
    fn to_svg_emits_a_well_formed_document_with_tables_and_a_relationship() {
        let (tables, relationships) = sample();
        let svg = to_svg(&tables, &relationships);
        assert!(svg.starts_with("<svg xmlns=\"http://www.w3.org/2000/svg\""));
        assert!(svg.trim_end().ends_with("</svg>"));
        assert!(
            svg.contains(">users<"),
            "table name should appear as text content"
        );
        assert!(svg.contains(">orders<"));
        assert!(
            svg.contains(">PK </text>"),
            "primary key marker should be rendered"
        );
        assert!(
            svg.contains(">FK </text>"),
            "foreign key marker should be rendered"
        );
        assert!(
            svg.contains("<polyline"),
            "relationship should render as a polyline"
        );
        assert!(
            svg.contains("<line"),
            "relationship endpoints should render crow's-foot / bar line marks"
        );
    }

    #[test]
    fn to_svg_escapes_special_characters_in_table_and_column_names() {
        let tables = vec![ErdTable {
            name: "a<b>&\"c\"".into(),
            columns: vec![ErdColumn {
                name: "x&y".into(),
                data_type: "varchar(10)".into(),
                is_primary_key: false,
                is_foreign_key: false,
            }],
        }];
        let svg = to_svg(&tables, &[]);
        assert!(
            !svg.contains("a<b>&\"c\""),
            "raw unescaped name must not appear"
        );
        assert!(svg.contains("a&lt;b&gt;&amp;&quot;c&quot;"));
        assert!(svg.contains("x&amp;y"));
    }

    // Column type strings observed against a live ClickHouse 24.10 instance
    // (`system.columns.type`): nested wrapper types (`Nullable`,
    // `LowCardinality`), composite types (`Array`, `Map`), and an `Enum8`
    // with embedded quotes and a comma -- exactly the punctuation the
    // Mermaid/DOT/SVG renderers below must not choke on. ClickHouse has no
    // foreign-key concept and `list_foreign_keys` is unimplemented for it
    // (falls back to `DbProvider`'s empty default), so a real ClickHouse ERD
    // always carries zero relationships -- this never passes any, matching
    // that.
    fn clickhouse_sample() -> Vec<ErdTable> {
        vec![ErdTable {
            name: "events".into(),
            columns: vec![
                ErdColumn {
                    name: "id".into(),
                    data_type: "UInt32".into(),
                    is_primary_key: true,
                    is_foreign_key: false,
                },
                ErdColumn {
                    name: "amount".into(),
                    data_type: "Nullable(UInt32)".into(),
                    is_primary_key: false,
                    is_foreign_key: false,
                },
                ErdColumn {
                    name: "tags".into(),
                    data_type: "Array(String)".into(),
                    is_primary_key: false,
                    is_foreign_key: false,
                },
                ErdColumn {
                    name: "attrs".into(),
                    data_type: "Map(String, String)".into(),
                    is_primary_key: false,
                    is_foreign_key: false,
                },
                ErdColumn {
                    name: "status".into(),
                    data_type: "Enum8('active' = 1, 'inactive' = 2)".into(),
                    is_primary_key: false,
                    is_foreign_key: false,
                },
                ErdColumn {
                    name: "label".into(),
                    data_type: "LowCardinality(String)".into(),
                    is_primary_key: false,
                    is_foreign_key: false,
                },
                ErdColumn {
                    name: "price".into(),
                    data_type: "Decimal(10, 2)".into(),
                    is_primary_key: false,
                    is_foreign_key: false,
                },
            ],
        }]
    }

    #[test]
    fn clickhouse_column_types_with_no_relationships_render_without_panicking_or_implying_fks() {
        let tables = clickhouse_sample();
        let relationships = Vec::new();

        let mermaid = to_mermaid(&tables, &relationships);
        assert!(mermaid.contains("    events {\n"));
        assert!(
            !mermaid.contains(" FK"),
            "ClickHouse has no FK concept -- nothing should be marked FK: {mermaid}"
        );
        assert!(mermaid.contains("UInt32 id PK"));

        let dot = to_dot(&tables, &relationships);
        assert!(dot.contains("\"events\""));
        assert!(!dot.contains("FK "));

        let svg = to_svg(&tables, &relationships);
        assert!(svg.starts_with("<svg"));
        assert!(svg.trim_end().ends_with("</svg>"));
        assert!(svg.contains(">events<"));
        assert!(
            !svg.contains("<polyline"),
            "no relationships means no relationship lines"
        );
        assert!(
            !svg.contains(">FK </text>"),
            "ClickHouse has no FK concept to mark"
        );
    }

    #[test]
    fn to_svg_handles_empty_schema_without_panicking() {
        let svg = to_svg(&[], &[]);
        assert!(svg.starts_with("<svg"));
        assert!(svg.trim_end().ends_with("</svg>"));
    }

    #[test]
    fn to_svg_skips_relationships_whose_endpoint_table_is_missing() {
        let (tables, _) = sample();
        // Reference a table name that was never placed -- must be skipped, not panic.
        let dangling = vec![ErdRelationship {
            from_table: "orders".into(),
            from_column: "user_id".into(),
            to_table: "does_not_exist".into(),
            to_column: "id".into(),
        }];
        let svg = to_svg(&tables, &dangling);
        assert!(
            !svg.contains("<polyline"),
            "a dangling relationship must not be drawn"
        );
    }

    #[test]
    fn layout_positions_wraps_after_columns_per_row() {
        let positions = layout_positions(5, 2);
        assert_eq!(positions, vec![(0, 0), (0, 1), (1, 0), (1, 1), (2, 0)]);
    }

    #[test]
    fn layout_positions_treats_zero_columns_as_one() {
        let positions = layout_positions(3, 0);
        assert_eq!(positions, vec![(0, 0), (1, 0), (2, 0)]);
    }

    #[test]
    fn box_height_grows_with_columns_and_caps_with_overflow_row() {
        let small = box_height(2);
        let capped = box_height(MAX_DISPLAY_COLUMNS + 5);
        assert!(small < capped);
        assert_eq!(
            capped,
            HEADER_HEIGHT + MAX_DISPLAY_COLUMNS as f32 * ROW_HEIGHT + ROW_HEIGHT
        );
    }

    #[test]
    fn box_origin_offsets_by_grid_step() {
        let (x0, y0) = box_origin(0, 0);
        let (x1, _) = box_origin(0, 1);
        let (_, y1) = box_origin(1, 0);
        assert_eq!((x0, y0), (OUTER_PADDING, OUTER_PADDING));
        assert!(x1 > x0);
        assert!(y1 > y0);
    }

    #[gpui::test]
    fn erd_view_builds_without_panicking(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });

        let (tables, relationships) = sample();
        let view = cx.add_window(|window, cx| {
            ErdView::new(tables, relationships, "Diagram: test", window, cx)
        });
        view.update(cx, |view, _, _| {
            assert_eq!(view.placed.len(), 2);
            assert_eq!(view.relationships.len(), 1);
            assert!(view.table_box("users").is_some());
            assert!(view.table_box("missing").is_none());
        })
        .expect("window should build");
    }

    struct ErdViewFrame {
        view: gpui::Entity<ErdView>,
    }

    impl Render for ErdViewFrame {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div().w(px(400.)).h(px(300.)).child(self.view.clone())
        }
    }

    fn draw_erd_frame(cx: &mut gpui::VisualTestContext) {
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        cx.run_until_parked();
    }

    // Renders a real ErdView inside a fixed 400x300 window and measures the
    // scroll handle's actual `max_offset()` after a real paint pass -- this is
    // the concrete, paint-derived proxy for "there is real scrollable range",
    // as opposed to reasoning about the styling code in isolation. A diagram
    // with many tables stacked vertically is far taller than the 300px frame,
    // so a correctly wired scroll container must report a positive y max
    // offset; if it reports zero, the content is being clipped/measured wrong
    // and the scrollbar has nothing to scroll regardless of how it's painted.
    #[gpui::test]
    fn scroll_handle_reports_real_range_when_content_exceeds_the_viewport(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });

        let tables = many_tables(30);
        let window = cx.add_window(|window, cx| {
            let erd = cx.new(|cx| ErdView::new(tables, Vec::new(), "Diagram: test", window, cx));
            ErdViewFrame { view: erd }
        });
        let mut cx = gpui::VisualTestContext::from_window(window.into(), cx);
        draw_erd_frame(&mut cx);

        let erd = window
            .update(&mut cx, |frame, _, _| frame.view.clone())
            .unwrap();
        let max_offset = erd.read_with(&cx, |view, _| view.scroll_handle.max_offset());

        assert!(
            max_offset.y > px(0.),
            "expected positive vertical scroll range for 30 stacked tables in a 300px-tall \
             viewport, got {max_offset:?} -- the diagram content is not exceeding the viewport \
             so there is nothing for the scrollbar to scroll"
        );
    }

    // Zooming in scales `total_width`/`total_height` (see `scaled_box`), so the
    // real measured scroll range must grow with it too -- this proves the zoom
    // factor actually reaches the painted layout the scrollbar measures against,
    // not just the in-memory field.
    #[gpui::test]
    fn scroll_range_grows_when_zooming_in(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });

        let tables = many_tables(6);
        let window = cx.add_window(|window, cx| {
            let erd = cx.new(|cx| ErdView::new(tables, Vec::new(), "Diagram: test", window, cx));
            ErdViewFrame { view: erd }
        });
        let mut cx = gpui::VisualTestContext::from_window(window.into(), cx);
        draw_erd_frame(&mut cx);
        let erd = window
            .update(&mut cx, |frame, _, _| frame.view.clone())
            .unwrap();

        let max_offset_at_1x = erd.read_with(&cx, |view, _| view.scroll_handle.max_offset());

        erd.update(&mut cx, |view, cx| {
            view.zoom_in(cx);
            view.zoom_in(cx);
        });
        draw_erd_frame(&mut cx);
        let max_offset_at_1_5x = erd.read_with(&cx, |view, _| view.scroll_handle.max_offset());

        assert!(
            max_offset_at_1_5x.y > max_offset_at_1x.y,
            "zooming in should increase the real measured scroll range: {max_offset_at_1x:?} \
             at 1.0x vs {max_offset_at_1_5x:?} after zooming in"
        );
    }

    // Drives a real mouse-down + mouse-move + mouse-up sequence over the canvas
    // (not a direct call to `update_pan`) and checks the scroll handle's actual
    // offset moved by the same delta as the cursor -- proving the gesture is
    // really wired to mouse events, not just that the underlying math is right.
    #[gpui::test]
    fn drag_pans_the_canvas_by_the_cursor_travel(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });

        let tables = many_tables(30);
        let window = cx.add_window(|window, cx| {
            let erd = cx.new(|cx| ErdView::new(tables, Vec::new(), "Diagram: test", window, cx));
            ErdViewFrame { view: erd }
        });
        let mut cx = gpui::VisualTestContext::from_window(window.into(), cx);
        draw_erd_frame(&mut cx);
        let erd = window
            .update(&mut cx, |frame, _, _| frame.view.clone())
            .unwrap();

        // Lock off the one-shot on-open fit so the drag runs at 100% zoom with
        // the full 2D scroll range; an auto-fit to 400x300 would zoom this
        // 30-table graph down until the horizontal range disappears.
        erd.update(&mut cx, |view, cx| view.reset_zoom(cx));
        draw_erd_frame(&mut cx);

        let scroll_bounds = cx
            .debug_bounds("ERD_SCROLL")
            .expect("scroll container should have painted bounds");
        let start = scroll_bounds.center();
        let end = point(start.x - px(15.), start.y - px(40.));

        let offset_before = erd.read_with(&cx, |view, _| view.scroll_handle.offset());
        assert!(
            erd.read_with(&cx, |view, _| view.pan_origin.is_none()),
            "canvas should not be mid-pan before any drag"
        );

        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
        assert!(
            erd.read_with(&cx, |view, _| view.pan_origin.is_some()),
            "mouse-down on the canvas should arm panning"
        );
        cx.simulate_mouse_move(end, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::none());
        assert!(
            erd.read_with(&cx, |view, _| view.pan_origin.is_none()),
            "mouse-up should disarm panning"
        );

        let offset_after = erd.read_with(&cx, |view, _| view.scroll_handle.offset());
        let expected = offset_before + (end - start);
        assert_eq!(
            offset_after, expected,
            "the content should move by exactly the cursor's own travel"
        );
    }

    // Reproduces "the horizontal scrollbar moves up and down / the vertical
    // scrollbar strangely changes size" (a real bug report, not a synthetic
    // one): captures the scroll container's own painted bounds and both axes'
    // `max_offset` at three points -- initial, after a diagonal scroll, and
    // after an idle re-render with no state change -- and asserts they never
    // move/shrink for a reason a scroll on the OTHER axis or a no-op render
    // should not cause.
    #[gpui::test]
    fn scroll_container_geometry_stays_stable_across_a_scroll_and_an_idle_render(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });

        // 6 tables at 3 columns/row and 220px/box comfortably overflow a
        // 400x300 window on both axes at once.
        let tables = many_tables(6);
        let window = cx.add_window(|window, cx| {
            let erd = cx.new(|cx| ErdView::new(tables, Vec::new(), "Diagram: test", window, cx));
            ErdViewFrame { view: erd }
        });
        let mut cx = gpui::VisualTestContext::from_window(window.into(), cx);
        draw_erd_frame(&mut cx);
        let erd = window
            .update(&mut cx, |frame, _, _| frame.view.clone())
            .unwrap();

        // Lock off the one-shot on-open fit so this test measures scroll
        // stability at a fixed 100% zoom, not a late auto-fit that would reset
        // the zoom and scroll offset mid-test.
        erd.update(&mut cx, |view, cx| view.reset_zoom(cx));
        draw_erd_frame(&mut cx);

        let bounds_initial = cx
            .debug_bounds("ERD_SCROLL")
            .expect("scroll container should have painted bounds");
        let max_offset_initial = erd.read_with(&cx, |view, _| view.scroll_handle.max_offset());
        assert!(
            max_offset_initial.x > px(0.) && max_offset_initial.y > px(0.),
            "expected both axes to overflow a 400x300 viewport with 6 tables at 3/row, got \
             {max_offset_initial:?}"
        );

        // Scroll the VERTICAL axis only (a real horizontal wheel gesture would
        // go through the container's own scroll handling; setting the offset
        // directly isolates the geometry question from input plumbing, which
        // `drag_pans_the_canvas_by_the_cursor_travel` already covers).
        erd.update(&mut cx, |view, cx| {
            let vertical_only = point(px(0.), -max_offset_initial.y / 2.0);
            view.scroll_handle.set_offset(vertical_only);
            cx.notify();
        });
        draw_erd_frame(&mut cx);

        let bounds_after_v_scroll = cx
            .debug_bounds("ERD_SCROLL")
            .expect("scroll container should still have painted bounds after a scroll");
        let max_offset_after_v_scroll =
            erd.read_with(&cx, |view, _| view.scroll_handle.max_offset());

        assert_eq!(
            bounds_after_v_scroll.origin, bounds_initial.origin,
            "the scroll container's own on-screen position must not move because its CONTENT \
             scrolled -- a vertical scroll must never shift the container's origin"
        );
        assert_eq!(
            bounds_after_v_scroll.size, bounds_initial.size,
            "the scroll container's own on-screen size must not change because its content \
             scrolled -- the viewport is fixed by the window, not by scroll position"
        );
        assert_eq!(
            max_offset_after_v_scroll, max_offset_initial,
            "max_offset (the content-vs-viewport overflow) must not change just because the \
             CURRENT scroll position changed -- it depends on content size and viewport size, \
             neither of which a scroll (as opposed to a zoom or resize) touches"
        );

        // An idle re-render with no state change at all must be a complete
        // no-op for geometry -- if a value here drifts, something is being
        // recomputed non-deterministically (e.g. from a fresh, un-cached
        // layout pass) rather than read from stable state.
        draw_erd_frame(&mut cx);
        let bounds_idle = cx
            .debug_bounds("ERD_SCROLL")
            .expect("scroll container should still have painted bounds after an idle render");
        let max_offset_idle = erd.read_with(&cx, |view, _| view.scroll_handle.max_offset());
        assert_eq!(
            bounds_idle, bounds_after_v_scroll,
            "an idle re-render (no state change) must not move or resize the scroll container"
        );
        assert_eq!(
            max_offset_idle, max_offset_after_v_scroll,
            "an idle re-render (no state change) must not change either axis's max_offset"
        );
    }

    #[test]
    fn edge_anchor_picks_the_facing_side_not_the_center() {
        let box_rect = (0.0, 0.0, 100.0, 40.0);
        // Something to the right and roughly level -> right edge, vertically centered.
        assert_eq!(edge_anchor(box_rect, (500.0, 20.0)), (100.0, 20.0));
        // Something to the left -> left edge.
        assert_eq!(edge_anchor(box_rect, (-500.0, 20.0)), (0.0, 20.0));
        // Something below -> bottom edge, horizontally centered.
        assert_eq!(edge_anchor(box_rect, (50.0, 500.0)), (50.0, 40.0));
        // Something above -> top edge.
        assert_eq!(edge_anchor(box_rect, (50.0, -500.0)), (50.0, 0.0));
    }

    #[test]
    fn elbow_route_bends_between_boxes_offset_in_both_axes() {
        let from_box = (0.0, 0.0, 100.0, 40.0);
        let to_box = (300.0, 200.0, 100.0, 40.0);
        let waypoints = elbow_route(from_box, to_box);
        assert_eq!(
            waypoints.len(),
            4,
            "route must be anchor, two bends, anchor"
        );
        let (start, bend1, bend2, end) = (waypoints[0], waypoints[1], waypoints[2], waypoints[3]);
        // Start and end anchor to box edges, not centers (50,20)/(350,220).
        assert_ne!(start, (50.0, 20.0));
        assert_ne!(end, (350.0, 220.0));
        // The route actually bends: the middle waypoints share one axis each
        // with an anchor, and the two bends share an x (the elbow's vertical
        // run), which is not true of a straight two-point diagonal.
        assert_eq!(bend1.1, start.1, "first bend keeps the source anchor's y");
        assert_eq!(bend2.1, end.1, "second bend takes on the target anchor's y");
        assert_eq!(
            bend1.0, bend2.0,
            "the two bends share an x (the vertical run)"
        );
    }

    #[test]
    fn elbow_route_collapses_to_a_straight_run_when_boxes_share_a_row() {
        let from_box = (0.0, 0.0, 100.0, 40.0);
        let to_box = (300.0, 0.0, 100.0, 40.0);
        let waypoints = elbow_route(from_box, to_box);
        // Same row -> anchors already share a y, so both bends collapse onto
        // that same y: the whole route is a single horizontal run.
        assert!(waypoints.iter().all(|point| point.1 == waypoints[0].1));
    }

    #[test]
    fn crows_foot_spreads_three_toes_at_the_anchor_with_the_apex_back_along_the_line() {
        // Line runs left -> right, marker anchored at (10, 0).
        let foot = crows_foot((0.0, 0.0), (10.0, 0.0), 8.0);
        // [apex, toe_left, toe_center, toe_right]
        assert_eq!(foot[0], (2.0, 0.0), "apex sits `size` back along the line");
        assert_eq!(
            foot[2],
            (10.0, 0.0),
            "center toe sits exactly on the anchor"
        );
        // Toes spread perpendicular by +/- size * MARKER_SPREAD = 6.0.
        assert_eq!(foot[1], (10.0, 6.0));
        assert_eq!(foot[3], (10.0, -6.0));
    }

    #[test]
    fn crows_foot_handles_coincident_points_without_panicking() {
        let foot = crows_foot((5.0, 5.0), (5.0, 5.0), 8.0);
        assert_eq!(foot, [(5.0, 5.0); 4]);
    }

    #[test]
    fn one_bar_draws_a_perpendicular_tick_back_from_the_anchor() {
        let bar = one_bar((0.0, 0.0), (10.0, 0.0), 8.0);
        // Center sits `size` back at (2, 0); bar spans +/- size * MARKER_SPREAD.
        assert_eq!(bar[0], (2.0, 6.0));
        assert_eq!(bar[1], (2.0, -6.0));
    }

    #[test]
    fn one_bar_handles_coincident_points_without_panicking() {
        let bar = one_bar((5.0, 5.0), (5.0, 5.0), 8.0);
        assert_eq!(bar, [(5.0, 5.0), (5.0, 5.0)]);
    }

    #[test]
    fn split_qualified_name_separates_schema_from_table() {
        assert_eq!(
            split_qualified_name("public.users"),
            (Some("public"), "users")
        );
        assert_eq!(split_qualified_name("users"), (None, "users"));
        // A leading/trailing dot is not a qualifier: keep the whole string.
        assert_eq!(split_qualified_name(".users"), (None, ".users"));
        assert_eq!(split_qualified_name("users."), (None, "users."));
        // Nested qualifier splits at the last dot.
        assert_eq!(
            split_qualified_name("db.public.users"),
            (Some("db.public"), "users")
        );
    }

    // Real end-to-end proof that relationship lines route around box edges and
    // carry crow's-foot ("many", FK side) and bar ("one", referenced side) end
    // markers, not a straight center-to-center diagonal through both tables'
    // contents -- calls the exact method `render` uses to build what it paints,
    // the closest achievable proxy since individual painted path segments have
    // no `debug_selector` to assert on directly.
    #[gpui::test]
    fn relationship_paths_mark_the_fk_side_with_a_crows_foot_and_the_referenced_side_with_a_bar(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });

        let (tables, relationships) = sample();
        let view = cx.add_window(|window, cx| {
            ErdView::new(tables, relationships, "Diagram: test", window, cx)
        });
        view.update(cx, |view, _, _| {
            let paths = view.relationship_paths();
            assert_eq!(paths.len(), 1, "expected exactly one relationship path");
            let geometry = &paths[0];
            let waypoints = &geometry.waypoints;

            let from_box = view.table_box("orders").expect("orders should be placed");
            let to_box = view.table_box("users").expect("users should be placed");
            let from_center = (from_box.0 + from_box.2 / 2.0, from_box.1 + from_box.3 / 2.0);
            let to_center = (to_box.0 + to_box.2 / 2.0, to_box.1 + to_box.3 / 2.0);
            // orders holds the FK (many side, route start); users is referenced
            // (one side, route end).
            let expected_many = edge_anchor(from_box, to_center);
            let expected_one = edge_anchor(to_box, from_center);

            assert_eq!(
                waypoints[0], expected_many,
                "route must start at the FK box's edge, not its center"
            );
            assert_eq!(
                *waypoints.last().unwrap(),
                expected_one,
                "route must end at the referenced box's edge, not its center"
            );
            // Crow's-foot center toe sits on the FK-side anchor (route start).
            assert_eq!(
                geometry.crows_foot[2], waypoints[0],
                "crow's-foot center toe must sit exactly at the FK-side anchor"
            );
            // The "one" bar is a tick set back from the referenced anchor, so its
            // midpoint is offset from that anchor (never drawn on top of it).
            let bar_mid = (
                (geometry.one_bar[0].0 + geometry.one_bar[1].0) / 2.0,
                (geometry.one_bar[0].1 + geometry.one_bar[1].1) / 2.0,
            );
            assert_ne!(
                bar_mid, expected_one,
                "the one-bar must sit back from the referenced anchor, not on it"
            );
        })
        .expect("window should build");
    }

    // A relationship between two tables stacked in the SAME grid column produces
    // a straight vertical route whose elbow collapses onto the endpoints, so the
    // immediate route neighbors are duplicates of the anchors. The end markers
    // must still be oriented from the nearest DISTINCT waypoint and render with
    // real extent -- not degenerate to invisible zero-length marks.
    #[gpui::test]
    fn vertical_same_column_relationship_still_gets_visible_end_markers(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });

        // With 3 columns/row, table indices 0 and 3 both land in column 0 (rows
        // 0 and 1), so a relationship between them routes straight down.
        let mut tables = many_tables(4);
        tables[3].columns.push(ErdColumn {
            name: "top_id".into(),
            data_type: "bigint".into(),
            is_primary_key: false,
            is_foreign_key: true,
        });
        let relationships = vec![ErdRelationship {
            from_table: "table_3".into(),
            from_column: "top_id".into(),
            to_table: "table_0".into(),
            to_column: "id".into(),
        }];
        let view = cx.add_window(|window, cx| {
            ErdView::new(tables, relationships, "Diagram: test", window, cx)
        });
        view.update(cx, |view, _, _| {
            let paths = view.relationship_paths();
            assert_eq!(
                paths.len(),
                1,
                "the same-column relationship must be routed"
            );
            let geometry = &paths[0];
            // Precondition: the route really is degenerate at both ends -- the
            // exact shape that used to erase the markers.
            assert_eq!(
                geometry.waypoints[0], geometry.waypoints[1],
                "a same-column vertical route starts with a zero-length segment"
            );
            let last = geometry.waypoints.len() - 1;
            assert_eq!(
                geometry.waypoints[last],
                geometry.waypoints[last - 1],
                "a same-column vertical route ends with a zero-length segment"
            );
            // The crow's-foot must have real extent (apex distinct from anchor).
            assert_ne!(
                geometry.crows_foot[0], geometry.crows_foot[2],
                "crow's-foot apex must sit back from the anchor, not collapse onto it"
            );
            // The one-bar must span a real width (its two ends differ).
            assert_ne!(
                geometry.one_bar[0], geometry.one_bar[1],
                "the one-bar must span a real width, not collapse to a point"
            );
        })
        .expect("window should build");
    }

    #[test]
    fn clamp_zoom_stays_within_bounds() {
        assert_eq!(clamp_zoom(1.0), 1.0);
        assert_eq!(clamp_zoom(MIN_ZOOM - 1.0), MIN_ZOOM);
        assert_eq!(clamp_zoom(MAX_ZOOM + 1.0), MAX_ZOOM);
    }

    #[test]
    fn scaled_box_scales_position_and_size_uniformly() {
        let (x, y, w, h) = scaled_box(10.0, 20.0, 220.0, 80.0, 2.0);
        assert_eq!((x, y, w, h), (20.0, 40.0, 440.0, 160.0));
        let (x, y, w, h) = scaled_box(10.0, 20.0, 220.0, 80.0, 0.5);
        assert_eq!((x, y, w, h), (5.0, 10.0, 110.0, 40.0));
    }

    #[test]
    fn fonts_and_geometry_share_one_zoom_factor() {
        assert_eq!(scaled(HEADER_FONT, 2.0), HEADER_FONT * 2.0);
        assert_eq!(scaled(COLUMN_FONT, 0.5), COLUMN_FONT * 0.5);
        assert_eq!(scaled(ICON_PX, 2.0), ICON_PX * 2.0);
        // Box width and font grow by the same ratio, so text stays in proportion
        // to the box at any zoom (image-like scaling, no drift).
        let zoom = 1.75;
        assert_eq!(
            scaled(BOX_WIDTH, zoom) / BOX_WIDTH,
            scaled(HEADER_FONT, zoom) / HEADER_FONT
        );
    }

    #[gpui::test]
    fn zoom_in_out_and_reset_clamp(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });

        let (tables, relationships) = sample();
        let view = cx.add_window(|window, cx| {
            ErdView::new(tables, relationships, "Diagram: test", window, cx)
        });
        view.update(cx, |view, _window, cx| {
            assert_eq!(view.zoom, 1.0);
            for _ in 0..20 {
                view.zoom_in(cx);
            }
            assert_eq!(view.zoom, MAX_ZOOM);
            for _ in 0..20 {
                view.zoom_out(cx);
            }
            assert_eq!(view.zoom, MIN_ZOOM);
            view.reset_zoom(cx);
            assert_eq!(view.zoom, 1.0);
        })
        .expect("window should build");
    }

    #[gpui::test]
    fn wheel_zoom_needs_modifier_and_clamps(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });

        let (tables, relationships) = sample();
        let view = cx.add_window(|window, cx| {
            ErdView::new(tables, relationships, "Diagram: test", window, cx)
        });
        view.update(cx, |view, _window, _cx| {
            assert_eq!(view.zoom, 1.0);

            // Plain wheel does not zoom.
            assert!(!view.apply_wheel_zoom(50.0, &Modifiers::none()));
            assert_eq!(view.zoom, 1.0);

            // Ctrl + wheel up zooms in, down zooms out.
            assert!(view.apply_wheel_zoom(50.0, &Modifiers::control()));
            assert!(view.zoom > 1.0);
            let zoomed_in = view.zoom;
            assert!(view.apply_wheel_zoom(-50.0, &Modifiers::control()));
            assert!(view.zoom < zoomed_in);

            // Stays within bounds under repeated zooming.
            for _ in 0..200 {
                view.apply_wheel_zoom(100.0, &Modifiers::control());
            }
            assert_eq!(view.zoom, MAX_ZOOM);
            for _ in 0..200 {
                view.apply_wheel_zoom(-100.0, &Modifiers::control());
            }
            assert_eq!(view.zoom, MIN_ZOOM);
        })
        .expect("window should build");
    }

    // Mirrors `scroll_container_geometry_stays_stable_across_a_scroll_and_an_idle_render`
    // but scrolls the HORIZONTAL axis only -- the exact inverse of that test,
    // and the exact user-reported symptom ("the vertical scrollbar shakes
    // when you swipe horizontally"). A real horizontal wheel event goes
    // through the container's own scroll handling (unlike the mirrored test,
    // which sets the offset directly), since input plumbing is precisely
    // what a wheel-driven horizontal scroll needs to prove.
    #[gpui::test]
    fn scroll_container_geometry_stays_stable_across_a_horizontal_scroll(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });

        let tables = many_tables(6);
        let window = cx.add_window(|window, cx| {
            let erd = cx.new(|cx| ErdView::new(tables, Vec::new(), "Diagram: test", window, cx));
            ErdViewFrame { view: erd }
        });
        let mut cx = gpui::VisualTestContext::from_window(window.into(), cx);
        draw_erd_frame(&mut cx);
        let erd = window
            .update(&mut cx, |frame, _, _| frame.view.clone())
            .unwrap();

        // Lock off the one-shot on-open fit so this test measures scroll
        // stability at a fixed 100% zoom, not a late auto-fit that would reset
        // the zoom and scroll offset mid-test.
        erd.update(&mut cx, |view, cx| view.reset_zoom(cx));
        draw_erd_frame(&mut cx);

        let bounds_initial = cx
            .debug_bounds("ERD_SCROLL")
            .expect("scroll container should have painted bounds");
        let max_offset_initial = erd.read_with(&cx, |view, _| view.scroll_handle.max_offset());
        assert!(
            max_offset_initial.x > px(0.),
            "expected horizontal overflow for 6 tables at 3/row in a 400px-wide viewport, got \
             {max_offset_initial:?}"
        );

        let center = bounds_initial.center();
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: center,
            delta: gpui::ScrollDelta::Pixels(point(-max_offset_initial.x / 2.0, px(0.))),
            modifiers: Modifiers::none(),
            touch_phase: gpui::TouchPhase::Moved,
        });
        draw_erd_frame(&mut cx);

        let offset_after = erd.read_with(&cx, |view, _| view.scroll_handle.offset());
        assert!(
            offset_after.x < px(0.),
            "a real horizontal wheel event must actually move the horizontal scroll offset, got \
             {offset_after:?}"
        );
        assert_eq!(
            offset_after.y,
            px(0.),
            "a horizontal-only wheel event must not touch the vertical offset"
        );

        let bounds_after_h_scroll = cx
            .debug_bounds("ERD_SCROLL")
            .expect("scroll container should still have painted bounds after a scroll");
        let max_offset_after_h_scroll =
            erd.read_with(&cx, |view, _| view.scroll_handle.max_offset());

        assert_eq!(
            bounds_after_h_scroll.origin, bounds_initial.origin,
            "the scroll container's own on-screen position must not move because its CONTENT \
             scrolled horizontally -- a horizontal scroll must never shift the container's \
             origin (this is the user-reported 'vertical scrollbar shakes' symptom)"
        );
        assert_eq!(
            bounds_after_h_scroll.size, bounds_initial.size,
            "the scroll container's own on-screen size must not change because its content \
             scrolled horizontally"
        );
        assert_eq!(
            max_offset_after_h_scroll, max_offset_initial,
            "max_offset must not change just because the current horizontal scroll position \
             changed"
        );
    }

    // `apply_fit` on a graph far larger than the viewport must zoom OUT (below
    // 1.0), frame the whole graph (or hit the min-zoom floor), and reset the
    // scroll to the origin. Directly exercises the fit math with an explicit
    // viewport, independent of paint timing.
    #[gpui::test]
    fn apply_fit_zooms_out_so_a_large_graph_fits_the_viewport(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });

        let tables = many_tables(30);
        let view = cx
            .add_window(|window, cx| ErdView::new(tables, Vec::new(), "Diagram: test", window, cx));
        view.update(cx, |view, _window, _cx| {
            assert_eq!(view.zoom, 1.0, "a fresh view starts at 100% zoom");
            let total_width = view.total_width;
            let total_height = view.total_height;

            view.apply_fit(gpui::size(px(400.), px(300.)));

            assert!(
                view.zoom < 1.0,
                "a 30-table graph must be zoomed out to fit a 400x300 viewport, got {}",
                view.zoom
            );
            // Either the whole graph now fits the viewport, or we bottomed out at
            // the minimum zoom (still the best achievable frame).
            let fits =
                total_width * view.zoom <= 400.0 + 0.5 && total_height * view.zoom <= 300.0 + 0.5;
            assert!(
                fits || view.zoom == MIN_ZOOM,
                "fit must frame the whole graph or clamp to the min zoom; zoom={}",
                view.zoom
            );
            assert_eq!(
                view.scroll_handle.offset(),
                point(px(0.), px(0.)),
                "fit resets the scroll so the framed graph starts at the top-left"
            );
        })
        .expect("window should build");
    }

    // Fit-on-open: after the scroll container has been measured, the one-shot
    // fit fires by itself (no toolbar click) and leaves a large schema zoomed
    // out. Pumps frames until the one-shot flag is consumed.
    #[gpui::test]
    fn opening_a_large_diagram_fits_it_to_the_viewport_automatically(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });

        let tables = many_tables(30);
        let window = cx.add_window(|window, cx| {
            let erd = cx.new(|cx| ErdView::new(tables, Vec::new(), "Diagram: test", window, cx));
            ErdViewFrame { view: erd }
        });
        let mut cx = gpui::VisualTestContext::from_window(window.into(), cx);
        let erd = window
            .update(&mut cx, |frame, _, _| frame.view.clone())
            .unwrap();

        assert!(
            erd.read_with(&cx, |view, _| view.pending_fit),
            "a freshly opened diagram arms the one-shot fit"
        );

        // First frame measures the container; the next consumes the fit.
        for _ in 0..MAX_FIT_ATTEMPTS {
            draw_erd_frame(&mut cx);
            if !erd.read_with(&cx, |view, _| view.pending_fit) {
                break;
            }
        }

        let (pending, zoom) = erd.read_with(&cx, |view, _| (view.pending_fit, view.zoom));
        assert!(
            !pending,
            "the one-shot fit must be consumed once the container is measured"
        );
        assert!(
            zoom < 1.0,
            "a 30-table schema must be auto-fitted (zoomed out) on open, got {zoom}"
        );
    }

    // Every column row (key and non-key alike) reserves a fixed leading gutter,
    // so a non-key column's name starts at the same x as a primary-key column's
    // name. Without the gutter, the non-key name shifts left under the missing
    // icon. Drives a real paint and compares the two names' painted origins.
    #[gpui::test]
    fn every_column_row_reserves_a_leading_gutter_so_names_align(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });

        let tables = vec![ErdTable {
            name: "account".into(),
            columns: vec![
                ErdColumn {
                    name: "id".into(),
                    data_type: "bigint".into(),
                    is_primary_key: true,
                    is_foreign_key: false,
                },
                ErdColumn {
                    name: "email".into(),
                    data_type: "varchar(255)".into(),
                    is_primary_key: false,
                    is_foreign_key: false,
                },
            ],
        }];
        let window = cx.add_window(|window, cx| {
            let erd = cx.new(|cx| ErdView::new(tables, Vec::new(), "Diagram: test", window, cx));
            ErdViewFrame { view: erd }
        });
        let mut cx = gpui::VisualTestContext::from_window(window.into(), cx);
        draw_erd_frame(&mut cx);

        let key_name = cx
            .debug_bounds("ERD_COL_NAME::id")
            .expect("the primary-key column name should paint");
        let plain_name = cx
            .debug_bounds("ERD_COL_NAME::email")
            .expect("the non-key column name should paint");

        assert_eq!(
            key_name.origin.x, plain_name.origin.x,
            "a non-key column name must align with a key column name via the fixed leading \
             gutter; key at {:?}, non-key at {:?}",
            key_name.origin, plain_name.origin
        );
    }
}
