use gpui::{
    Context, CursorStyle, DismissEvent, EventEmitter, FocusHandle, Focusable, Hsla, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Modifiers, Point, ScrollDelta, ScrollHandle,
    ScrollWheelEvent, Window, canvas, point, prelude::*, px,
};
use ui::{ScrollAxes, Scrollbars, Tooltip, WithScrollbar, prelude::*, rems_from_px};
use workspace::{Item, item::ItemEvent};

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

struct PlacedTable {
    table: ErdTable,
    origin_x: f32,
    origin_y: f32,
}

pub struct ErdView {
    focus_handle: FocusHandle,
    title: SharedString,
    placed: Vec<PlacedTable>,
    relationships: Vec<ErdRelationship>,
    mermaid: String,
    dot: String,
    total_width: f32,
    total_height: f32,
    scroll_handle: ScrollHandle,
    zoom: f32,
    // (mouse position, scroll offset) captured at the start of a click-drag pan;
    // `None` means the canvas is idle (not currently being dragged).
    pan_origin: Option<(Point<gpui::Pixels>, Point<gpui::Pixels>)>,
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
        let positions = layout_positions(tables.len(), DEFAULT_COLUMNS_PER_ROW);
        let mut placed = Vec::with_capacity(tables.len());
        let mut total_width: f32 = 0.0;
        let mut total_height: f32 = 0.0;
        for (table, (row, column)) in tables.into_iter().zip(positions) {
            let (origin_x, origin_y) = box_origin(row, column);
            total_width = total_width.max(origin_x + BOX_WIDTH + OUTER_PADDING);
            total_height = total_height.max(origin_y + box_height(table.columns.len()) + OUTER_PADDING);
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
            total_width: total_width.max(1.0),
            total_height: total_height.max(1.0),
            scroll_handle: ScrollHandle::new(),
            zoom: 1.0,
            pan_origin: None,
        }
    }

    fn zoom_in(&mut self, cx: &mut Context<Self>) {
        self.zoom = clamp_zoom(self.zoom + ZOOM_STEP);
        cx.notify();
    }

    fn zoom_out(&mut self, cx: &mut Context<Self>) {
        self.zoom = clamp_zoom(self.zoom - ZOOM_STEP);
        cx.notify();
    }

    fn reset_zoom(&mut self, cx: &mut Context<Self>) {
        self.zoom = 1.0;
        cx.notify();
    }

    fn begin_pan(&mut self, mouse_position: Point<gpui::Pixels>, cx: &mut Context<Self>) {
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
        self.zoom = clamp_zoom(self.zoom * factor);
        true
    }

    fn table_center(&self, table_name: &str) -> Option<(f32, f32)> {
        self.placed.iter().find_map(|placed| {
            if placed.table.name == table_name {
                let height = box_height(placed.table.columns.len());
                Some((
                    placed.origin_x + BOX_WIDTH / 2.0,
                    placed.origin_y + height / 2.0,
                ))
            } else {
                None
            }
        })
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
                    .items_center()
                    .bg(colors.element_background)
                    .border_b_1()
                    .border_color(colors.border)
                    .child(
                        div()
                            .text_size(header_font)
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(colors.text)
                            .truncate()
                            .child(placed.table.name.clone()),
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
            box_element = box_element.child(
                h_flex()
                    .h(px(scaled(ROW_HEIGHT, zoom)))
                    .w_full()
                    .px(cell_padding)
                    .gap(row_gap)
                    .items_center()
                    .when_some(key_color, |row, color| {
                        row.child(
                            Icon::new(if column.is_primary_key {
                                IconName::StarFilled
                            } else {
                                IconName::Link
                            })
                            .size(icon_size)
                            .color(Color::Custom(color)),
                        )
                    })
                    .child(
                        div()
                            .text_size(column_font)
                            .text_color(colors.text)
                            .truncate()
                            .child(column.name.clone()),
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
                    .items_center()
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
        let colors = cx.theme().colors();
        let line_color: Hsla = colors.border_variant;
        let zoom = self.zoom;

        let mut segments: Vec<(gpui::Point<gpui::Pixels>, gpui::Point<gpui::Pixels>)> = Vec::new();
        for relationship in &self.relationships {
            if let (Some(from), Some(to)) = (
                self.table_center(&relationship.from_table),
                self.table_center(&relationship.to_table),
            ) {
                segments.push((
                    point(px(from.0 * zoom), px(from.1 * zoom)),
                    point(px(to.0 * zoom), px(to.1 * zoom)),
                ));
            }
        }

        let mermaid = self.mermaid.clone();
        let dot = self.dot.clone();
        let stroke_width = px(scaled(LINE_STROKE, zoom));

        let mut surface = div()
            .relative()
            .w(px(scaled(self.total_width, zoom)))
            .h(px(scaled(self.total_height, zoom)))
            .child(
                canvas(
                    move |_, _, _| {},
                    move |_bounds, _, window, _| {
                        for (from, to) in &segments {
                            let mut builder = gpui::PathBuilder::stroke(stroke_width);
                            builder.move_to(*from);
                            builder.line_to(*to);
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
                    .child(
                        IconButton::new("erd-zoom-out", IconName::Dash)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Zoom out"))
                            .on_click(cx.listener(|this, _, _, cx| this.zoom_out(cx))),
                    )
                    .child(
                        Button::new("erd-zoom-reset", format!("{}%", (zoom * 100.0).round() as i32))
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::Small)
                            .tooltip(Tooltip::text("Reset zoom to 100%"))
                            .on_click(cx.listener(|this, _, _, cx| this.reset_zoom(cx))),
                    )
                    .child(
                        IconButton::new("erd-zoom-in", IconName::Plus)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Zoom in"))
                            .on_click(cx.listener(|this, _, _, cx| this.zoom_in(cx))),
                    )
                    .child(
                        Button::new("erd-copy-mermaid", "Copy Mermaid")
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::Small)
                            .tooltip(Tooltip::text("Copy the diagram as Mermaid erDiagram"))
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                    mermaid.clone(),
                                ));
                            })),
                    )
                    .child(
                        Button::new("erd-copy-dot", "Copy DOT")
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::Small)
                            .tooltip(Tooltip::text("Copy the diagram as Graphviz DOT"))
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.write_to_clipboard(gpui::ClipboardItem::new_string(dot.clone()));
                            })),
                    ),
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
        let view =
            cx.add_window(|window, cx| ErdView::new(tables, relationships, "Diagram: test", window, cx));
        view.update(cx, |view, _, _| {
            assert_eq!(view.placed.len(), 2);
            assert_eq!(view.relationships.len(), 1);
            assert!(view.table_center("users").is_some());
            assert!(view.table_center("missing").is_none());
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

        let erd = window.update(&mut cx, |frame, _, _| frame.view.clone()).unwrap();
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
        let erd = window.update(&mut cx, |frame, _, _| frame.view.clone()).unwrap();

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
        let erd = window.update(&mut cx, |frame, _, _| frame.view.clone()).unwrap();

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
        let view = cx
            .add_window(|window, cx| ErdView::new(tables, relationships, "Diagram: test", window, cx));
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
        let view = cx
            .add_window(|window, cx| ErdView::new(tables, relationships, "Diagram: test", window, cx));
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
}
