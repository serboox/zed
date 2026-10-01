use std::time::Duration;

use db_client::connection::ConnectionId;
use gpui::{App, Context, Entity, Subscription, Task, WeakEntity, Window, prelude::*};
use ui::{
    Button, Color, ContextMenu, Icon, IconName, IconSize, PopoverMenu, PopoverMenuHandle, Tooltip,
    cyberpunk, prelude::*,
};
use util::ResultExt as _;
use workspace::notifications::NotificationId;
use workspace::{StatusItemView, Toast, Workspace, item::ItemHandle};

use crate::query_runs::RunningQuery;
use crate::store::{DatabaseStore, DatabaseStoreEvent};

/// What the Stop control promises, for tooltips. The same words everywhere, so
/// the reader learns once what pressing it does.
pub const STOP_TOOLTIP: &str =
    "Stop the statement on the server. An open transaction is rolled back.";

/// Stops what `connection` is running and tells the reader how it ended.
pub fn stop_and_tell(
    connection: ConnectionId,
    workspace: Option<WeakEntity<Workspace>>,
    cx: &mut App,
) {
    let Some(store) = DatabaseStore::global(cx) else {
        return;
    };
    let Some(stop) = store.update(cx, |store, cx| store.stop_queries(connection, cx)) else {
        return;
    };
    cx.spawn(async move |cx| {
        let Ok(report) = stop.await else {
            return;
        };
        let Some(workspace) = workspace else {
            return;
        };
        workspace
            .update(cx, |workspace, cx| {
                workspace.show_toast(
                    Toast::new(
                        NotificationId::named("db-query-stopped".into()),
                        report.describe(),
                    ),
                    cx,
                )
            })
            .log_err();
    })
    .detach();
}

/// Stops everything every connection is running.
pub fn stop_everything_and_tell(workspace: Option<WeakEntity<Workspace>>, cx: &mut App) {
    let Some(store) = DatabaseStore::global(cx) else {
        return;
    };
    let mut connections: Vec<ConnectionId> = store
        .read(cx)
        .running_queries()
        .iter()
        .map(|query| query.connection)
        .collect();
    connections.sort();
    connections.dedup();
    for connection in connections {
        stop_and_tell(connection, workspace.clone(), cx);
    }
}

/// How long a statement has been running, in the units a reader thinks in.
pub fn elapsed_text(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    match seconds {
        0..=59 => format!("{seconds} s"),
        60..=3599 => format!("{} min {} s", seconds / 60, seconds % 60),
        _ => format!("{} h {} min", seconds / 3600, (seconds % 3600) / 60),
    }
}

/// One line for the list of what is running.
fn running_line(query: &RunningQuery, connection_label: &str) -> String {
    format!(
        "{connection_label} — {} · {}",
        query.summary,
        elapsed_text(query.started.elapsed())
    )
}

/// The workspace's status bar item that shows what the database client is
/// running, wherever the reader is looking, and stops it.
///
/// Absent while nothing runs, so it costs nothing the rest of the time.
pub struct QueryStatusIndicator {
    // The store is a lazily created app global that can postdate this item, so
    // it is bound on first render, like the item that shows script runs.
    store: Option<Entity<DatabaseStore>>,
    popover_handle: PopoverMenuHandle<ContextMenu>,
    workspace: Option<WeakEntity<Workspace>>,
    ticker: Option<Task<()>>,
    _subscription: Option<Subscription>,
}

/// How often the elapsed time is brought up to date.
const TICK: Duration = Duration::from_secs(1);

impl QueryStatusIndicator {
    pub fn new(workspace: WeakEntity<Workspace>, _cx: &mut Context<Self>) -> Self {
        Self {
            store: None,
            popover_handle: PopoverMenuHandle::default(),
            workspace: Some(workspace),
            ticker: None,
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
        let subscription = cx.subscribe(&store, |this, _, event, cx| {
            if matches!(event, DatabaseStoreEvent::QueriesChanged) {
                this.keep_ticking_while_something_runs(cx);
                cx.notify();
            }
        });
        self.store = Some(store);
        self._subscription = Some(subscription);
        self.keep_ticking_while_something_runs(cx);
    }

    fn keep_ticking_while_something_runs(&mut self, cx: &mut Context<Self>) {
        let Some(store) = self.store.clone() else {
            return;
        };
        if self.ticker.is_some() || store.read(cx).running_queries().is_empty() {
            return;
        }
        self.ticker = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(TICK).await;
                let keep_going = this
                    .update(cx, |this, cx| {
                        cx.notify();
                        let running = this
                            .store
                            .as_ref()
                            .is_some_and(|store| !store.read(cx).running_queries().is_empty());
                        if !running {
                            this.ticker = None;
                        }
                        running
                    })
                    .unwrap_or(false);
                if !keep_going {
                    break;
                }
            }
        }));
    }
}

impl Render for QueryStatusIndicator {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_bound_to_store(cx);
        let Some(store) = self.store.clone() else {
            return div().into_any_element();
        };
        let queries = store.read(cx).running_queries();
        if queries.is_empty() {
            return div().into_any_element();
        }

        let longest = queries
            .iter()
            .map(|query| query.started.elapsed())
            .max()
            .unwrap_or_default();
        let stopping = queries.iter().all(|query| query.stopping);
        let label = match queries.len() {
            1 => format!("1 query · {}", elapsed_text(longest)),
            many => format!("{many} queries · {}", elapsed_text(longest)),
        };
        let workspace = self.workspace.clone();
        let lines: Vec<(ConnectionId, String)> = queries
            .iter()
            .map(|query| {
                let label = store
                    .read(cx)
                    .connections()
                    .iter()
                    .find(|connection| connection.config.id == query.connection)
                    .map(|connection| connection.config.label.clone())
                    .unwrap_or_default();
                (query.connection, running_line(query, &label))
            })
            .collect();

        div()
            .debug_selector(|| "query-status-indicator".to_string())
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .child(
                PopoverMenu::new("query-status-popover")
                    .menu(move |window, cx| {
                        let lines = lines.clone();
                        let workspace = workspace.clone();
                        Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                            for (connection, line) in lines {
                                let workspace = workspace.clone();
                                menu = menu.entry(format!("Stop · {line}"), None, move |_, cx| {
                                    stop_and_tell(connection, workspace.clone(), cx);
                                });
                            }
                            menu
                        }))
                    })
                    .with_handle(self.popover_handle.clone())
                    .trigger(
                        Button::new("query-status-trigger", label)
                            .style(cyberpunk::Rank::Quiet.style())
                            .start_icon(Icon::new(IconName::ArrowCircle).size(IconSize::XSmall)),
                    ),
            )
            .child(
                div()
                    .debug_selector(|| "query-status-stop".to_string())
                    .child(
                        Button::new(
                            "query-status-stop",
                            if stopping { "Stopping…" } else { "Stop" },
                        )
                        .style(cyberpunk::Rank::Destructive.style())
                        .start_icon(
                            Icon::new(IconName::Stop)
                                .size(IconSize::XSmall)
                                .color(Color::Error),
                        )
                        .disabled(stopping)
                        .tooltip(Tooltip::text(STOP_TOOLTIP))
                        .on_click(move |_, window, cx| {
                            stop_everything_and_tell(
                                Workspace::for_window(window, cx)
                                    .map(|workspace| workspace.downgrade()),
                                cx,
                            );
                        }),
                    ),
            )
            .into_any_element()
    }
}

impl StatusItemView for QueryStatusIndicator {
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
    fn elapsed_time_is_written_in_the_largest_units_that_say_something() {
        assert_eq!(elapsed_text(Duration::from_secs(0)), "0 s");
        assert_eq!(elapsed_text(Duration::from_secs(59)), "59 s");
        assert_eq!(elapsed_text(Duration::from_secs(60)), "1 min 0 s");
        assert_eq!(elapsed_text(Duration::from_secs(125)), "2 min 5 s");
        assert_eq!(elapsed_text(Duration::from_secs(3600)), "1 h 0 min");
        assert_eq!(
            elapsed_text(Duration::from_secs(3 * 3600 + 25 * 60 + 9)),
            "3 h 25 min"
        );
    }

    struct Frame(Entity<QueryStatusIndicator>);

    impl Render for Frame {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .w(gpui::px(600.))
                .h(gpui::px(40.))
                .child(self.0.clone())
        }
    }

    fn draw(cx: &mut gpui::VisualTestContext) {
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn the_status_item_is_absent_until_something_runs_and_then_stops_it(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::query_runs::test_support::HangingProvider;
        use crate::store::GlobalDatabaseStore;

        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let provider = HangingProvider::new(true, false);
        let config = db_client::ConnectionConfig::default();
        let connection = config.id;
        let store = cx.new(DatabaseStore::new);
        cx.update(|cx| cx.set_global(GlobalDatabaseStore(store.clone())));
        store.update(cx, |store, cx| {
            store.add_connected_for_test(config, provider.clone(), cx);
        });
        let window = cx.add_window(|_window, cx| {
            Frame(cx.new(|cx| QueryStatusIndicator::new(WeakEntity::new_invalid(), cx)))
        });
        let mut cx = gpui::VisualTestContext::from_window(window.into(), cx);
        draw(&mut cx);
        assert!(
            cx.debug_bounds("query-status-indicator").is_none(),
            "nothing runs, so the bar shows nothing"
        );

        let statement = store.update(&mut cx, |store, cx| {
            store.execute_query(connection, "shop".into(), "SELECT SLEEP(60)".into(), cx)
        });
        cx.run_until_parked();
        draw(&mut cx);
        assert!(
            cx.debug_bounds("query-status-indicator").is_some(),
            "a statement runs, so the bar says so"
        );

        let stop = cx
            .debug_bounds("query-status-stop")
            .expect("the bar has the way to stop it")
            .center();
        cx.simulate_click(stop, gpui::Modifiers::none());
        for _ in 0..5 {
            cx.run_until_parked();
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(150));
        }
        cx.run_until_parked();

        let error = statement
            .await
            .expect_err("a stopped statement is an error");
        assert!(crate::query_runs::is_cancelled(&error), "{error:#}");
        assert_eq!(
            *provider
                .interrupts
                .lock()
                .expect("the lock is not poisoned"),
            [db_client::interrupt::Interrupt::Statement]
        );
        draw(&mut cx);
        assert!(
            cx.debug_bounds("query-status-indicator").is_none(),
            "the statement is gone, and so is the item"
        );
    }
}
