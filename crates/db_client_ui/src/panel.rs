use crate::connection_view::ConnectionView;
use crate::driver_icon::brand_icon;
use crate::result_view::ResultView;
use crate::sql_completion_provider::install_on_editor;
use crate::store::{ActiveConnection, ConnectionStatus, DatabaseStore, DatabaseStoreEvent};
use db_client::{
    ConnectionConfig, ConnectionId, DatabaseDriver, ProcedureKind, QueryResult, schema::ColumnInfo,
};
use editor::{Editor, EditorEvent, ToOffset};
use gpui::{
    App, AsyncWindowContext, ClickEvent, Context, ElementId, Entity, EventEmitter, FocusHandle,
    Focusable, IntoElement, ParentElement, PromptLevel, Render, SharedString,
    StatefulInteractiveElement, Styled, Subscription, WeakEntity, Window, div, px,
};
use multi_buffer::MultiBuffer;
use std::collections::HashSet;
use terminal_view::terminal_panel::TerminalPanel;
use ui::{ContextMenu, Icon, IconButton, IconName, IconSize, Indicator, Label, LabelSize, Tooltip, prelude::*, right_click_menu};
use util::ResultExt as _;
use workspace::{
    Event as WorkspaceEvent, ItemHandle, OpenOptions, OpenVisible, Pane, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};
use zed_actions::database_panel::ToggleFocus;

const DATABASE_PANEL_KEY: &str = "DatabasePanel";

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
struct DbQueryEditorAddon {
    connection_id: ConnectionId,
}

impl editor::Addon for DbQueryEditorAddon {
    fn to_any(&self) -> &dyn std::any::Any {
        self
    }
}

// Returns the `;`-delimited SQL statement that contains the byte offset
// `cursor`, trimmed. `;` is ASCII so byte scanning stays on char boundaries.
fn statement_at_cursor(text: &str, cursor: usize) -> String {
    let cursor = cursor.min(text.len());
    let start = text[..cursor].rfind(';').map(|i| i + 1).unwrap_or(0);
    let end = text[cursor..]
        .find(';')
        .map(|i| cursor + i)
        .unwrap_or(text.len());
    text[start..end].trim().to_string()
}

// Splits connections into the top-level group (folder == None) and named
// folders. Both the top-level list and each folder's contents are sorted by
// label (case-insensitive, then by id to keep duplicates stable); folders are
// returned sorted by name. Returns indices into the input slice so callers can
// render the original `ActiveConnection` without cloning here. Pure so the
// grouping/sorting is unit-tested without the GPUI render path.
fn group_connections_by_folder(
    connections: &[ActiveConnection],
) -> (Vec<usize>, Vec<(String, Vec<usize>)>) {
    let mut top_level: Vec<usize> = Vec::new();
    let mut folders: std::collections::BTreeMap<String, Vec<usize>> =
        std::collections::BTreeMap::new();

    for (index, connection) in connections.iter().enumerate() {
        match connection.config.folder.as_deref() {
            None => top_level.push(index),
            Some(folder) if folder.trim().is_empty() => top_level.push(index),
            Some(folder) => folders
                .entry(folder.trim().to_string())
                .or_default()
                .push(index),
        }
    }

    let sort_key = |index: &usize| -> (String, ConnectionId) {
        let config = &connections[*index].config;
        (config.label.to_lowercase(), config.id)
    };
    top_level.sort_by_key(sort_key);

    let folders = folders
        .into_iter()
        .map(|(name, mut indices)| {
            indices.sort_by_key(sort_key);
            (name, indices)
        })
        .collect();

    (top_level, folders)
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

/// Opens the persistent SQL console for `connection_id`. The file lives on disk
/// (openable even when the database is not connected) and auto-saves whenever
/// the editor loses focus, so there is never a save prompt. Ctrl+Enter runs the
/// statement under the cursor against this connection.
pub fn open_new_sql_query(
    _workspace: &mut Workspace,
    connection_id: ConnectionId,
    connection_label: String,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
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

        workspace.update_in(cx, |_workspace, _window, cx| {
            let Some(editor) = item.act_as::<Editor>(cx) else {
                return;
            };
            editor.update(cx, |editor, _| {
                editor.register_addon(DbQueryEditorAddon { connection_id });
            });
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
    window: &mut Window,
    cx: &mut App,
) -> Entity<ResultView> {
    let existing = pane
        .read(cx)
        .items_of_type::<ResultView>()
        .find(|view| view.read(cx).connection_id() == Some(connection_id));

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

    let view = cx.new(|cx| ResultView::new(title, cx).with_connection(connection_id));
    pane.update(cx, |pane, cx| {
        pane.add_item(Box::new(view.clone()), true, true, None, window, cx);
    });
    view
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
    run_sql_from_editor(workspace, window, cx, |sql| format!("EXPLAIN {sql}"));
}

fn run_sql_from_editor(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    transform: impl FnOnce(String) -> String,
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

    // Run the selection if there is one, otherwise just the statement under the
    // cursor — never the whole editor, which would fire every query at once.
    let sql = editor.update(cx, |editor, cx| {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let selection = editor.selections.newest_anchor();
        let start = selection.start.to_offset(&snapshot).0;
        let end = selection.end.to_offset(&snapshot).0;
        let full = editor.text(cx);
        if start != end {
            let (lo, hi) = (start.min(end), start.max(end));
            full.get(lo..hi).map(|s| s.to_string()).unwrap_or(full)
        } else {
            let cursor = selection.head().to_offset(&snapshot).0;
            statement_at_cursor(&full, cursor)
        }
    });
    if sql.trim().is_empty() {
        return;
    }
    let sql = sql.trim().trim_end_matches(';').trim().to_string();
    let sql = transform(sql);

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
            )
        })
    };
    let (conn_id, db_name, conn_label, connected) = match connection {
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
    let result_view =
        show_result_in_pane(&pane, conn_id, format!("{conn_label} — Results").into(), window, cx);
    result_view.update(cx, |view, cx| view.set_loading(cx));
    workspace.open_panel::<TerminalPanel>(window, cx);

    cx.spawn_in(window, async move |_workspace, cx| {
        // Auto-connect if the database is not connected (covers both a fresh
        // session and a dropped connection).
        if !connected {
            let connect = store.update(cx, |store, cx| store.connect(conn_id, cx));
            if let Err(err) = connect.await {
                result_view.update(cx, |view, cx| {
                    view.set_error(format!("Could not connect to '{conn_label}': {err}"), cx);
                });
                return anyhow::Ok(());
            }
        }

        // The result view owns fetching from here on: it pages the statement in
        // chunks and fills the grid, with its own Stop control.
        let store = store.downgrade();
        result_view.update(cx, |view, cx| {
            view.run_sql(store, conn_id, db_name.clone(), sql.clone(), cx);
        });
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
    collapsed_folders: HashSet<String>,
    views_expanded: HashSet<(ConnectionId, String)>,
    table_indexes_expanded: HashSet<(ConnectionId, String, String)>,
    table_fks_expanded: HashSet<(ConnectionId, String, String)>,
    table_triggers_expanded: HashSet<(ConnectionId, String, String)>,
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
                    collapsed_folders: HashSet::default(),
                    views_expanded: HashSet::default(),
                    table_indexes_expanded: HashSet::default(),
                    table_fks_expanded: HashSet::default(),
                    table_triggers_expanded: HashSet::default(),
                    _subscriptions: vec![store_subscription, workspace_subscription, filter_subscription],
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

    fn open_edit_connection_modal(&self, existing: ConnectionConfig, window: &mut Window, cx: &mut Context<Self>) {
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

    fn toggle_folder_collapsed(&mut self, folder: &str, cx: &mut Context<Self>) {
        if !self.collapsed_folders.remove(folder) {
            self.collapsed_folders.insert(folder.to_string());
        }
        cx.notify();
    }

    fn render_folder_header(
        &self,
        folder: SharedString,
        is_collapsed: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let folder_for_click = folder.to_string();
        div()
            .id(ElementId::from(SharedString::from(format!(
                "folder-header-{folder}"
            ))))
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .cursor_pointer()
            .hover(|style| style.bg(gpui::transparent_white()))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_folder_collapsed(&folder_for_click, cx);
            }))
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
                .color(Color::Muted),
            )
            .child(Label::new(folder).size(LabelSize::Small))
    }

    fn open_sql_query_with_text(workspace: WeakEntity<Workspace>, connection_id: ConnectionId, text: String, window: &mut Window, cx: &mut Context<Self>) {
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
                            ed.register_addon(DbQueryEditorAddon { connection_id });
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
                        IconButton::new("add-connection", IconName::Plus)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Add Connection"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_add_connection_modal(window, cx);
                            })),
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
                                                Self::open_sql_query_with_text(workspace.clone(), id, sql, window, cx);
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
                                    let table_idx_data = table_indexes.get(&tbl_key).cloned().unwrap_or_default();
                                    let table_fk_data = table_fks.get(&tbl_key).cloned().unwrap_or_default();
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
                                                if event.modifiers().control && event.click_count() == 1 {
                                                    let ddl_task = this.store.update(cx, |store, cx| {
                                                        store.get_table_ddl(id, db.clone(), tbl.clone(), cx)
                                                    });
                                                    let ws = workspace.clone();
                                                    cx.spawn_in(window, async move |this, cx| {
                                                        let ddl = ddl_task.await?;
                                                        this.update_in(cx, |_, window, cx| {
                                                            Self::open_sql_query_with_text(ws.clone(), id, ddl, window, cx);
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
                                                .child(Label::new(table_name.clone()).size(LabelSize::Small))
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
                                                            Self::open_sql_query_with_text(ws.clone(), id, ddl, window, cx);
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
                                                    Self::open_sql_query_with_text(workspace.clone(), id, sql, window, cx);
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
                                                                Self::open_sql_query_with_text(workspace.clone(), id, sql, window, cx);
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
                                                                        Self::open_sql_query_with_text(ws.clone(), id, ddl, window, cx);
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
                                                                Self::open_sql_query_with_text(workspace.clone(), id, sql, window, cx);
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
                                                                Self::open_sql_query_with_text(workspace.clone(), id, sql, window, cx);
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
                                                                Self::open_sql_query_with_text(workspace.clone(), id, sql, window, cx);
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
                                                                Self::open_sql_query_with_text(workspace.clone(), id, sql, window, cx);
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
                                                                Self::open_sql_query_with_text(workspace.clone(), id, sql, window, cx);
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
                                                                Self::open_sql_query_with_text(workspace.clone(), id, sql, window, cx);
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
                                                                                this.update_in(cx, |_, window, cx| {
                                                                                    Self::open_sql_query_with_text(ws.clone(), id, ddl, window, cx);
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
                                                                                "SELECT * FROM `{}`.`{}` LIMIT 200",
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
            })
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
                        move |_, _, window, cx| {
                            Self::open_sql_query_with_text(workspace.clone(), history_conn_id, query.clone(), window, cx);
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

        let (top_level, folders) = group_connections_by_folder(&connections);

        let mut tree = v_flex().flex_col();
        for index in top_level {
            if let Some(conn) = connections.get(index) {
                tree = tree.child(self.render_connection_item(conn.clone(), cx));
            }
        }
        for (folder, indices) in folders {
            let is_collapsed = self.collapsed_folders.contains(&folder);
            tree = tree.child(self.render_folder_header(folder.into(), is_collapsed, cx));
            if is_collapsed {
                continue;
            }
            let mut group = v_flex().flex_col().pl(px(12.));
            for index in indices {
                if let Some(conn) = connections.get(index) {
                    group = group.child(self.render_connection_item(conn.clone(), cx));
                }
            }
            tree = tree.child(group);
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
                    .child(tree),
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

    fn connection_with(label: &str, folder: Option<&str>) -> ActiveConnection {
        let config = db_client::ConnectionConfig {
            label: label.to_string(),
            folder: folder.map(|f| f.to_string()),
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
    fn group_connections_splits_top_level_and_folders() {
        let connections = vec![
            connection_with("Beta", None),
            connection_with("alpha", None),
            connection_with("Staging DB", Some("Work")),
            connection_with("Prod DB", Some("Work")),
            connection_with("Scratch", Some("Personal")),
        ];

        let (top_level, folders) = super::group_connections_by_folder(&connections);

        // Top level is sorted case-insensitively by label.
        let top_labels: Vec<&str> = top_level
            .iter()
            .map(|i| connections[*i].config.label.as_str())
            .collect();
        assert_eq!(top_labels, vec!["alpha", "Beta"]);

        // Folders are returned sorted by name.
        let folder_names: Vec<&str> = folders.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(folder_names, vec!["Personal", "Work"]);

        // Connections inside a folder are sorted by label.
        let work = &folders
            .iter()
            .find(|(name, _)| name == "Work")
            .expect("Work folder must be present")
            .1;
        let work_labels: Vec<&str> = work
            .iter()
            .map(|i| connections[*i].config.label.as_str())
            .collect();
        assert_eq!(work_labels, vec!["Prod DB", "Staging DB"]);
    }

    #[test]
    fn group_connections_treats_blank_folder_as_top_level() {
        let connections = vec![
            connection_with("Only", Some("   ")),
            connection_with("Other", None),
        ];

        let (top_level, folders) = super::group_connections_by_folder(&connections);

        assert_eq!(top_level.len(), 2, "blank folder names fall back to top level");
        assert!(folders.is_empty());
    }

    #[test]
    fn group_connections_trims_folder_names() {
        let connections = vec![
            connection_with("A", Some("  Work  ")),
            connection_with("B", Some("Work")),
        ];

        let (top_level, folders) = super::group_connections_by_folder(&connections);

        assert!(top_level.is_empty());
        assert_eq!(folders.len(), 1, "padded and exact folder names must merge");
        assert_eq!(folders[0].0, "Work");
        assert_eq!(folders[0].1.len(), 2);
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
        async fn list_tables(
            &self,
            _database: &str,
        ) -> anyhow::Result<Vec<db_client::TableInfo>> {
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
        async fn get_table_ddl(
            &self,
            _database: &str,
            _table: &str,
        ) -> anyhow::Result<String> {
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
                    views_expanded: HashSet::default(),
                    table_indexes_expanded: HashSet::default(),
                    table_fks_expanded: HashSet::default(),
                    table_triggers_expanded: HashSet::default(),
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
        assert!(connected, "connection must be established before running the query");

        let editor = workspace.update_in(cx, |workspace, window, cx| {
            let buffer = cx.new(|cx| language::Buffer::local("SELECT 1 AS one", cx));
            let multi = cx.new(|cx| MultiBuffer::singleton(buffer, cx));
            let editor = cx.new(|cx| {
                let mut editor = Editor::for_multibuffer(multi, None, window, cx);
                editor.register_addon(DbQueryEditorAddon { connection_id });
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

        let pane = terminal_panel
            .read_with(cx, |panel, _| panel.pane())
            .expect("terminal panel must have a pane");
        let result = pane.read_with(cx, |pane, cx| {
            pane.items_of_type::<crate::result_view::ResultView>()
                .next()
                .and_then(|view| view.read(cx).result.clone())
        });

        let result =
            result.expect("Ctrl+Enter must execute the query and open a results table below");
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
            show_result_in_pane(&pane, conn_a, "A — Results".into(), window, cx)
        });
        let view_a2 = workspace.update_in(cx, |_, window, cx| {
            show_result_in_pane(&pane, conn_a, "A — Results".into(), window, cx)
        });
        let view_b = workspace.update_in(cx, |_, window, cx| {
            show_result_in_pane(&pane, conn_b, "B — Results".into(), window, cx)
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
            pane.items_of_type::<crate::result_view::ResultView>().count()
        });
        assert_eq!(
            tab_count, 2,
            "two connections must produce exactly two result tabs"
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
                editor.register_addon(DbQueryEditorAddon {
                    connection_id: uuid::Uuid::new_v4(),
                });
                editor
            });
            workspace.add_item_to_active_pane(
                Box::new(editor.clone()),
                None,
                true,
                window,
                cx,
            );
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
                workspace.register_action(
                    |workspace, _: &RunQuery, window, cx| {
                        run_current_sql_query(workspace, window, cx);
                    },
                );
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
                    views_expanded: HashSet::default(),
                    table_indexes_expanded: HashSet::default(),
                    table_fks_expanded: HashSet::default(),
                    table_triggers_expanded: HashSet::default(),
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
                cx.spawn_in(window, async move |workspace_handle, cx: &mut AsyncWindowContext| {
                    DatabasePanel::load(workspace_handle, cx.clone()).await
                })
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
