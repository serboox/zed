use db_client::ConnectionId;
use editor::Editor;
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, SharedString,
    WeakEntity, Window, prelude::*, px,
};
use ui::prelude::*;
use ui::{Button, ButtonStyle, IconName, Label, LabelSize};
use util::ResultExt;
use workspace::{Item, Workspace, item::ItemEvent};

use crate::result_view::{ResultView, format_query_error};
use crate::store::DatabaseStore;

/// Upper bound on records fetched by a single Scan, mirroring
/// `MAX_RESULT_ROWS`'s role for SQL grids — Aerospike sets can hold many
/// millions of records, and Scan has no server-side row estimate up front.
const SCAN_ROW_LIMIT: usize = 500;

/// Parses the Put form's `bin=value, bin2=value2` shorthand into ordered
/// bin/value pairs. Every value is sent as a string bin — Aerospike's typed
/// bins (int/float/list/map) aren't exposed by this form; use a real client
/// for anything beyond simple string values.
fn parse_bins_shorthand(text: &str) -> Result<Vec<(String, String)>, String> {
    let mut bins = Vec::new();
    for part in text.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let Some((name, value)) = part.split_once('=') else {
            return Err(format!("Expected \"bin=value\", got \"{part}\""));
        };
        let name = name.trim();
        if name.is_empty() {
            return Err(format!("Empty bin name in \"{part}\""));
        }
        bins.push((name.to_string(), value.trim().to_string()));
    }
    if bins.is_empty() {
        return Err("Enter at least one bin=value pair.".to_string());
    }
    Ok(bins)
}

/// Distinguishes a completed action from a validation/execution failure, so
/// the status line's colour and icon reflect what actually happened instead
/// of always reading as an error.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AerospikeStatusKind {
    Error,
    Success,
}

/// A form-based console for Aerospike, which has no query language: records
/// are addressed by namespace/set/key (Get/Put) or swept with a Scan,
/// mirroring how the server itself (and `asinfo`/`aql`) models access.
pub struct AerospikeView {
    focus_handle: FocusHandle,
    store: Entity<DatabaseStore>,
    workspace: WeakEntity<Workspace>,
    connection_id: ConnectionId,
    connection_label: SharedString,
    namespace_editor: Entity<Editor>,
    set_editor: Entity<Editor>,
    key_editor: Entity<Editor>,
    put_bins_editor: Entity<Editor>,
    status: Option<(SharedString, AerospikeStatusKind)>,
    is_running: bool,
    get_result: Option<Vec<(String, String)>>,
    get_result_missing: bool,
}

impl AerospikeView {
    pub fn new(
        store: Entity<DatabaseStore>,
        workspace: WeakEntity<Workspace>,
        connection_id: ConnectionId,
        connection_label: SharedString,
        default_namespace: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let namespace_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("namespace", window, cx);
            if !default_namespace.is_empty() {
                editor.set_text(default_namespace, window, cx);
            }
            editor
        });
        let set_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("set", window, cx);
            editor
        });
        let key_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("key", window, cx);
            editor
        });
        let put_bins_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("bin=value, bin2=value2", window, cx);
            editor
        });
        Self {
            focus_handle: cx.focus_handle(),
            store,
            workspace,
            connection_id,
            connection_label,
            namespace_editor,
            set_editor,
            key_editor,
            put_bins_editor,
            status: None,
            is_running: false,
            get_result: None,
            get_result_missing: false,
        }
    }

    fn namespace(&self, cx: &App) -> String {
        self.namespace_editor.read(cx).text(cx).trim().to_string()
    }

    fn set(&self, cx: &App) -> String {
        self.set_editor.read(cx).text(cx).trim().to_string()
    }

    fn key(&self, cx: &App) -> String {
        self.key_editor.read(cx).text(cx).trim().to_string()
    }

    /// Validates namespace/set are filled in before any of the three
    /// actions runs; Get/Put additionally require a key.
    fn require_namespace_and_set(&mut self, cx: &mut Context<Self>) -> Option<(String, String)> {
        let (namespace, set) = (self.namespace(cx), self.set(cx));
        if namespace.is_empty() || set.is_empty() {
            self.status = Some((
                "Namespace and set are required.".into(),
                AerospikeStatusKind::Error,
            ));
            cx.notify();
            return None;
        }
        Some((namespace, set))
    }

    fn parse_put_bins(&self, cx: &App) -> Result<Vec<(String, String)>, String> {
        parse_bins_shorthand(&self.put_bins_editor.read(cx).text(cx))
    }

    fn run_get(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((namespace, set)) = self.require_namespace_and_set(cx) else {
            return;
        };
        let key = self.key(cx);
        if key.is_empty() {
            self.status = Some((
                "Key is required for Get.".into(),
                AerospikeStatusKind::Error,
            ));
            cx.notify();
            return;
        }

        self.status = None;
        self.get_result = None;
        self.get_result_missing = false;
        self.is_running = true;
        cx.notify();

        let connection_id = self.connection_id;
        let task = self.store.update(cx, |store, cx| {
            store.get_record(connection_id, namespace, set, key, cx)
        });
        cx.spawn_in(window, async move |this, cx| {
            let outcome = task.await;
            this.update(cx, |view, cx| {
                view.is_running = false;
                match outcome {
                    Ok(Some(bins)) => view.get_result = Some(bins),
                    Ok(None) => view.get_result_missing = true,
                    Err(error) => {
                        view.status = Some((
                            format_query_error(&error).into(),
                            AerospikeStatusKind::Error,
                        ))
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn run_put(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((namespace, set)) = self.require_namespace_and_set(cx) else {
            return;
        };
        let key = self.key(cx);
        if key.is_empty() {
            self.status = Some((
                "Key is required for Put.".into(),
                AerospikeStatusKind::Error,
            ));
            cx.notify();
            return;
        }
        let bins = match self.parse_put_bins(cx) {
            Ok(bins) => bins,
            Err(error) => {
                self.status = Some((error.into(), AerospikeStatusKind::Error));
                cx.notify();
                return;
            }
        };

        self.status = None;
        self.get_result = None;
        self.get_result_missing = false;
        self.is_running = true;
        cx.notify();

        let connection_id = self.connection_id;
        let task = self.store.update(cx, |store, cx| {
            store.put_record(connection_id, namespace, set, key, bins, cx)
        });
        cx.spawn_in(window, async move |this, cx| {
            let outcome = task.await;
            this.update(cx, |view, cx| {
                view.is_running = false;
                view.status = Some(match outcome {
                    Ok(()) => ("Put succeeded.".into(), AerospikeStatusKind::Success),
                    Err(error) => (
                        format_query_error(&error).into(),
                        AerospikeStatusKind::Error,
                    ),
                });
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn run_scan(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((namespace, set)) = self.require_namespace_and_set(cx) else {
            return;
        };

        self.status = None;
        self.get_result = None;
        self.get_result_missing = false;
        self.is_running = true;
        cx.notify();

        let connection_id = self.connection_id;
        let title = SharedString::from(format!("Scan: {namespace}.{set}"));
        let task = self.store.update(cx, |store, cx| {
            store.scan_records(connection_id, namespace, set, SCAN_ROW_LIMIT, cx)
        });
        let store_weak = self.store.downgrade();
        let env_color = crate::panel::connection_env_color(&store_weak, connection_id, cx);
        let result_view = cx.new(|cx| ResultView::new(title, cx).with_env_color(env_color));
        let rv = result_view.clone();
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |this, cx| {
            let outcome = task.await;
            this.update(cx, |view, cx| {
                view.is_running = false;
                cx.notify();
            })
            .ok();
            rv.update(cx, |view, cx| match outcome {
                Ok(result) => view.set_result(result, cx),
                Err(error) => view.set_error(format_query_error(&error), cx),
            });
            workspace
                .update_in(cx, |workspace, window, cx| {
                    workspace.add_item_to_active_pane(
                        Box::new(result_view),
                        None,
                        true,
                        window,
                        cx,
                    );
                })
                .log_err();
        })
        .detach();
    }
}

impl EventEmitter<DismissEvent> for AerospikeView {}

impl Item for AerospikeView {
    type Event = DismissEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        format!("Aerospike: {}", self.connection_label).into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::DatabaseZap))
    }

    fn to_item_events(_event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(ItemEvent::CloseItem);
    }
}

impl Focusable for AerospikeView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for AerospikeView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let get_rows = self.get_result.as_ref().map(|bins| {
            v_flex()
                .id("aerospike-get-result")
                .gap_1()
                .p_2()
                .children(bins.iter().map(|(name, value)| {
                    h_flex()
                        .gap_2()
                        .child(
                            Label::new(name.clone())
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .child(Label::new(value.clone()).size(LabelSize::Small))
                }))
        });

        v_flex()
            .id("aerospike-view")
            .size_full()
            .p_2()
            .gap_2()
            .bg(cx.theme().colors().editor_background)
            .child(
                Label::new(format!("Aerospike — {}", self.connection_label))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .w(px(160.))
                            .debug_selector(|| "AEROSPIKE_NAMESPACE".to_string())
                            .child(self.namespace_editor.clone()),
                    )
                    .child(
                        div()
                            .w(px(160.))
                            .debug_selector(|| "AEROSPIKE_SET".to_string())
                            .child(self.set_editor.clone()),
                    )
                    .child(
                        div()
                            .w(px(200.))
                            .debug_selector(|| "AEROSPIKE_KEY".to_string())
                            .child(self.key_editor.clone()),
                    ),
            )
            .child(
                h_flex().gap_2().child(
                    div()
                        .flex_1()
                        .debug_selector(|| "AEROSPIKE_PUT_BINS".to_string())
                        .child(self.put_bins_editor.clone()),
                ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div().debug_selector(|| "aerospike-get".to_string()).child(
                            Button::new("aerospike-get", "Get")
                                .style(ButtonStyle::Filled)
                                .disabled(self.is_running)
                                .on_click(
                                    cx.listener(|view, _, window, cx| view.run_get(window, cx)),
                                ),
                        ),
                    )
                    .child(
                        div().debug_selector(|| "aerospike-put".to_string()).child(
                            Button::new("aerospike-put", "Put")
                                .style(ButtonStyle::Filled)
                                .disabled(self.is_running)
                                .on_click(
                                    cx.listener(|view, _, window, cx| view.run_put(window, cx)),
                                ),
                        ),
                    )
                    .child(
                        Button::new("aerospike-scan", "Scan")
                            .style(ButtonStyle::Filled)
                            .disabled(self.is_running)
                            .on_click(cx.listener(|view, _, window, cx| view.run_scan(window, cx))),
                    ),
            )
            .when_some(self.status.clone(), |el, (status, kind)| {
                let (icon, color) = match kind {
                    AerospikeStatusKind::Success => (IconName::Check, Color::Success),
                    AerospikeStatusKind::Error => (IconName::XCircle, Color::Error),
                };
                el.child(
                    h_flex()
                        .gap_1()
                        .items_center()
                        .debug_selector(|| "AEROSPIKE_STATUS".to_string())
                        .child(Icon::new(icon).color(color).size(IconSize::Small))
                        .child(Label::new(status).size(LabelSize::Small).color(color)),
                )
            })
            .when(self.get_result_missing, |el| {
                el.child(
                    div()
                        .debug_selector(|| "AEROSPIKE_GET_MISSING".to_string())
                        .child(
                            Label::new("No record found for this key.")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                )
            })
            .children(get_rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::{TestAppContext, VisualTestContext};
    use project::Project;
    use settings::SettingsStore;
    use std::sync::Arc;
    use workspace::MultiWorkspace;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            crate::init(cx);
        });
    }

    fn debug_center(cx: &mut VisualTestContext, selector: &'static str) -> gpui::Point<Pixels> {
        cx.debug_bounds(selector)
            .unwrap_or_else(|| panic!("expected debug bounds for {selector}"))
            .center()
    }

    /// A record store that always reports the key as missing on Get and
    /// always succeeds on Put -- enough to drive the real Get-then-Put
    /// sequence below without a live Aerospike cluster.
    struct MissingThenPutOkProvider;

    #[async_trait::async_trait]
    impl db_client::provider::DbProvider for MissingThenPutOkProvider {
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
            Err(anyhow::anyhow!("not used in this test"))
        }
        async fn get_table_ddl(&self, _database: &str, _table: &str) -> anyhow::Result<String> {
            Ok(String::new())
        }
        async fn get_record(
            &self,
            _namespace: &str,
            _set: &str,
            _key: &str,
        ) -> anyhow::Result<Option<Vec<(String, String)>>> {
            Ok(None)
        }
        async fn put_record(
            &self,
            _namespace: &str,
            _set: &str,
            _key: &str,
            _bins: &[(String, String)],
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    // Fails against the pre-fix `run_put` (which never clears
    // `get_result_missing`): after Get reports the key missing, a
    // subsequent Put must clear that stale "No record found" state rather
    // than leaving it displayed alongside the new Put status.
    #[gpui::test]
    async fn putting_a_record_clears_a_stale_missing_get_result(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs.clone(), [], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let mut cx = VisualTestContext::from_window(window.into(), cx);

        let store = cx.new(DatabaseStore::new);
        let config = db_client::ConnectionConfig {
            label: "aerospike-test".to_string(),
            read_only: false,
            ..Default::default()
        };
        let connection_id = config.id;
        store.update(&mut cx, |store, cx| {
            store.add_connected_for_test(config, Arc::new(MissingThenPutOkProvider), cx);
        });

        let view = cx.add_window(|window, cx| {
            AerospikeView::new(
                store.clone(),
                workspace.downgrade(),
                connection_id,
                "aerospike-test".into(),
                "ns".to_string(),
                window,
                cx,
            )
        });
        let mut cx = VisualTestContext::from_window(view.into(), &mut cx);
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });

        view.update(&mut cx, |view, window, cx| {
            view.set_editor.update(cx, |editor, cx| {
                editor.set_text("myset", window, cx);
            });
            view.key_editor.update(cx, |editor, cx| {
                editor.set_text("mykey", window, cx);
            });
            view.put_bins_editor.update(cx, |editor, cx| {
                editor.set_text("name=Alice", window, cx);
            });
        })
        .unwrap();

        cx.run_until_parked();

        let get_target = debug_center(&mut cx, "aerospike-get");
        cx.simulate_click(get_target, gpui::Modifiers::none());
        cx.run_until_parked();

        view.read_with(&cx, |view, _| {
            assert!(
                view.get_result_missing,
                "Get on a nonexistent key should report it missing"
            );
        })
        .unwrap();

        let put_target = debug_center(&mut cx, "aerospike-put");
        cx.simulate_click(put_target, gpui::Modifiers::none());
        cx.run_until_parked();

        view.read_with(&cx, |view, _| {
            assert!(
                !view.get_result_missing,
                "Put must clear a stale missing-Get result rather than leaving it displayed"
            );
            assert!(view.get_result.is_none());
            assert!(matches!(
                &view.status,
                Some((message, AerospikeStatusKind::Success)) if message == "Put succeeded."
            ));
        })
        .unwrap();
    }

    #[test]
    fn parse_bins_shorthand_splits_comma_separated_pairs() {
        assert_eq!(
            parse_bins_shorthand("name=Ada, age=30"),
            Ok(vec![
                ("name".to_string(), "Ada".to_string()),
                ("age".to_string(), "30".to_string()),
            ])
        );
    }

    #[test]
    fn parse_bins_shorthand_ignores_blank_segments() {
        assert_eq!(
            parse_bins_shorthand("name=Ada, , age=30,"),
            Ok(vec![
                ("name".to_string(), "Ada".to_string()),
                ("age".to_string(), "30".to_string()),
            ])
        );
    }

    #[test]
    fn parse_bins_shorthand_rejects_a_pair_with_no_equals_sign() {
        assert_eq!(
            parse_bins_shorthand("name"),
            Err("Expected \"bin=value\", got \"name\"".to_string())
        );
    }

    #[test]
    fn parse_bins_shorthand_rejects_an_empty_bin_name() {
        assert_eq!(
            parse_bins_shorthand("=value"),
            Err("Empty bin name in \"=value\"".to_string())
        );
    }

    #[test]
    fn parse_bins_shorthand_rejects_entirely_empty_input() {
        assert_eq!(
            parse_bins_shorthand(""),
            Err("Enter at least one bin=value pair.".to_string())
        );
        assert_eq!(
            parse_bins_shorthand("   "),
            Err("Enter at least one bin=value pair.".to_string())
        );
    }
}
