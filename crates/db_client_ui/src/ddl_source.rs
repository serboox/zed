use editor::Editor;
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, SharedString, Window,
    actions, prelude::*, px,
};
use ui::prelude::*;
use ui::{Button, Divider, Icon, Label};
use util::ResultExt;
use workspace::{Item, item::ItemEvent};

actions!(
    db_ddl_source,
    [
        /// Closes the DDL Source view.
        CloseDdlSource
    ]
);

#[derive(Debug, Clone, PartialEq)]
pub struct DdlColumn {
    pub name: String,
    pub data_type: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DdlTable {
    pub name: String,
    pub columns: Vec<DdlColumn>,
    pub raw_sql: String,
}

fn unquote_identifier(token: &str) -> String {
    let token = token.trim();
    let bytes = token.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0] as char;
        let last = bytes[bytes.len() - 1] as char;
        if (first == '`' && last == '`')
            || (first == '"' && last == '"')
            || (first == '[' && last == ']')
        {
            return token[1..token.len() - 1].to_string();
        }
    }
    token.to_string()
}

// Constraint-only lines inside a CREATE TABLE body that do not define a column.
fn is_constraint_line(line: &str) -> bool {
    let upper = line.trim_start().to_uppercase();
    [
        "PRIMARY KEY",
        "FOREIGN KEY",
        "UNIQUE",
        "KEY ",
        "KEY(",
        "INDEX",
        "CONSTRAINT",
        "CHECK",
        "FULLTEXT",
        "SPATIAL",
    ]
    .iter()
    .any(|prefix| upper.starts_with(prefix))
}

// Splits the parenthesized body of a CREATE TABLE into top-level items,
// respecting nested parentheses (e.g. ENUM(...), DECIMAL(10,2)).
fn split_top_level(body: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut current = String::new();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    for ch in body.chars() {
        if let Some(active) = quote {
            current.push(ch);
            if ch == active {
                quote = None;
            }
            continue;
        }
        match ch {
            '`' | '"' | '\'' => {
                quote = Some(ch);
                current.push(ch);
            }
            '(' => {
                depth += 1;
                current.push(ch);
            }
            ')' => {
                depth -= 1;
                current.push(ch);
            }
            ',' if depth == 0 => {
                items.push(std::mem::take(&mut current));
            }
            _ => current.push(ch),
        }
    }
    if !current.trim().is_empty() {
        items.push(current);
    }
    items
}

fn parse_column(item: &str) -> Option<DdlColumn> {
    let item = item.trim();
    if item.is_empty() || is_constraint_line(item) {
        return None;
    }
    let mut parts = item.splitn(2, char::is_whitespace);
    let name = unquote_identifier(parts.next()?.trim());
    if name.is_empty() {
        return None;
    }
    let data_type = parts.next().unwrap_or("").trim();
    // The type runs up to the first top-level comma already removed by the
    // splitter; keep the leading type token group as-is for display.
    let data_type = data_type
        .split(char::is_whitespace)
        .next()
        .unwrap_or("")
        .to_string();
    Some(DdlColumn { name, data_type })
}

/// Parses CREATE TABLE statements out of a `.sql` script into a virtual schema.
/// Column definitions are kept; key/constraint lines are skipped. The full text
/// of each statement is preserved as `raw_sql`.
pub fn parse_ddl_schema(sql: &str) -> Vec<DdlTable> {
    let mut tables = Vec::new();
    let upper = sql.to_uppercase();
    let mut search_from = 0;
    while let Some(relative) = upper[search_from..].find("CREATE TABLE") {
        let start = search_from + relative;
        // Find the opening parenthesis of the column list.
        let Some(open_relative) = sql[start..].find('(') else {
            break;
        };
        let open = start + open_relative;
        let header = &sql[start..open];
        let name = header
            .split_whitespace()
            .rfind(|token| {
                let upper = token.to_uppercase();
                !matches!(upper.as_str(), "CREATE" | "TABLE" | "IF" | "NOT" | "EXISTS")
            })
            .map(unquote_identifier)
            .unwrap_or_default();

        // Walk to the matching close parenthesis.
        let mut depth = 0i32;
        let mut close = open;
        let mut quote: Option<char> = None;
        for (offset, ch) in sql[open..].char_indices() {
            if let Some(active) = quote {
                if ch == active {
                    quote = None;
                }
                continue;
            }
            match ch {
                '`' | '"' | '\'' => quote = Some(ch),
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        close = open + offset;
                        break;
                    }
                }
                _ => {}
            }
        }
        if close <= open {
            break;
        }
        let body = &sql[open + 1..close];
        let columns = split_top_level(body)
            .iter()
            .filter_map(|item| parse_column(item))
            .collect();
        let statement_end = sql[close..]
            .find(';')
            .map(|index| close + index + 1)
            .unwrap_or(sql.len());
        let raw_sql = sql[start..statement_end].trim().to_string();
        if !name.is_empty() {
            tables.push(DdlTable {
                name,
                columns,
                raw_sql,
            });
        }
        search_from = statement_end;
    }
    tables
}

pub struct DdlSourceView {
    focus_handle: FocusHandle,
    path_editor: Entity<Editor>,
    tables: Vec<DdlTable>,
    selected: Option<usize>,
    status: Option<String>,
}

impl DdlSourceView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let path_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("/path/to/schema.sql", window, cx);
            editor
        });
        Self {
            focus_handle: cx.focus_handle(),
            path_editor,
            tables: Vec::new(),
            selected: None,
            status: None,
        }
    }

    fn load(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let path = self.path_editor.read(cx).text(cx).trim().to_string();
        if path.is_empty() {
            self.status = Some("Enter a .sql file path.".into());
            cx.notify();
            return;
        }
        self.status = Some("Loading…".into());
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let read = cx
                .background_spawn(async move { std::fs::read_to_string(&path) })
                .await;
            this.update(cx, |view, cx| {
                match read {
                    Ok(text) => {
                        let tables = parse_ddl_schema(&text);
                        view.status = Some(format!("Found {} tables.", tables.len()));
                        view.selected = (!tables.is_empty()).then_some(0);
                        view.tables = tables;
                    }
                    Err(error) => view.status = Some(format!("Could not read file: {error}")),
                }
                cx.notify();
            })
            .log_err();
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }
}

impl EventEmitter<DismissEvent> for DdlSourceView {}

impl Item for DdlSourceView {
    type Event = DismissEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "DDL Source".into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::FileCode))
    }

    fn to_item_events(_event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(ItemEvent::CloseItem);
    }
}

impl Focusable for DdlSourceView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for DdlSourceView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.selected;
        let table_list = v_flex().gap_0p5().children(
            self.tables
                .iter()
                .enumerate()
                .map(|(index, table)| {
                    let is_selected = selected == Some(index);
                    Button::new(("ddl-table", index), table.name.clone())
                        .style(if is_selected {
                            ButtonStyle::Tinted(ui::TintColor::Accent)
                        } else {
                            ButtonStyle::Subtle
                        })
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(move |view, _, _, cx| {
                            view.selected = Some(index);
                            cx.notify();
                        }))
                })
                .collect::<Vec<_>>(),
        );

        let detail = selected
            .and_then(|index| self.tables.get(index))
            .map(|table| {
                v_flex()
                    .gap_2()
                    .child(Label::new(table.name.clone()))
                    .child(
                        v_flex().gap_0p5().children(
                            table
                                .columns
                                .iter()
                                .map(|column| {
                                    h_flex()
                                        .gap_2()
                                        .child(
                                            Label::new(column.name.clone()).size(LabelSize::Small),
                                        )
                                        .child(
                                            Label::new(column.data_type.clone())
                                                .size(LabelSize::Small)
                                                .color(Color::Muted),
                                        )
                                })
                                .collect::<Vec<_>>(),
                        ),
                    )
                    .child(Divider::horizontal())
                    .child(
                        div()
                            .id("ddl-raw")
                            .max_h(px(180.0))
                            .overflow_y_scroll()
                            .child(
                                Label::new(table.raw_sql.clone())
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                    )
            });

        v_flex()
            .key_context("DdlSource")
            .track_focus(&self.focus_handle)
            .size_full()
            .p_4()
            .gap_3()
            .on_action(cx.listener(|_, _: &CloseDdlSource, _window, cx| cx.emit(DismissEvent)))
            .child(crate::widgets::dialog_header(
                "SQL schema source",
                "close-ddl",
                cx.listener(|_, _, _, cx| cx.emit(DismissEvent)),
            ))
            .child(
                h_flex()
                    .gap_2()
                    .child(crate::widgets::text_field(&self.path_editor, cx).flex_1())
                    .child(
                        Button::new("load-ddl", "Load")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|view, _, window, cx| view.load(window, cx))),
                    ),
            )
            .when_some(self.status.clone(), |el, status| {
                el.child(
                    Label::new(status)
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .child(
                h_flex()
                    .gap_4()
                    .h(px(360.0))
                    .child(
                        div()
                            .id("ddl-tables")
                            .w(px(220.0))
                            .overflow_y_scroll()
                            .child(table_list),
                    )
                    .child(Divider::vertical())
                    .child(div().flex_1().children(detail)),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_table() {
        let sql = "CREATE TABLE users (\n  id INT PRIMARY KEY,\n  name VARCHAR(255) NOT NULL\n);";
        let tables = parse_ddl_schema(sql);
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].name, "users");
        assert_eq!(tables[0].columns.len(), 2);
        assert_eq!(tables[0].columns[0].name, "id");
        assert_eq!(tables[0].columns[1].name, "name");
        assert_eq!(tables[0].columns[1].data_type, "VARCHAR(255)");
    }

    #[test]
    fn skips_constraint_lines() {
        let sql = "CREATE TABLE t (\n  id INT,\n  ref INT,\n  PRIMARY KEY (id),\n  FOREIGN KEY (ref) REFERENCES o(id)\n);";
        let tables = parse_ddl_schema(sql);
        assert_eq!(tables[0].columns.len(), 2);
        assert!(
            tables[0]
                .columns
                .iter()
                .all(|c| c.name == "id" || c.name == "ref")
        );
    }

    #[test]
    fn parses_multiple_tables_and_quoted_names() {
        let sql = "CREATE TABLE `a` (id INT);\nCREATE TABLE IF NOT EXISTS \"b\" (x TEXT, y TEXT);";
        let tables = parse_ddl_schema(sql);
        assert_eq!(tables.len(), 2);
        assert_eq!(tables[0].name, "a");
        assert_eq!(tables[1].name, "b");
        assert_eq!(tables[1].columns.len(), 2);
    }

    #[test]
    fn handles_nested_parens_in_types() {
        let sql = "CREATE TABLE t (price DECIMAL(10,2), tag ENUM('a','b'));";
        let tables = parse_ddl_schema(sql);
        assert_eq!(tables[0].columns.len(), 2);
        assert_eq!(tables[0].columns[0].data_type, "DECIMAL(10,2)");
    }

    #[test]
    fn empty_input_yields_no_tables() {
        assert!(parse_ddl_schema("").is_empty());
        assert!(parse_ddl_schema("SELECT 1;").is_empty());
    }
}
