use std::sync::Arc;

use db_client::connection::ConnectionId;
use editor::Editor;
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, SharedString, Window,
    prelude::*, px,
};
use ui::{
    Button, ButtonStyle, Icon, IconName, Label, LabelSize, PopoverMenu, PopoverMenuHandle,
    cyberpunk, prelude::*,
};
use util::ResultExt;
use workspace::{ModalView, StatusItemView, item::ItemHandle};

use crate::store::{DatabaseStore, DatabaseStoreEvent};

/// Splits `text` into individual SQL statements using the same quote/comment
/// aware boundary detection as the query console's statement-at-cursor
/// resolution, so a heavy multi-statement script is never split mid-string.
pub(crate) fn split_sql_statements(text: &str) -> Vec<String> {
    crate::panel::statement_runs_in_range(text, 0..text.len())
        .into_iter()
        .map(|run| run.sql)
        .collect()
}

/// One statement's outcome within an [`ExecJob`], recorded so the detail
/// popover can show exactly which statements failed and why.
#[derive(Clone, Debug)]
pub struct ExecStatementOutcome {
    pub index: usize,
    pub error: Option<String>,
}

/// Tracks a single "Exec" run (a pasted or file-loaded multi-statement
/// script) executed sequentially against one connection. Cancellation is
/// checked between statements only -- a statement already in flight always
/// runs to completion, mirroring the same limitation `table_copy`'s
/// cancel-mid-batch accepts for the same reason (no per-statement cancel
/// token is exposed by `DbProvider`).
#[derive(Clone, Debug)]
pub struct ExecJob {
    pub id: usize,
    pub connection_id: ConnectionId,
    pub label: SharedString,
    pub total: usize,
    pub completed: usize,
    pub failed: usize,
    pub cancelled: bool,
    pub done: bool,
    pub outcomes: Vec<ExecStatementOutcome>,
}

impl ExecJob {
    pub fn is_running(&self) -> bool {
        !self.done && !self.cancelled
    }
}

impl DatabaseStore {
    /// Splits `sql_text` into statements and runs them sequentially against
    /// `id`/`database`, updating the returned job's progress as each
    /// statement finishes. Returns `None` (no job started) for a
    /// whitespace-only script.
    pub fn start_exec_job(
        &mut self,
        id: ConnectionId,
        database: String,
        label: SharedString,
        sql_text: String,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        let statements = split_sql_statements(&sql_text);
        if statements.is_empty() {
            return None;
        }
        let job_id = self.next_exec_job_id;
        self.next_exec_job_id += 1;
        self.exec_jobs.push(ExecJob {
            id: job_id,
            connection_id: id,
            label,
            total: statements.len(),
            completed: 0,
            failed: 0,
            cancelled: false,
            done: false,
            outcomes: Vec::new(),
        });
        cx.emit(DatabaseStoreEvent::ExecJobsChanged);
        cx.notify();

        cx.spawn(async move |this, cx| {
            for (index, statement) in statements.into_iter().enumerate() {
                let cancelled = this
                    .read_with(cx, |store, _| {
                        store
                            .exec_jobs
                            .iter()
                            .find(|job| job.id == job_id)
                            .map(|job| job.cancelled)
                            .unwrap_or(true)
                    })
                    .unwrap_or(true);
                if cancelled {
                    break;
                }

                let task = this
                    .update(cx, |store, cx| {
                        store.execute_query(id, database.clone(), statement, cx)
                    })
                    .ok();
                let Some(task) = task else { break };
                let result = task.await;

                let updated = this
                    .update(cx, |store, cx| {
                        let Some(job) = store.exec_jobs.iter_mut().find(|job| job.id == job_id)
                        else {
                            return false;
                        };
                        job.completed += 1;
                        if let Err(error) = result {
                            job.failed += 1;
                            job.outcomes.push(ExecStatementOutcome {
                                index,
                                error: Some(error.to_string()),
                            });
                        }
                        cx.emit(DatabaseStoreEvent::ExecJobsChanged);
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !updated {
                    break;
                }
            }
            this.update(cx, |store, cx| {
                if let Some(job) = store.exec_jobs.iter_mut().find(|job| job.id == job_id) {
                    job.done = true;
                }
                cx.emit(DatabaseStoreEvent::ExecJobsChanged);
                cx.notify();
            })
            .ok();
        })
        .detach();

        Some(job_id)
    }

    pub fn cancel_exec_job(&mut self, job_id: usize, cx: &mut Context<Self>) {
        if let Some(job) = self.exec_jobs.iter_mut().find(|job| job.id == job_id) {
            job.cancelled = true;
        }
        cx.emit(DatabaseStoreEvent::ExecJobsChanged);
        cx.notify();
    }

    pub fn dismiss_exec_job(&mut self, job_id: usize, cx: &mut Context<Self>) {
        self.exec_jobs.retain(|job| job.id != job_id);
        cx.emit(DatabaseStoreEvent::ExecJobsChanged);
        cx.notify();
    }
}

/// Invoked when the Exec dialog is confirmed with the script text to run.
pub type ExecRunCallback = Arc<dyn Fn(String, &mut Window, &mut App)>;

/// A modal offering a pasted or file-loaded multi-statement SQL script to run
/// against a connection, kept entirely separate from the connection's
/// persistent query console file.
pub struct ExecDialog {
    focus_handle: FocusHandle,
    connection_label: SharedString,
    sql_editor: Entity<Editor>,
    on_run: Option<ExecRunCallback>,
}

pub enum ExecDialogEvent {
    Dismissed,
}

impl ExecDialog {
    pub fn new(
        connection_label: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let sql_editor = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.set_placeholder_text(
                "Paste a heavy or multi-statement SQL script, or load one from a file…",
                window,
                cx,
            );
            editor
        });
        Self {
            focus_handle: cx.focus_handle(),
            connection_label,
            sql_editor,
            on_run: None,
        }
    }

    pub fn on_run(mut self, callback: ExecRunCallback) -> Self {
        self.on_run = Some(callback);
        self
    }

    /// Replaces the editor's text with `text` (called after a file is loaded
    /// from disk), so the dialog can preview/edit it before running.
    pub fn set_text(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        self.sql_editor.update(cx, |editor, cx| {
            editor.set_text(text, window, cx);
        });
    }

    pub fn text(&self, cx: &App) -> String {
        self.sql_editor.read(cx).text(cx)
    }

    fn load_from_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let path_rx = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: None,
        });
        cx.spawn_in(window, async move |this, cx| {
            let Some(path) = path_rx
                .await
                .log_err()
                .and_then(|result| result.log_err())
                .flatten()
                .and_then(|paths| paths.into_iter().next())
            else {
                return;
            };
            let text = cx
                .background_executor()
                .spawn(async move { std::fs::read_to_string(&path) })
                .await;
            let Some(text) = text.log_err() else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                this.set_text(text, window, cx);
            })
            .ok();
        })
        .detach();
    }
}

impl EventEmitter<ExecDialogEvent> for ExecDialog {}
impl EventEmitter<DismissEvent> for ExecDialog {}
impl ModalView for ExecDialog {}

impl Focusable for ExecDialog {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ExecDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::widgets::dialog_surface(cx)
            .track_focus(&self.focus_handle)
            .key_context("ExecDialog")
            .w(px(720.))
            .max_h(px(560.))
            .p_4()
            .gap_3()
            .flex()
            .flex_col()
            .child(crate::widgets::dialog_header(
                format!("Exec on {}…", self.connection_label),
                "exec-dialog-close",
                cx.listener(|_, _, _, cx| {
                    cx.emit(ExecDialogEvent::Dismissed);
                    cx.emit(DismissEvent);
                }),
                cx,
            ))
            .child(
                Label::new(
                    "Runs a script of one or more statements sequentially, separately from \
                     this connection's SQL Queries console.",
                )
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .child(
                div()
                    .flex_1()
                    .min_h(px(240.))
                    .rounded_none()
                    .border_1()
                    .border_color(cyberpunk::border_dim())
                    .bg(cyberpunk::surface())
                    .p_2()
                    .child(self.sql_editor.clone()),
            )
            .child(
                h_flex()
                    .w_full()
                    .justify_between()
                    .gap_2()
                    .child(
                        Button::new("exec-load-file", "Load from file…")
                            .style(cyberpunk::Rank::Quiet.style())
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.load_from_file(window, cx);
                            })),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("exec-cancel", "Cancel")
                                    .style(cyberpunk::Rank::Neutral.style())
                                    .on_click(cx.listener(|_, _, _, cx| {
                                        cx.emit(ExecDialogEvent::Dismissed);
                                        cx.emit(DismissEvent);
                                    })),
                            )
                            .child(
                                Button::new("exec-run", "Run")
                                    .style(ButtonStyle::OutlinedCustom(
                                        cyberpunk::Accent::Cyan.border(),
                                    ))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        let text = this.sql_editor.read(cx).text(cx);
                                        if let Some(callback) = this.on_run.clone() {
                                            callback(text, window, cx);
                                        }
                                        cx.emit(DismissEvent);
                                    })),
                            ),
                    ),
            )
    }
}

/// Global status-bar indicator for background "Exec" jobs, mirroring
/// `activity_indicator`'s always-present-but-usually-empty pattern: renders
/// nothing when there are no jobs, otherwise a small clickable summary that
/// opens a popover listing every job with its progress and a Cancel/Dismiss
/// action.
pub struct ExecStatusIndicator {
    // The Database Explorer's `DatabaseStore` is a lazily-initialized app
    // global (set only once the panel itself loads), which can postdate this
    // indicator's own construction in the status bar -- so the binding is
    // acquired lazily on first render, not required up front.
    store: Option<Entity<DatabaseStore>>,
    popover_handle: PopoverMenuHandle<ui::ContextMenu>,
    _subscription: Option<gpui::Subscription>,
}

impl ExecStatusIndicator {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        Self {
            store: None,
            popover_handle: PopoverMenuHandle::default(),
            _subscription: None,
        }
    }

    fn ensure_bound_to_store(&mut self, cx: &mut Context<Self>) {
        if self.store.is_some() {
            return;
        }
        let Some(store) = DatabaseStore::global(cx) else {
            return;
        };
        let subscription = cx.subscribe(&store, |_, _, event, cx| {
            if matches!(event, DatabaseStoreEvent::ExecJobsChanged) {
                cx.notify();
            }
        });
        self.store = Some(store);
        self._subscription = Some(subscription);
    }
}

impl Render for ExecStatusIndicator {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_bound_to_store(cx);
        let Some(store) = self.store.clone() else {
            return div().into_any_element();
        };
        let jobs = store.read(cx).exec_jobs.clone();
        if jobs.is_empty() {
            return div().into_any_element();
        }

        let running = jobs.iter().filter(|job| job.is_running()).count();
        let summary = if running > 0 {
            let job = jobs.iter().find(|job| job.is_running()).expect("running");
            format!("Exec {}/{}", job.completed, job.total)
        } else {
            let failed: usize = jobs.iter().map(|job| job.failed).sum();
            if failed > 0 {
                format!("Exec: {failed} failed")
            } else {
                "Exec: done".to_string()
            }
        };

        div()
            .debug_selector(|| "exec-status-indicator".to_string())
            .child(
                PopoverMenu::new("exec-status-popover")
                    .menu(move |window, cx| {
                        let store = store.clone();
                        let jobs = store.read(cx).exec_jobs.clone();
                        Some(ui::ContextMenu::build(window, cx, move |mut menu, _, _| {
                            for job in jobs {
                                let label = if job.done {
                                    format!(
                                        "{} — {}/{} done, {} failed",
                                        job.label, job.completed, job.total, job.failed
                                    )
                                } else {
                                    format!(
                                        "{} — {}/{} running…",
                                        job.label, job.completed, job.total
                                    )
                                };
                                let store_for_entry = store.clone();
                                let job_id = job.id;
                                let done = job.done;
                                menu = menu.entry(label, None, move |_, cx| {
                                    store_for_entry.update(cx, |store, cx| {
                                        if done {
                                            store.dismiss_exec_job(job_id, cx);
                                        } else {
                                            store.cancel_exec_job(job_id, cx);
                                        }
                                    });
                                });
                            }
                            menu
                        }))
                    })
                    .with_handle(self.popover_handle.clone())
                    .trigger(
                        Button::new("exec-status-trigger", summary)
                            .style(cyberpunk::Rank::Quiet.style())
                            .start_icon(
                                Icon::new(if running > 0 {
                                    IconName::ArrowCircle
                                } else {
                                    IconName::Check
                                })
                                .size(IconSize::XSmall),
                            ),
                    ),
            )
            .into_any_element()
    }
}

impl StatusItemView for ExecStatusIndicator {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _: &App) -> Option<workspace::HideStatusItem> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_sql_statements_separates_a_multi_statement_script() {
        let text = "INSERT INTO t VALUES (1);\nUPDATE t SET a = 1 WHERE id = 1;\n";
        let statements = split_sql_statements(text);
        assert_eq!(
            statements,
            vec![
                "INSERT INTO t VALUES (1)".to_string(),
                "UPDATE t SET a = 1 WHERE id = 1".to_string(),
            ]
        );
    }

    #[test]
    fn split_sql_statements_ignores_semicolons_inside_string_literals() {
        let text = "INSERT INTO t (name) VALUES ('a;b');\nSELECT 1;";
        let statements = split_sql_statements(text);
        assert_eq!(
            statements,
            vec![
                "INSERT INTO t (name) VALUES ('a;b')".to_string(),
                "SELECT 1".to_string(),
            ]
        );
    }

    #[test]
    fn split_sql_statements_is_empty_for_blank_input() {
        assert!(split_sql_statements("   \n\t  ").is_empty());
    }

    #[gpui::test]
    async fn start_exec_job_runs_every_statement_and_reports_failures(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::store::DatabaseStore;
        use db_client::provider::DbProvider;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingProvider {
            calls: Arc<AtomicUsize>,
        }

        #[async_trait::async_trait]
        impl DbProvider for CountingProvider {
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
            ) -> anyhow::Result<Vec<db_client::schema::ColumnInfo>> {
                Ok(Vec::new())
            }
            async fn execute_query(
                &self,
                _database: &str,
                sql: &str,
            ) -> anyhow::Result<db_client::schema::QueryResult> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                if sql.contains("FAIL") {
                    anyhow::bail!("simulated failure");
                }
                Ok(db_client::schema::QueryResult {
                    columns: Vec::new(),
                    rows: Vec::new(),
                    rows_affected: 1,
                    execution_time_ms: 0,
                    timing: None,
                    raw_documents: None,
                })
            }
            async fn get_table_ddl(&self, _database: &str, _table: &str) -> anyhow::Result<String> {
                Ok(String::new())
            }
        }

        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
        });

        let calls = Arc::new(AtomicUsize::new(0));
        let store = cx.new(DatabaseStore::new);
        let config = db_client::ConnectionConfig {
            label: "exec-test".to_string(),
            ..Default::default()
        };
        let connection_id = config.id;
        store.update(cx, |store, cx| {
            store.add_connected_for_test(
                config,
                Arc::new(CountingProvider {
                    calls: calls.clone(),
                }),
                cx,
            );
        });

        let job_id = store.update(cx, |store, cx| {
            store.start_exec_job(
                connection_id,
                "mydb".to_string(),
                "exec-test".into(),
                "INSERT INTO t VALUES (1);\nSELECT FAIL;\nSELECT 1;".to_string(),
                cx,
            )
        });
        assert!(job_id.is_some(), "a non-empty script must start a job");
        cx.run_until_parked();

        store.read_with(cx, |store, _| {
            let job = store
                .exec_jobs
                .iter()
                .find(|job| job.id == job_id.unwrap())
                .expect("job should still be tracked after completion");
            assert!(job.done, "job must be marked done once every statement ran");
            assert_eq!(job.total, 3);
            assert_eq!(
                job.completed, 3,
                "every statement must run even after a failure"
            );
            assert_eq!(job.failed, 1);
            assert_eq!(job.outcomes.len(), 1);
            assert_eq!(job.outcomes[0].index, 1);
        });
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[gpui::test]
    async fn cancel_exec_job_stops_before_the_next_statement(cx: &mut gpui::TestAppContext) {
        use crate::store::DatabaseStore;
        use db_client::provider::DbProvider;
        use std::sync::Arc;

        struct SlowProvider;

        #[async_trait::async_trait]
        impl DbProvider for SlowProvider {
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
            ) -> anyhow::Result<Vec<db_client::schema::ColumnInfo>> {
                Ok(Vec::new())
            }
            async fn execute_query(
                &self,
                _database: &str,
                _sql: &str,
            ) -> anyhow::Result<db_client::schema::QueryResult> {
                Ok(db_client::schema::QueryResult {
                    columns: Vec::new(),
                    rows: Vec::new(),
                    rows_affected: 1,
                    execution_time_ms: 0,
                    timing: None,
                    raw_documents: None,
                })
            }
            async fn get_table_ddl(&self, _database: &str, _table: &str) -> anyhow::Result<String> {
                Ok(String::new())
            }
        }

        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
        });

        let store = cx.new(DatabaseStore::new);
        let config = db_client::ConnectionConfig {
            label: "exec-cancel-test".to_string(),
            ..Default::default()
        };
        let connection_id = config.id;
        store.update(cx, |store, cx| {
            store.add_connected_for_test(config, Arc::new(SlowProvider), cx);
        });

        let job_id = store
            .update(cx, |store, cx| {
                store.start_exec_job(
                    connection_id,
                    "mydb".to_string(),
                    "exec-cancel-test".into(),
                    "SELECT 1;\nSELECT 2;\nSELECT 3;".to_string(),
                    cx,
                )
            })
            .unwrap();

        store.update(cx, |store, cx| {
            store.cancel_exec_job(job_id, cx);
        });
        cx.run_until_parked();

        store.read_with(cx, |store, _| {
            let job = store
                .exec_jobs
                .iter()
                .find(|job| job.id == job_id)
                .expect("cancelled job stays visible until dismissed");
            assert!(
                job.completed < job.total,
                "cancelling before the loop starts must stop it short of completion"
            );
        });
    }
}
