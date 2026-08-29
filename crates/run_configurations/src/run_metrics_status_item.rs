use std::time::Instant;

use gpui::{App, Context, Subscription, Task, WeakEntity, Window, prelude::*};
use settings::Settings;
use ui::{Tooltip, prelude::*};
use workspace::{HideStatusItem, StatusItemView, Workspace, item::ItemHandle};

use crate::process_metrics::{self, Metrics, Sample, Watcher};
use crate::run_configurations_settings::RunConfigurationsSettings;

/// The status-bar reading of what the project's running configuration is
/// using: CPU and memory, and the process count when there is more than one.
/// The rest -- PID, network, video memory -- is in a tooltip rather than
/// spelled out in the bar, and nothing at all is painted while nothing runs.
pub struct RunMetricsStatusItem {
    workspace: WeakEntity<Workspace>,
    metrics: Option<Metrics>,
    watcher: Watcher,
    /// Whether this window is the one in front. A poll nobody can see is a poll
    /// for nothing, so it stops the moment focus leaves this window and starts
    /// again the moment focus comes back.
    window_active: bool,
    _watching_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl RunMetricsStatusItem {
    /// Starts watching the project's running configuration the moment the
    /// status bar is built: a run already going should be reported at once,
    /// not a second after the bar is first drawn.
    pub fn new(workspace: &Workspace, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let subscriptions = vec![
            cx.observe_window_activation(window, Self::window_activation_changed),
            // Turning the reading off by setting should stop the poll right
            // away, not wait for the window to lose and regain focus first.
            cx.observe_global::<settings::SettingsStore>(|item, cx| item.watch_the_run(cx)),
        ];
        let mut item = Self {
            workspace: workspace.weak_handle(),
            metrics: None,
            watcher: Watcher::default(),
            window_active: window.is_window_active(),
            _watching_task: None,
            _subscriptions: subscriptions,
        };
        item.watch_the_run(cx);
        item
    }

    /// The window this item lives in gained or lost focus. Losing it is
    /// exactly the moment nobody can see the reading, so the poll is stopped
    /// along with it; gaining it back starts the poll again.
    fn window_activation_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.window_active = window.is_window_active();
        self.watch_the_run(cx);
    }

    /// Reads what the run is using, once a second, for as long as this item is
    /// on screen, its window has focus, and the reader has not turned the
    /// reading off. The reading itself happens off the drawing thread: `/proc`
    /// holds a few hundred files and none of that belongs in a frame.
    fn watch_the_run(&mut self, cx: &mut Context<Self>) {
        if !self.window_active || !RunConfigurationsSettings::get_global(cx).show_process_metrics {
            // Neither of these means the run itself stopped, but nobody can
            // see the reading right now, or the reader turned it off -- either
            // way it is not worth keeping stale numbers around for.
            self._watching_task = None;
            self.metrics = None;
            self.watcher.forget();
            return;
        }
        self._watching_task = Some(cx.spawn(async move |item, cx| {
            loop {
                let Ok(pid) = item.read_with(cx, |item, cx| item.process_of_a_run(cx)) else {
                    return;
                };
                let samples = match pid {
                    Some(_) => {
                        cx.background_spawn(async move { process_metrics::everything_running() })
                            .await
                    }
                    None => None,
                };
                let now = Instant::now();
                if item
                    .update(cx, |item, cx| {
                        if item.read_the_run(pid, samples.as_deref(), now) {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    return;
                }
                cx.background_executor().timer(Watcher::HOW_OFTEN).await;
            }
        }));
    }

    /// One reading. `samples` is every process the machine talked about, or
    /// nothing when it did not answer; `pid` is the run to look for among them.
    /// Says whether the reading changed.
    ///
    /// A run the machine has nothing to say about is over, and the reading
    /// says so. A machine that did not answer at all leaves the reading as it
    /// was, rather than reporting a running thing as gone.
    fn read_the_run(&mut self, pid: Option<u32>, samples: Option<&[Sample]>, now: Instant) -> bool {
        let Some(pid) = pid else {
            self.watcher.forget();
            return self.metrics.take().is_some();
        };
        let Some(samples) = samples else {
            return false;
        };
        let read = self.watcher.metrics_of(pid, samples, now);
        let changed = read != self.metrics;
        self.metrics = read;
        changed
    }

    /// The process a run of this project is going on in, if one is. The
    /// terminal panel holds the runs; a task terminal is one that was started
    /// from a task, which is what a configuration is.
    fn process_of_a_run(&self, cx: &App) -> Option<u32> {
        let workspace = self.workspace.upgrade()?;
        let panel = workspace
            .read(cx)
            .panel::<terminal_view::terminal_panel::TerminalPanel>(cx)?;
        let panel = panel.read(cx);
        let mut newest = None;
        for pane in panel.panes() {
            for item in pane.read(cx).items() {
                let Some(view) = item.downcast::<terminal_view::TerminalView>() else {
                    continue;
                };
                let terminal = view.read(cx).terminal().read(cx);
                if terminal.task().is_some()
                    && let Some(pid) = terminal.pid()
                {
                    newest = Some(pid.as_u32());
                }
            }
        }
        newest
    }
}

impl Render for RunMetricsStatusItem {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let Some(metrics) = self.metrics.clone() else {
            return div().into_any_element();
        };

        let said = |label: &'static str, value: String| {
            h_flex()
                .gap_1()
                .child(
                    Label::new(label)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .child(Label::new(value).size(LabelSize::XSmall))
        };

        let cpu = match metrics.cpu {
            Some(cpu) => format!("{cpu:.1}%"),
            None => "-- reading".to_string(),
        };
        let processes = metrics.processes;
        let pid = metrics.pid;
        let network = metrics.network;
        let video_memory = metrics.video_memory;

        h_flex()
            .id("run-metrics-status")
            .debug_selector(|| "run-metrics-status".to_string())
            .gap_2()
            .items_center()
            .child(said("CPU", cpu))
            .child(said("RAM", process_metrics::as_memory(metrics.memory)))
            .when(processes > 1, |row| {
                row.child(said("processes", processes.to_string()))
            })
            .tooltip(Tooltip::element(move |_window, _cx| {
                v_flex()
                    .gap_1()
                    .child(Label::new(format!("PID {pid}")).size(LabelSize::Small))
                    .child(
                        Label::new(match network {
                            Ok(bytes) => format!("network {}", process_metrics::as_memory(bytes)),
                            Err(why) => format!("network -- {why}"),
                        })
                        .size(LabelSize::Small),
                    )
                    .child(
                        Label::new(match video_memory {
                            Ok(bytes) => {
                                format!("video memory {}", process_metrics::as_memory(bytes))
                            }
                            Err(why) => format!("video memory -- {why}"),
                        })
                        .size(LabelSize::Small),
                    )
                    .into_any_element()
            }))
            .into_any_element()
    }
}

impl StatusItemView for RunMetricsStatusItem {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        // The reading is the project's, not the active tab's: it does not
        // change with whatever the reader has open.
    }

    fn hide_setting(&self, _cx: &App) -> Option<HideStatusItem> {
        Some(HideStatusItem::new(|settings| {
            settings
                .run_configurations
                .get_or_insert_default()
                .show_process_metrics = Some(false);
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use gpui::{Entity, TestAppContext, VisualTestContext};
    use project::{FakeFs, Project};
    use serde_json::json;
    use util::path;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            crate::init(cx);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
        });
    }

    fn draw(cx: &mut VisualTestContext) {
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        cx.run_until_parked();
    }

    /// A window with the item already sitting in its status bar, the same way
    /// `crates/zed/src/zed.rs` puts it there for a real window.
    async fn an_item_of_its_own(
        cx: &mut TestAppContext,
    ) -> (Entity<RunMetricsStatusItem>, VisualTestContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({ "src": { "main.rs": "" } }))
            .await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let item = workspace.update_in(cx, |workspace, window, cx| {
            let item = cx.new(|cx| RunMetricsStatusItem::new(workspace, window, cx));
            workspace.status_bar().update(cx, |status_bar, cx| {
                status_bar.add_left_item(item.clone(), window, cx);
            });
            item
        });
        cx.run_until_parked();
        (item, cx.clone())
    }

    fn a_sample(pid: u32) -> Sample {
        Sample {
            pid,
            parent: 1,
            ticks: 10,
            memory: 8 * 1024 * 1024,
            started: 5_000,
        }
    }

    /// The row that says what the project's running configuration is using
    /// must not clutter the status bar when nothing is running at all.
    #[gpui::test]
    async fn nothing_is_painted_when_nothing_is_running(cx: &mut TestAppContext) {
        let (_item, mut cx) = an_item_of_its_own(cx).await;
        draw(&mut cx);

        assert!(
            cx.debug_bounds("run-metrics-status").is_none(),
            "with nothing running there is nothing to show in the status bar"
        );
    }

    /// Once something is running, the reading shows up in the status bar.
    #[gpui::test]
    async fn the_reading_is_painted_when_something_is_running(cx: &mut TestAppContext) {
        let (item, mut cx) = an_item_of_its_own(cx).await;
        let watched = 4242;
        let running = [a_sample(watched)];

        item.update(&mut cx, |item, _| {
            assert!(item.read_the_run(Some(watched), Some(&running), Instant::now()));
        });
        draw(&mut cx);

        assert!(
            cx.debug_bounds("run-metrics-status").is_some(),
            "a running configuration shows its reading in the status bar"
        );
    }

    /// A machine that does not answer is not a run that has ended: an answer
    /// with no processes in it at all must leave the reading as it was rather
    /// than blank it out.
    #[gpui::test]
    async fn a_machine_that_does_not_answer_does_not_blank_an_existing_reading(
        cx: &mut TestAppContext,
    ) {
        let (item, mut cx) = an_item_of_its_own(cx).await;
        let watched = 4242;
        let running = [a_sample(watched)];
        let at = Instant::now();

        item.update(&mut cx, |item, _| {
            item.read_the_run(Some(watched), Some(&running), at);
        });
        draw(&mut cx);
        assert!(
            cx.debug_bounds("run-metrics-status").is_some(),
            "the reading is there while the run goes"
        );

        item.update(&mut cx, |item, _| {
            assert!(
                !item.read_the_run(Some(watched), None, at + Watcher::HOW_OFTEN),
                "nothing to report from a reading that did not happen"
            );
        });
        draw(&mut cx);
        assert!(
            cx.debug_bounds("run-metrics-status").is_some(),
            "the machine's silence must not blank an existing reading"
        );
    }

    /// The setting is what keeps the reading gone for good, unlike a window
    /// that merely lost focus: it stops the poll and clears what was shown.
    #[gpui::test]
    async fn turning_the_setting_off_paints_nothing(cx: &mut TestAppContext) {
        let (item, mut cx) = an_item_of_its_own(cx).await;
        let watched = 4242;
        let running = [a_sample(watched)];
        item.update(&mut cx, |item, _| {
            item.read_the_run(Some(watched), Some(&running), Instant::now());
        });
        draw(&mut cx);
        assert!(cx.debug_bounds("run-metrics-status").is_some());

        cx.update(|_, cx| {
            RunConfigurationsSettings::override_global(
                RunConfigurationsSettings {
                    show_process_metrics: false,
                    ..RunConfigurationsSettings::get_global(cx).clone()
                },
                cx,
            );
        });
        cx.run_until_parked();
        draw(&mut cx);

        assert!(
            cx.debug_bounds("run-metrics-status").is_none(),
            "turned off by setting, nothing is painted"
        );
        assert!(
            item.read_with(&cx, |item, _| item._watching_task.is_none()),
            "and the poll behind an invisible reading is not worth running either"
        );
    }

    /// A poll costs a reading a second, and that is only worth paying while
    /// somebody can actually see it. The window losing focus stops it, and
    /// getting focus back starts it again.
    #[gpui::test]
    async fn the_poll_stops_while_the_window_is_not_active(cx: &mut TestAppContext) {
        let (item, mut cx) = an_item_of_its_own(cx).await;
        assert!(
            item.read_with(&cx, |item, _| item._watching_task.is_some()),
            "the poll runs while the window has focus"
        );

        cx.deactivate_window();
        assert!(
            item.read_with(&cx, |item, _| item._watching_task.is_none()),
            "losing focus stops it -- nobody left to read the reading"
        );

        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        assert!(
            item.read_with(&cx, |item, _| item._watching_task.is_some()),
            "and getting focus back starts it again"
        );
    }
}
