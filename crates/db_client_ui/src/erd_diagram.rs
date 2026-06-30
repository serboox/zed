use gpui::{
    Context, DismissEvent, EventEmitter, FocusHandle, Focusable, Hsla, Window, canvas, point,
    prelude::*, px,
};
use ui::{Tooltip, prelude::*};
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
        }
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

    fn render_table_box(&self, placed: &PlacedTable, cx: &Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let visible = displayed_column_count(placed.table.columns.len());
        let hidden = placed.table.columns.len().saturating_sub(visible);
        let mut box_element = v_flex()
            .absolute()
            .left(px(placed.origin_x))
            .top(px(placed.origin_y))
            .w(px(BOX_WIDTH))
            .border_1()
            .border_color(colors.border)
            .rounded_md()
            .bg(colors.elevated_surface_background)
            .overflow_hidden()
            .child(
                h_flex()
                    .h(px(HEADER_HEIGHT))
                    .w_full()
                    .px_2()
                    .items_center()
                    .bg(colors.element_background)
                    .border_b_1()
                    .border_color(colors.border)
                    .child(
                        Label::new(placed.table.name.clone())
                            .size(LabelSize::Small)
                            .weight(gpui::FontWeight::SEMIBOLD)
                            .truncate(),
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
                    .h(px(ROW_HEIGHT))
                    .w_full()
                    .px_2()
                    .gap_1()
                    .items_center()
                    .when_some(key_color, |row, color| {
                        row.child(
                            Icon::new(if column.is_primary_key {
                                IconName::StarFilled
                            } else {
                                IconName::Link
                            })
                            .size(IconSize::XSmall)
                            .color(Color::Custom(color)),
                        )
                    })
                    .child(
                        Label::new(column.name.clone())
                            .size(LabelSize::XSmall)
                            .truncate(),
                    )
                    .child(
                        div().flex_1().child(
                            Label::new(column.data_type.clone())
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .truncate(),
                        ),
                    ),
            );
        }
        if hidden > 0 {
            box_element = box_element.child(
                h_flex().h(px(ROW_HEIGHT)).w_full().px_2().items_center().child(
                    Label::new(format!("+{hidden} more"))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let line_color: Hsla = colors.border_variant;

        let mut segments: Vec<(gpui::Point<gpui::Pixels>, gpui::Point<gpui::Pixels>)> = Vec::new();
        for relationship in &self.relationships {
            if let (Some(from), Some(to)) = (
                self.table_center(&relationship.from_table),
                self.table_center(&relationship.to_table),
            ) {
                segments.push((
                    point(px(from.0), px(from.1)),
                    point(px(to.0), px(to.1)),
                ));
            }
        }

        let mermaid = self.mermaid.clone();
        let dot = self.dot.clone();

        let mut surface = div()
            .relative()
            .w(px(self.total_width))
            .h(px(self.total_height))
            .child(
                canvas(
                    move |_, _, _| {},
                    move |_bounds, _, window, _| {
                        for (from, to) in &segments {
                            let mut builder = gpui::PathBuilder::stroke(px(1.5));
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
            surface = surface.child(self.render_table_box(placed, cx));
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
                    .flex_1()
                    .w_full()
                    .overflow_scroll()
                    .child(surface),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
