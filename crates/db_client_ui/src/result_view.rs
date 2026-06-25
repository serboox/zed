use crate::store::DatabaseStore;
use db_client::{ConnectionId, schema::QueryResult};
use editor::Editor;
use gpui::{App, ClipboardItem, Context, ElementId, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, PromptLevel, Render, SharedString, WeakEntity, Window};
use ui::{Button, ButtonCommon, ButtonStyle, Color, Icon, IconButton, IconName, IconSize, Label, LabelSize, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{Item, Workspace};
use std::io::Write;

const PAGE_SIZE: usize = 200;
const DEFAULT_LIMIT: usize = 200;

pub enum ResultViewEvent {
    ResultChanged,
}

pub struct ResultView {
    focus_handle: FocusHandle,
    title: SharedString,
    pub result: Option<QueryResult>,
    pub error: Option<String>,
    page: usize,
    sort_column: Option<usize>,
    sort_ascending: bool,
    store: Option<WeakEntity<DatabaseStore>>,
    connection_id: Option<ConnectionId>,
    database: Option<String>,
    table_name: Option<String>,
    filter_editor: Option<Entity<Editor>>,
    workspace: Option<WeakEntity<Workspace>>,
    is_loading: bool,
}

impl ResultView {
    pub fn new(title: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            title: title.into(),
            result: None,
            error: None,
            page: 0,
            sort_column: None,
            sort_ascending: true,
            store: None,
            connection_id: None,
            database: None,
            table_name: None,
            filter_editor: None,
            workspace: None,
            is_loading: false,
        }
    }

    pub fn with_workspace(mut self, workspace: WeakEntity<Workspace>) -> Self {
        self.workspace = Some(workspace);
        self
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
        self
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

        let filter_text = self.filter_editor.as_ref().map(|ed| ed.read(cx).text(cx)).unwrap_or_default();
        let filter_text = filter_text.trim().to_string();

        let quote = match store.upgrade().and_then(|s| {
            let store_ref = s.read(cx);
            store_ref.connections().iter().find(|c| c.config.id == conn_id).map(|c| c.config.driver)
        }) {
            Some(db_client::DatabaseDriver::MySQL) => '`',
            _ => '"',
        };

        let mut sql = format!("SELECT * FROM {0}{1}{0}", quote, table);
        if !filter_text.is_empty() {
            sql.push_str(&format!(" WHERE {}", filter_text));
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
            }).log_err();
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    pub fn set_result(&mut self, result: QueryResult, cx: &mut Context<Self>) {
        self.result = Some(result);
        self.error = None;
        self.page = 0;
        self.sort_column = None;
        self.sort_ascending = true;
        cx.emit(ResultViewEvent::ResultChanged);
        cx.notify();
    }

    pub fn set_error(&mut self, error: String, cx: &mut Context<Self>) {
        self.error = Some(error);
        self.result = None;
        cx.emit(ResultViewEvent::ResultChanged);
        cx.notify();
    }

    fn export_csv(result: &QueryResult) -> String {
        let mut out = String::new();
        out.push_str(&result.columns.join(","));
        out.push('\n');
        for row in &result.rows {
            let cells: Vec<String> = row.iter().map(|c| {
                let s = c.as_deref().unwrap_or("");
                if s.contains(',') || s.contains('"') || s.contains('\n') {
                    format!("\"{}\"", s.replace('"', "\"\""))
                } else {
                    s.to_string()
                }
            }).collect();
            out.push_str(&cells.join(","));
            out.push('\n');
        }
        out
    }

    fn export_json(result: &QueryResult) -> String {
        let rows: Vec<String> = result.rows.iter().map(|row| {
            let pairs: Vec<String> = result.columns.iter().zip(row.iter()).map(|(col, cell)| {
                match cell {
                    Some(v) => format!("\"{}\":\"{}\"", col, v.replace('"', "\\\"")),
                    None => format!("\"{}\":null", col),
                }
            }).collect();
            format!("{{{}}}", pairs.join(","))
        }).collect();
        format!("[{}]", rows.join(","))
    }

    fn export_xlsx(result: &QueryResult) -> Vec<u8> {
        let buf = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(buf);
        let opts = zip::write::FileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);

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

        let mut sheet = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><sheetData>");

        let header_row: Vec<String> = result.columns.iter()
            .enumerate()
            .map(|(i, col)| format!("<c r=\"{}1\" t=\"inlineStr\" s=\"1\"><is><t>{}</t></is></c>", col_name(i), xml_escape(col)))
            .collect();
        sheet.push_str(&format!("<row r=\"1\">{}</row>", header_row.join("")));

        for (row_idx, row) in result.rows.iter().enumerate() {
            let row_num = row_idx + 2;
            let cells: Vec<String> = row.iter().enumerate().map(|(col_idx, cell)| {
                let cell_ref = format!("{}{}", col_name(col_idx), row_num);
                match cell {
                    Some(v) => {
                        if v.parse::<f64>().is_ok() {
                            format!("<c r=\"{}\"><v>{}</v></c>", cell_ref, xml_escape(v))
                        } else {
                            format!("<c r=\"{}\" t=\"inlineStr\"><is><t>{}</t></is></c>", cell_ref, xml_escape(v))
                        }
                    }
                    None => format!("<c r=\"{}\" t=\"inlineStr\"><is><t></t></is></c>", cell_ref),
                }
            }).collect();
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
        format!("UPDATE {} SET {} WHERE {};", table, set_clauses.join(", "), where_clause)
    }

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

    fn edit_row_as_sql(&self, row: &[Option<String>], columns: &[String], window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.clone() else { return };
        let table = self.table_name.clone().unwrap_or_default();
        let sql = Self::generate_update_sql(&table, columns, row);
        Self::open_sql_in_workspace(workspace, sql, window, cx);
    }

    fn open_sql_in_workspace(workspace: WeakEntity<Workspace>, text: String, window: &mut Window, cx: &mut Context<Self>) {
        let languages = workspace
            .update(cx, |ws, _cx| ws.app_state().languages.clone())
            .log_err();
        let Some(languages) = languages else { return };
        let language_task = languages.language_for_name("SQL");
        cx.spawn_in(window, async move |_, cx| {
            let language = language_task.await.log_err();
            workspace.update_in(cx, |workspace, window, cx| {
                let project = workspace.project().clone();
                let buffer_task = project.update(cx, move |project, cx| {
                    project.create_buffer(language, false, cx)
                });
                cx.spawn_in(window, async move |workspace, cx| {
                    let buffer = buffer_task.await?;
                    let multi = cx.new(|cx| {
                        multi_buffer::MultiBuffer::singleton(buffer, cx).with_title("query.sql".into())
                    });
                    workspace.update_in(cx, |workspace, window, cx| {
                        let editor = cx.new(|cx| {
                            let mut ed = Editor::for_multibuffer(multi, None, window, cx);
                            ed.set_text(text.clone(), window, cx);
                            ed
                        });
                        workspace.add_item_to_active_pane(Box::new(editor), None, true, window, cx);
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

    fn delete_row(&mut self, row: Vec<Option<String>>, columns: Vec<String>, window: &mut Window, cx: &mut Context<Self>) {
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
                let task = store.update(cx, |store, cx| {
                    store.execute_query(conn_id, db, sql, cx)
                })?;
                task.await.log_err();
                this.update_in(cx, |this, window, cx| {
                    this.refresh_table_data(window, cx);
                }).log_err();
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
                .child(Label::new("WHERE").size(LabelSize::Small).color(Color::Muted))
                .child(
                    div()
                        .flex_1()
                        .border_1()
                        .rounded_md()
                        .px_1()
                        .child(editor),
                )
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
        div()
            .flex()
            .flex_col()
            .size_full()
            .items_center()
            .justify_center()
            .child(Label::new("No results").size(LabelSize::Small).color(Color::Muted))
    }

    fn render_error(&self, error: &str) -> impl IntoElement {
        div()
            .p_4()
            .child(Label::new(error.to_string()).size(LabelSize::Small).color(Color::Error))
    }

    fn render_result(&self, result: &QueryResult, cx: &mut Context<Self>) -> impl IntoElement {
        let sort_column = self.sort_column;
        let sort_ascending = self.sort_ascending;
        let has_table_context = self.table_name.is_some() && self.workspace.is_some();

        let mut sorted_rows = result.rows.clone();
        if let Some(col_idx) = sort_column {
            sorted_rows.sort_by(|a, b| {
                let a_val = a.get(col_idx).and_then(|v| v.as_deref()).unwrap_or("");
                let b_val = b.get(col_idx).and_then(|v| v.as_deref()).unwrap_or("");
                let ord = a_val.cmp(b_val);
                if sort_ascending { ord } else { ord.reverse() }
            });
        }

        let total_rows = sorted_rows.len();
        let total_pages = total_rows.div_ceil(PAGE_SIZE).max(1);
        let page = self.page.min(total_pages - 1);
        let start = page * PAGE_SIZE;
        let end = (start + PAGE_SIZE).min(total_rows);
        let visible_rows = sorted_rows[start..end].to_vec();
        let columns = result.columns.clone();

        let status = format!(
            "{} row{} ({} ms) — showing {}-{} of {}",
            total_rows,
            if total_rows == 1 { "" } else { "s" },
            result.execution_time_ms,
            start + 1,
            end,
            total_rows,
        );

        let csv = Self::export_csv(result);
        let json = Self::export_json(result);

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
                            .child(Label::new(status).size(LabelSize::Small).color(Color::Muted)),
                    )
                    .child(
                        Button::new("copy-csv", "CSV")
                            .style(ButtonStyle::Subtle)
                            .tooltip(Tooltip::text("Copy as CSV"))
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(csv.clone()));
                            })),
                    )
                    .child(
                        Button::new("copy-json", "JSON")
                            .style(ButtonStyle::Subtle)
                            .tooltip(Tooltip::text("Copy as JSON"))
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(json.clone()));
                            })),
                    )
                    .child(
                        IconButton::new("save-csv", IconName::Download)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Save as CSV file"))
                            .on_click(cx.listener({
                                let csv_data = Self::export_csv(result);
                                move |_, _, _, cx| {
                                    let home = paths::home_dir().to_path_buf();
                                    let path_rx = cx.prompt_for_new_path(&home, Some("result.csv"));
                                    let data = csv_data.clone();
                                    cx.background_spawn(async move {
                                        let path = path_rx.await.log_err().and_then(|r| r.log_err()).flatten();
                                        if let Some(path) = path {
                                            std::fs::write(path, data).log_err();
                                        }
                                    })
                                    .detach();
                                }
                            })),
                    )
                    .child(
                        IconButton::new("save-json", IconName::FileCode)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Save as JSON file"))
                            .on_click(cx.listener({
                                let json_data = Self::export_json(result);
                                move |_, _, _, cx| {
                                    let home = paths::home_dir().to_path_buf();
                                    let path_rx = cx.prompt_for_new_path(&home, Some("result.json"));
                                    let data = json_data.clone();
                                    cx.background_spawn(async move {
                                        let path = path_rx.await.log_err().and_then(|r| r.log_err()).flatten();
                                        if let Some(path) = path {
                                            std::fs::write(path, data).log_err();
                                        }
                                    })
                                    .detach();
                                }
                            })),
                    )
                    .child(
                        IconButton::new("save-xlsx", IconName::FileDoc)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Save as Excel (XLSX) file"))
                            .on_click(cx.listener({
                                let xlsx_data = Self::export_xlsx(result);
                                move |_, _, _, cx| {
                                    let home = paths::home_dir().to_path_buf();
                                    let path_rx = cx.prompt_for_new_path(&home, Some("result.xlsx"));
                                    let data = xlsx_data.clone();
                                    cx.background_spawn(async move {
                                        let path = path_rx.await.log_err().and_then(|r| r.log_err()).flatten();
                                        if let Some(path) = path {
                                            std::fs::write(path, data).log_err();
                                        }
                                    })
                                    .detach();
                                }
                            })),
                    )
                    .when(total_pages > 1, |el| {
                        el.child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap_1()
                                .child(
                                    IconButton::new("prev-page", IconName::ChevronLeft)
                                        .icon_size(IconSize::XSmall)
                                        .disabled(page == 0)
                                        .tooltip(Tooltip::text("Previous page"))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            if this.page > 0 {
                                                this.page -= 1;
                                                cx.notify();
                                            }
                                        })),
                                )
                                .child(
                                    Label::new(format!("{}/{}", page + 1, total_pages))
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                                .child(
                                    IconButton::new("next-page", IconName::ChevronRight)
                                        .icon_size(IconSize::XSmall)
                                        .disabled(page + 1 >= total_pages)
                                        .tooltip(Tooltip::text("Next page"))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            if this.page + 1 < total_pages {
                                                this.page += 1;
                                                cx.notify();
                                            }
                                        })),
                                ),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .overflow_hidden()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .border_b_1()
                            .children(columns.iter().enumerate().map(|(col_idx, col)| {
                                let is_sorted = sort_column == Some(col_idx);
                                let sort_indicator = if is_sorted {
                                    if sort_ascending { " ↑" } else { " ↓" }
                                } else {
                                    ""
                                };
                                div()
                                    .id(ElementId::from(SharedString::from(format!("col-header-{col_idx}"))))
                                    .px_2()
                                    .py_1()
                                    .min_w(px(80.))
                                    .cursor_pointer()
                                    .hover(|s| s.bg(gpui::transparent_white()))
                                    .font_weight(gpui::FontWeight::BOLD)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if this.sort_column == Some(col_idx) {
                                            this.sort_ascending = !this.sort_ascending;
                                        } else {
                                            this.sort_column = Some(col_idx);
                                            this.sort_ascending = true;
                                        }
                                        this.page = 0;
                                        cx.notify();
                                    }))
                                    .child(Label::new(format!("{}{}", col, sort_indicator)).size(LabelSize::Small))
                            }))
                            .when(has_table_context, |el| {
                                el.child(
                                    div()
                                        .px_2()
                                        .py_1()
                                        .w(px(64.))
                                        .font_weight(gpui::FontWeight::BOLD)
                                        .child(Label::new("Actions").size(LabelSize::Small).color(Color::Muted)),
                                )
                            }),
                    )
                    .children(visible_rows.into_iter().enumerate().map(|(row_idx, row)| {
                        let row_cells = div()
                            .flex()
                            .flex_row()
                            .border_b_1()
                            .hover(|style| style.bg(gpui::transparent_white()))
                            .children(row.iter().enumerate().map(|(cell_idx, cell)| {
                                let display = cell.clone().unwrap_or_else(|| "NULL".to_string());
                                let copy_val = cell.clone().unwrap_or_default();
                                div()
                                    .id(ElementId::from(SharedString::from(format!("cell-{start}-{row_idx}-{cell_idx}"))))
                                    .px_2()
                                    .py_1()
                                    .min_w(px(80.))
                                    .cursor_pointer()
                                    .hover(|s| s.bg(gpui::transparent_black()))
                                    .tooltip(Tooltip::text("Click to copy"))
                                    .on_click(cx.listener(move |_, _, _, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(copy_val.clone()));
                                    }))
                                    .child(Label::new(display).size(LabelSize::Small))
                            }));

                        if has_table_context {
                            let row_data = row.clone();
                            let cols_for_edit = columns.clone();
                            let row_data_del = row;
                            let cols_for_del = columns.clone();
                            row_cells
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .items_center()
                                        .gap_1()
                                        .px_1()
                                        .w(px(64.))
                                        .child(
                                            IconButton::new(
                                                ElementId::from(SharedString::from(format!("edit-row-{start}-{row_idx}"))),
                                                IconName::Pencil,
                                            )
                                            .icon_size(IconSize::XSmall)
                                            .tooltip(Tooltip::text("Edit row (opens UPDATE in editor)"))
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.edit_row_as_sql(&row_data, &cols_for_edit, window, cx);
                                            })),
                                        )
                                        .child(
                                            IconButton::new(
                                                ElementId::from(SharedString::from(format!("del-row-{start}-{row_idx}"))),
                                                IconName::Trash,
                                            )
                                            .icon_size(IconSize::XSmall)
                                            .tooltip(Tooltip::text("Delete row"))
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.delete_row(row_data_del.clone(), cols_for_del.clone(), window, cx);
                                            })),
                                        ),
                                )
                                .into_any_element()
                        } else {
                            row_cells.into_any_element()
                        }
                    })),
            )
    }
}

impl EventEmitter<ResultViewEvent> for ResultView {}

impl Item for ResultView {
    type Event = ResultViewEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.title.clone()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<ui::Icon> {
        Some(Icon::new(IconName::DatabaseZap))
    }
}

impl Focusable for ResultView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ResultView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let filter_bar = self.render_filter_bar(cx);
        let content = if let Some(ref error) = self.error {
            self.render_error(error).into_any_element()
        } else if let Some(result) = self.result.clone() {
            self.render_result(&result, cx).into_any_element()
        } else if self.is_loading {
            div()
                .flex()
                .size_full()
                .items_center()
                .justify_center()
                .child(Label::new("Loading…").size(LabelSize::Small).color(Color::Muted))
                .into_any_element()
        } else {
            self.render_empty_state().into_any_element()
        };

        v_flex()
            .size_full()
            .when_some(filter_bar, |el, bar| el.child(bar))
            .child(div().flex_1().overflow_hidden().child(content))
    }
}
