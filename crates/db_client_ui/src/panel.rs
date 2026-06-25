use crate::connection_modal::ConnectionModal;
use crate::result_view::ResultView;
use crate::sql_completion_provider::install_on_editor;
use crate::store::{ActiveConnection, ConnectionStatus, DatabaseStore, DatabaseStoreEvent};
use db_client::{ConnectionConfig, DatabaseDriver, ProcedureKind, QueryResult, schema::ColumnInfo};
use editor::Editor;
use gpui::{
    Action, App, AsyncWindowContext, Context, ElementId, Entity, EventEmitter, FocusHandle,
    Focusable, IntoElement, ParentElement, PromptLevel, Render, SharedString,
    StatefulInteractiveElement, Styled, Subscription, WeakEntity, Window, div, px,
};
use multi_buffer::MultiBuffer;
use ui::{ContextMenu, Icon, IconButton, IconName, IconSize, Label, LabelSize, Tooltip, prelude::*, right_click_menu};
use util::ResultExt as _;
use workspace::{
    Event as WorkspaceEvent, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};
use zed_actions::database_panel::{NewQuery, RunQuery, ToggleFocus};

const DATABASE_PANEL_KEY: &str = "DatabasePanel";

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace
            .register_action(|workspace, _: &ToggleFocus, window, cx| {
                workspace.toggle_panel_focus::<DatabasePanel>(window, cx);
            })
            .register_action(|workspace, _: &NewQuery, window, cx| {
                open_new_sql_query(workspace, window, cx);
            })
            .register_action(|workspace, _: &RunQuery, window, cx| {
                run_current_sql_query(workspace, window, cx);
            });
    })
    .detach();
}

fn open_new_sql_query(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let languages = workspace.app_state().languages.clone();
    let language_task = languages.language_for_name("SQL");
    cx.spawn_in(window, async move |workspace, cx| {
        let language = language_task.await.log_err();
        workspace.update_in(cx, |workspace, window, cx| {
            let project = workspace.project().clone();
            let buffer_task = project.update(cx, move |project, cx| {
                project.create_buffer(language, false, cx)
            });
            cx.spawn_in(window, async move |workspace, cx| {
                let buffer = buffer_task.await?;
                let multi = cx.new(|cx| {
                    MultiBuffer::singleton(buffer, cx).with_title("query.sql".into())
                });
                workspace.update_in(cx, |workspace, window, cx| {
                    let editor = cx.new(|cx| Editor::for_multibuffer(multi, None, window, cx));
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

fn run_current_sql_query(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let panel = workspace.panel::<DatabasePanel>(cx);
    let panel = match panel {
        Some(p) => p,
        None => return,
    };
    let store = panel.read(cx).store.clone();

    let active_item = workspace.active_item(cx);
    let editor = active_item.and_then(|item| item.act_as::<Editor>(cx));
    let editor = match editor {
        Some(e) => e,
        None => return,
    };

    let sql = editor.read(cx).text(cx);
    if sql.trim().is_empty() {
        return;
    }

    let (conn_id, db_name) = {
        let store_ref = store.read(cx);
        let active_conn = store_ref
            .active_connection()
            .or_else(|| store_ref.connections().iter().find(|c| matches!(c.status, ConnectionStatus::Connected)));
        match active_conn {
            Some(c) => (c.config.id, c.config.database.clone().unwrap_or_default()),
            None => return,
        }
    };

    let query_task = store.update(cx, |store, cx| {
        store.execute_query(conn_id, db_name, sql, cx)
    });

    let result_view = cx.new(|cx| ResultView::new("Query Results", cx));
    let result_view_clone = result_view.clone();

    cx.spawn_in(window, async move |workspace, cx| {
        let outcome = query_task.await;
        result_view_clone.update(cx, |view, cx| match outcome {
            Ok(result) => view.set_result(result, cx),
            Err(err) => view.set_error(err.to_string(), cx),
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(result_view), None, true, window, cx);
        }).log_err();
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
    _subscriptions: Vec<Subscription>,
}

impl DatabasePanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        let result = workspace.update_in(&mut cx, |workspace, window, cx| {
            let store = cx.new(|cx| DatabaseStore::new(cx));
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
                    |_this: &mut DatabasePanel, _store: Entity<DatabaseStore>, _event: &DatabaseStoreEvent, cx: &mut Context<DatabasePanel>| {
                        cx.notify();
                    },
                );
                let store_weak = store.downgrade();
                let workspace_subscription = cx.subscribe(
                    &workspace_entity,
                    move |_this: &mut DatabasePanel, _workspace: Entity<Workspace>, event: &WorkspaceEvent, cx: &mut Context<DatabasePanel>| {
                        if let WorkspaceEvent::ItemAdded { item } = event {
                            if let Some(editor) = item.act_as::<Editor>(cx) {
                                install_on_editor(editor, store_weak.clone(), cx);
                            }
                        }
                    },
                );
                let filter_subscription = cx.subscribe(
                    &table_filter_editor,
                    |_this: &mut DatabasePanel, _editor: Entity<Editor>, _event: &editor::EditorEvent, cx: &mut Context<DatabasePanel>| {
                        cx.notify();
                    },
                );
                DatabasePanel {
                    focus_handle,
                    store,
                    workspace: workspace_handle,
                    history_expanded: false,
                    table_filter_editor,
                    _subscriptions: vec![store_subscription, workspace_subscription, filter_subscription],
                }
            })
        });
        result
    }

    fn open_add_connection_modal(&self, _window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        let workspace = self.workspace.clone();
        workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.toggle_modal(window, cx, |window, cx| {
                    ConnectionModal::new(window, cx).with_on_confirm(move |config, cx| {
                        store.update(cx, |store, cx| {
                            store.add_connection(config, cx);
                        });
                    })
                });
            })
            .log_err();
    }

    fn open_edit_connection_modal(&self, existing: ConnectionConfig, _window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        let workspace = self.workspace.clone();
        let original_id = existing.id;
        workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.toggle_modal(window, cx, |window, cx| {
                    ConnectionModal::new_with_config(&existing, window, cx).with_on_confirm(move |mut config, cx| {
                        config.id = original_id;
                        store.update(cx, |store, cx| {
                            store.update_connection(config, cx);
                        });
                    })
                });
            })
            .log_err();
    }

    fn open_sql_query_with_text(workspace: WeakEntity<Workspace>, text: String, window: &mut Window, cx: &mut Context<Self>) {
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
                        MultiBuffer::singleton(buffer, cx).with_title("query.sql".into())
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

    fn quote_ident(name: &str, driver: DatabaseDriver) -> String {
        match driver {
            DatabaseDriver::MySQL => format!("`{}`", name.replace('`', "``")),
            _ => format!("\"{}\"", name.replace('"', "\"\"")),
        }
    }

    fn generate_insert_template(table: &str, driver: DatabaseDriver, columns: &[ColumnInfo]) -> String {
        let qt = Self::quote_ident(table, driver);
        if columns.is_empty() {
            return format!("INSERT INTO {} () VALUES ();", qt);
        }
        let cols: Vec<String> = columns.iter().map(|c| Self::quote_ident(&c.name, driver)).collect();
        let placeholders: Vec<String> = columns.iter().map(|c| format!("'{}'", c.name)).collect();
        format!(
            "INSERT INTO {} ({})\nVALUES ({});",
            qt,
            cols.join(", "),
            placeholders.join(", "),
        )
    }

    fn generate_update_template(table: &str, driver: DatabaseDriver, columns: &[ColumnInfo]) -> String {
        let qt = Self::quote_ident(table, driver);
        if columns.is_empty() {
            return format!("UPDATE {} SET  WHERE ;", qt);
        }
        let pk = columns.iter().find(|c| c.column_key.as_deref() == Some("PRI"));
        let non_pk_cols: Vec<&ColumnInfo> = columns.iter().filter(|c| c.column_key.as_deref() != Some("PRI")).collect();
        let set_clause: Vec<String> = non_pk_cols
            .iter()
            .map(|c| format!("{} = '{}'", Self::quote_ident(&c.name, driver), c.name))
            .collect();
        let where_clause = if let Some(pk_col) = pk {
            format!("{} = '{}'", Self::quote_ident(&pk_col.name, driver), pk_col.name)
        } else {
            "1 = 1".to_string()
        };
        format!("UPDATE {} SET {}\nWHERE {};", qt, set_clause.join(",\n       "), where_clause)
    }

    fn mock_value(data_type: &str, row_num: usize) -> String {
        let upper = data_type.to_uppercase();
        if upper.starts_with("INT") || upper.starts_with("BIGINT") || upper.starts_with("SMALLINT")
            || upper.starts_with("TINYINT") || upper.starts_with("MEDIUMINT")
            || upper.starts_with("SERIAL") || upper.starts_with("INTEGER")
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
            return if row_num.is_multiple_of(2) { "true".to_string() } else { "false".to_string() };
        }
        if upper.starts_with("DATE") && !upper.contains("TIME") {
            return format!("'{}-{:02}-{:02}'", 2024, (row_num % 12) + 1, (row_num % 28) + 1);
        }
        if upper.starts_with("TIMESTAMP") || upper.starts_with("DATETIME") {
            return format!("'{}-{:02}-{:02} {:02}:00:00'", 2024, (row_num % 12) + 1, (row_num % 28) + 1, row_num % 24);
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

    fn generate_mock_data(table: &str, driver: DatabaseDriver, columns: &[ColumnInfo], count: usize) -> String {
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
                format!("INSERT INTO {} ({}) VALUES ({});", qt, col_list.join(", "), values.join(", "))
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn generate_delete_template(table: &str, driver: DatabaseDriver, columns: &[ColumnInfo]) -> String {
        let qt = Self::quote_ident(table, driver);
        let pk = columns.iter().find(|c| c.column_key.as_deref() == Some("PRI"));
        let where_clause = if let Some(pk_col) = pk {
            format!("{} = '{}'", Self::quote_ident(&pk_col.name, driver), pk_col.name)
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
                    .child(Label::new("Database").size(LabelSize::Small))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_1()
                            .child(
                                IconButton::new("new-query", IconName::File)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("New SQL Query"))
                                    .on_click(cx.listener(|_, _, window, cx| {
                                        window.dispatch_action(NewQuery.boxed_clone(), cx);
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
                    .child(div().flex_1().child(self.table_filter_editor.clone())),
            )
    }

    fn render_connection_item(
        &self,
        conn: ActiveConnection,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let id = conn.config.id;
        let label = conn.config.label.clone();
        let driver_label = conn.config.driver.to_string();
        let driver = conn.config.driver;
        let config_for_edit = conn.config.clone();

        let (status_icon, status_color) = match &conn.status {
            ConnectionStatus::Connected => (IconName::DatabaseZap, Color::Success),
            ConnectionStatus::Connecting => (IconName::ArrowCircle, Color::Modified),
            ConnectionStatus::Disconnected => (IconName::DatabaseZap, Color::Muted),
            ConnectionStatus::Error(_) => (IconName::Warning, Color::Error),
        };
        let is_connected = matches!(conn.status, ConnectionStatus::Connected);
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
        let table_filter = self.table_filter_editor.read(cx).text(cx).to_lowercase();
        let entity = cx.entity();

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
                    .px_2()
                    .py_1()
                    .hover(|style| style.bg(gpui::transparent_white()))
                    .when(is_active, |el| el.bg(gpui::transparent_black()))
                    .when(is_connected, |el| {
                        el.cursor_pointer().on_click(cx.listener(move |this, _, _, cx| {
                            this.store.update(cx, |store, cx| {
                                store.set_active_connection(id, cx);
                            });
                        }))
                    })
                    .child(Icon::new(status_icon).size(IconSize::Small).color(status_color))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .overflow_hidden()
                            .child(Label::new(label).size(LabelSize::Small))
                            .child(Label::new(driver_label).size(LabelSize::XSmall).color(Color::Muted)),
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
                                        store.refresh_databases(id, cx).detach_and_log_err(cx);
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
            .when_some(databases, |el, dbs| {
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
                            move |this, _, _, cx| {
                                this.store.update(cx, |store, cx| {
                                    store
                                        .toggle_database_expanded(id, db_name.clone(), cx)
                                        .detach_and_log_err(cx);
                                });
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
                                                Self::open_sql_query_with_text(workspace.clone(), sql, window, cx);
                                                let _ = panel;
                                            });
                                        }
                                    })
                                    .entry("Refresh Tables", None, {
                                        let entity = entity.clone();
                                        let db = db.clone();
                                        move |_, cx| {
                                            entity.update(cx, |panel, cx| {
                                                panel.store.update(cx, |store, cx| {
                                                    store.toggle_database_expanded(id, db.clone(), cx).detach_and_log_err(cx);
                                                    store.toggle_database_expanded(id, db.clone(), cx).detach_and_log_err(cx);
                                                });
                                            });
                                        }
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
                                    if !table_filter.is_empty() && !table_name.to_lowercase().contains(&table_filter) {
                                        return None;
                                    }
                                    let tbl_key = (db_name.clone(), table_name.clone());
                                    let is_table_expanded = expanded_table_set.contains(&tbl_key);
                                    let table_columns = expanded_tables.get(&tbl_key).cloned();
                                    let db_for_table = db_name.clone();

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
                                            move |this, _, _, cx| {
                                                this.store.update(cx, |store, cx| {
                                                    store
                                                        .toggle_table_expanded(
                                                            id, db.clone(), tbl.clone(), cx,
                                                        )
                                                        .detach_and_log_err(cx);
                                                });
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
                                        .child(Label::new(table_name.clone()).size(LabelSize::Small))
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
                                                        "SELECT * FROM {} LIMIT 200",
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
                                                        this.update_in(cx, |_, window, cx| {
                                                            Self::open_sql_query_with_text(ws.clone(), ddl, window, cx);
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
                                                move |_, _, window, cx| {
                                                    let insert = Self::generate_insert_template(&tbl, driver, &cols);
                                                    let update = Self::generate_update_template(&tbl, driver, &cols);
                                                    let delete = Self::generate_delete_template(&tbl, driver, &cols);
                                                    let sql = format!("{}\n\n{}\n\n{}", insert, update, delete);
                                                    Self::open_sql_query_with_text(workspace.clone(), sql, window, cx);
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
                                                                    "SELECT * FROM {} LIMIT 200",
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
                                                                Self::open_sql_query_with_text(workspace.clone(), sql, window, cx);
                                                                let _ = panel;
                                                            });
                                                        }
                                                    })
                                                    .separator()
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
                                                                    this.update_in(cx, |_, window, cx| {
                                                                        Self::open_sql_query_with_text(ws.clone(), ddl, window, cx);
                                                                    }).log_err();
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
                                                                Self::open_sql_query_with_text(workspace.clone(), sql, window, cx);
                                                                let _ = panel;
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
                                                                Self::open_sql_query_with_text(workspace.clone(), sql, window, cx);
                                                                let _ = panel;
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
                                                                Self::open_sql_query_with_text(workspace.clone(), sql, window, cx);
                                                                let _ = panel;
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
                                                                Self::open_sql_query_with_text(workspace.clone(), sql, window, cx);
                                                                let _ = panel;
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
                                                                                store.refresh_databases(id, cx).detach_and_log_err(cx);
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
                                                                Self::open_sql_query_with_text(workspace.clone(), sql, window, cx);
                                                                let _ = panel;
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
                                                                Self::open_sql_query_with_text(workspace.clone(), sql, window, cx);
                                                                let _ = panel;
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
                                                el.children(columns.into_iter().map(|col| {
                                                    let key_indicator = col
                                                        .column_key
                                                        .as_deref()
                                                        .unwrap_or("")
                                                        .to_string();
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
                                                        .when(!key_indicator.is_empty(), |el| {
                                                            el.child(
                                                                Label::new(key_indicator)
                                                                    .size(LabelSize::XSmall)
                                                                    .color(Color::Accent),
                                                            )
                                                        })
                                                }))
                                            })
                                        }))
                                }))
                            })
                        })
                }))
            })
    }

    fn render_history(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let history = self.store.read(cx).query_history().to_vec();
        let is_expanded = self.history_expanded;

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
                        move |_, _, window, cx| {
                            Self::open_sql_query_with_text(workspace.clone(), query.clone(), window, cx);
                        }
                    }))
                    .child(Label::new(display).size(LabelSize::XSmall).color(Color::Muted));
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
                    .child(Label::new("Query History").size(LabelSize::XSmall).color(Color::Muted)),
            )
            .children(history_items)
    }
}

impl Render for DatabasePanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let connections: Vec<ActiveConnection> = self
            .store
            .read(cx)
            .connections()
            .iter()
            .cloned()
            .collect();

        let mut conn_items = Vec::new();
        for conn in connections {
            conn_items.push(self.render_connection_item(conn, cx));
        }

        v_flex()
            .key_context("DatabasePanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .overflow_hidden()
            .child(self.render_toolbar(cx))
            .child(
                div()
                    .id("db-panel-scroll")
                    .flex_1()
                    .overflow_y_scroll()
                    .children(conn_items),
            )
            .when(self.store.read(cx).connections().is_empty(), |el| {
                el.child(
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .flex_1()
                        .gap_2()
                        .p_4()
                        .child(Icon::new(IconName::DatabaseZap).size(IconSize::Medium).color(Color::Muted))
                        .child(Label::new("No connections").size(LabelSize::Small).color(Color::Muted))
                        .child(
                            Label::new("Click + to add a connection")
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                )
            })
            .when(!self.store.read(cx).query_history().is_empty(), |el| {
                el.child(self.render_history(cx))
            })
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
        Some("Database Panel")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        8
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::{TestAppContext, VisualTestContext};
    use project::Project;
    use settings::SettingsStore;
    use workspace::MultiWorkspace;
    use zed_actions::database_panel::ToggleFocus;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            super::init(cx);
        });
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
            assert!(dock.is_open(), "left dock must be open after toggle_panel_focus");
            assert!(dock.panel::<DatabasePanel>().is_some(), "DatabasePanel must be in left dock");
        });
    }

    // Replicates what the real app does via observe_new path:
    // 1. Uses DatabasePanel::load (as initialize_panels in zed.rs does)
    // 2. Dispatches ToggleFocus action (as the View menu click does)
    // 3. Asserts the dock opened and the panel is visible
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
                cx.spawn_in(window, async move |workspace_handle, cx: &mut AsyncWindowContext| {
                    DatabasePanel::load(workspace_handle, cx.clone()).await
                })
            })
            .await
            .expect("DatabasePanel::load must succeed");

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_panel(panel, window, cx);
        });

        cx.run_until_parked();

        // Dispatch ToggleFocus exactly as the View menu click does
        cx.dispatch_action(ToggleFocus);

        cx.run_until_parked();

        workspace.read_with(cx, |workspace, cx| {
            let dock = workspace.left_dock().read(cx);
            assert!(dock.is_open(), "left dock must be open after ToggleFocus action dispatch");
            assert!(dock.panel::<DatabasePanel>().is_some(), "DatabasePanel must be in left dock");
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
                cx.spawn_in(window, async move |workspace_handle, cx: &mut AsyncWindowContext| {
                    DatabasePanel::load(workspace_handle, cx.clone()).await
                })
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
}
