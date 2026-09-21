use std::collections::VecDeque;
use std::time::Instant;

use gpui::{
    App, Bounds, Context, Hsla, PathBuilder, Point, Subscription, Task, WeakEntity, Window, canvas,
    fill, point, prelude::*, size,
};
use settings::Settings;
use ui::{ButtonLike, Tooltip, cyberpunk, prelude::*};
use workspace::{HideStatusItem, StatusItemView, Workspace, item::ItemHandle};

use crate::process_metrics::{self, Metrics, ProcessReading, Sample, Watcher};
use crate::run_configurations_settings::RunConfigurationsSettings;
use crate::run_metrics_modal::RunMetricsModal;

/// How many readings are kept for the charts: two minutes at
/// [`Watcher::HOW_OFTEN`], which is long enough to see a build ramp up and come
/// back down without turning the chart into a smear.
const READINGS_KEPT: usize = 120;

/// How tall one chart stands. It holds no text, so it may be told a height;
/// everything with words in it takes the height its words need.
const CHART_HEIGHT: Pixels = px(56.);

/// How wide the column of axis labels beside a chart is.
const AXIS_WIDTH: Pixels = px(34.);

/// How wide the column naming a process is, and how wide the column of values
/// after its bar is.
const PROCESS_NAME_WIDTH: Pixels = px(118.);
const PROCESS_VALUE_WIDTH: Pixels = px(58.);

/// How thick a bar and a chart's line are.
const BAR_HEIGHT: Pixels = px(6.);
const LINE_WIDTH: Pixels = px(1.5);

/// The status-bar plaque saying what the project's running configuration is
/// using: CPU and memory, and the process count when there is more than one.
/// Pressing it opens the reading in full -- the two minutes behind those
/// numbers, which of the run's processes hold the memory, and the facts that
/// have no time series. Nothing at all is painted while nothing runs.
pub struct RunMetricsStatusItem {
    workspace: WeakEntity<Workspace>,
    metrics: Option<Metrics>,
    /// The last [`READINGS_KEPT`] readings, oldest first. The charts draw only
    /// these, so a run a few seconds old draws a few seconds.
    readings: VecDeque<Reading>,
    watcher: Watcher,
    /// Whether this window is the one in front. A poll nobody can see is a poll
    /// for nothing, so it stops the moment focus leaves this window and starts
    /// again the moment focus comes back.
    window_active: bool,
    _watching_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

/// One reading, kept only for what the charts draw.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Reading {
    /// `None` for the first reading of a run, where a rate has nothing to be
    /// measured against yet.
    cpu: Option<f32>,
    memory: u64,
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
            readings: VecDeque::new(),
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
            self.readings.clear();
            self.watcher.forget();
            return;
        }
        self._watching_task = Some(cx.spawn(async move |item, cx| {
            loop {
                let Ok(pid) = item.read_with(cx, |item, cx| item.process_of_a_run(cx)) else {
                    return;
                };
                let read = match pid {
                    Some(_) => {
                        cx.background_spawn(async move {
                            (
                                process_metrics::everything_running(),
                                process_metrics::machine_uptime(),
                            )
                        })
                        .await
                    }
                    None => (None, None),
                };
                let (samples, machine_uptime) = read;
                let now = Instant::now();
                if item
                    .update(cx, |item, cx| {
                        if item.read_the_run(pid, samples.as_deref(), now, machine_uptime) {
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
    fn read_the_run(
        &mut self,
        pid: Option<u32>,
        samples: Option<&[Sample]>,
        now: Instant,
        machine_uptime: Option<std::time::Duration>,
    ) -> bool {
        let Some(pid) = pid else {
            self.watcher.forget();
            self.readings.clear();
            return self.metrics.take().is_some();
        };
        let Some(samples) = samples else {
            return false;
        };
        let read = self.watcher.metrics_of(pid, samples, now, machine_uptime);
        match &read {
            Some(metrics) => {
                if self.readings.len() >= READINGS_KEPT {
                    self.readings.pop_front();
                }
                self.readings.push_back(Reading {
                    cpu: metrics.cpu,
                    memory: metrics.memory,
                });
            }
            None => self.readings.clear(),
        }
        let changed = read != self.metrics;
        self.metrics = read;
        changed
    }

    /// Opens the reading in full, over the workspace this item's status bar
    /// belongs to.
    fn open_the_reading(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let item = cx.entity().downgrade();
        workspace.update(cx, |workspace, cx| {
            RunMetricsModal::open(workspace, item, window, cx);
        });
    }

    /// What the last reading said, for a window that draws it in full.
    pub(crate) fn reading(&self) -> Option<Metrics> {
        self.metrics.clone()
    }

    /// The readings the charts draw, oldest first: what the processor read and
    /// how much memory was held at each of them.
    pub(crate) fn series(&self) -> Vec<(Option<f32>, u64)> {
        self.readings
            .iter()
            .map(|reading| (reading.cpu, reading.memory))
            .collect()
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

/// A label and the value after it, the way both the plaque and the reading say
/// a fact that has no chart of its own.
pub(crate) fn said(label: &'static str, value: String) -> gpui::Div {
    h_flex()
        .gap_1()
        .child(
            Label::new(label)
                .size(LabelSize::XSmall)
                .color(Color::Muted),
        )
        .child(Label::new(value).size(LabelSize::XSmall))
}

/// What a number nobody could measure says instead of a zero, which would read
/// as "it is using none of this".
pub(crate) fn what_it_says(value: Result<u64, &'static str>) -> String {
    match value {
        Ok(bytes) => process_metrics::as_memory(bytes),
        Err(why) => format!("-- {why}"),
    }
}

/// Where each reading falls inside `bounds`, for a series whose top edge stands
/// for `ceiling`.
///
/// The step between readings is the whole window's rather than the readings'
/// own, so three readings sit at the left edge of the chart instead of being
/// spread across it: a chart that stretches what it has over the full width
/// claims to know two minutes of history it has never seen.
fn plotted(readings: &[(usize, f32)], ceiling: f32, bounds: Bounds<Pixels>) -> Vec<Point<Pixels>> {
    let steps = READINGS_KEPT.saturating_sub(1).max(1) as f32;
    let ceiling = match ceiling > 0. {
        true => ceiling,
        false => 1.,
    };
    readings
        .iter()
        .map(|(at, value)| {
            let across = (*at as f32 / steps).clamp(0., 1.);
            let up = (value / ceiling).clamp(0., 1.);
            point(
                bounds.origin.x + bounds.size.width * across,
                bounds.origin.y + bounds.size.height * (1. - up),
            )
        })
        .collect()
}

/// Every process of a reading, largest first, ties broken by pid so the order
/// does not shuffle between two readings that hold the same amount.
///
/// All of them and not a first few: the window they are drawn in is given a
/// height by the reader, and what that height is for is seeing the whole run.
pub(crate) fn by_memory(tree: &[ProcessReading]) -> Vec<ProcessReading> {
    let mut sorted = tree.to_vec();
    sorted.sort_by(|left, right| {
        right
            .memory
            .cmp(&left.memory)
            .then_with(|| left.pid.cmp(&right.pid))
    });
    sorted
}

/// One chart: what it is, what it reads right now, and the two minutes behind
/// that number.
///
/// The axis labels sit in a column beside the chart rather than inside it, so
/// that the chart alone carries the fixed height and no text is ever inside a
/// box that cannot grow for it.
pub(crate) fn a_chart(
    heading: &'static str,
    reading_now: String,
    readings: Vec<(usize, f32)>,
    ceiling: f32,
    hue: Hsla,
    axis: Vec<(f32, String)>,
) -> gpui::Div {
    let gridlines: Vec<f32> = axis.iter().map(|(at, _)| *at).collect();
    let grid = cyberpunk::border_dim();
    // The plot takes whatever height the caller gives the chart and keeps
    // `CHART_HEIGHT` as its floor. A fixed height here is what left a window
    // stretched to the editor drawing a 56px line under 500px of nothing.
    v_flex()
        .h_full()
        .gap(cyberpunk::SPACE_4)
        .child(
            h_flex()
                .flex_none()
                .justify_between()
                .items_end()
                .child(
                    Label::new(heading)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .child(Label::new(reading_now).size(LabelSize::Large)),
        )
        .child(
            h_flex()
                .flex_1()
                .min_h(CHART_HEIGHT)
                .items_stretch()
                .gap(cyberpunk::SPACE_4)
                .child(
                    div()
                        .relative()
                        .w(AXIS_WIDTH)
                        .flex_none()
                        .children(axis.into_iter().map(|(at, label)| {
                            div().absolute().right_0().top(relative(at)).child(
                                Label::new(label)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                        })),
                )
                .child(
                    div().flex_1().h_full().child(
                        canvas(
                            |_, _, _| {},
                            move |bounds: Bounds<Pixels>, _, window: &mut Window, _| {
                                for at in &gridlines {
                                    let across = point(
                                        bounds.origin.x,
                                        bounds.origin.y + bounds.size.height * *at,
                                    );
                                    window.paint_quad(fill(
                                        Bounds::new(across, size(bounds.size.width, px(1.))),
                                        grid,
                                    ));
                                }
                                let points = plotted(&readings, ceiling, bounds);
                                let floor = bounds.origin.y + bounds.size.height;
                                match points.split_first() {
                                    Some((first, rest)) if !rest.is_empty() => {
                                        let mut area = PathBuilder::fill();
                                        area.move_to(point(first.x, floor));
                                        area.line_to(*first);
                                        for next in rest {
                                            area.line_to(*next);
                                        }
                                        if let Some(last) = rest.last() {
                                            area.line_to(point(last.x, floor));
                                        }
                                        area.close();
                                        if let Ok(path) = area.build() {
                                            window.paint_path(path, hue.opacity(0.22));
                                        }
                                        let mut line = PathBuilder::stroke(LINE_WIDTH);
                                        line.move_to(*first);
                                        for next in rest {
                                            line.line_to(*next);
                                        }
                                        if let Ok(path) = line.build() {
                                            window.paint_path(path, hue);
                                        }
                                    }
                                    // One reading is a dot, not a line: there is
                                    // nothing yet for a line to go between.
                                    Some((only, _)) => {
                                        window.paint_quad(fill(
                                            Bounds::new(
                                                point(only.x, only.y - px(1.)),
                                                size(px(3.), px(3.)),
                                            ),
                                            hue,
                                        ));
                                    }
                                    None => {}
                                }
                            },
                        )
                        .size_full(),
                    ),
                ),
        )
}

/// One process of the run: what it is, how much of the largest one's memory it
/// holds, and that amount written out. The bar's place on the ramp says the
/// same thing the number does, so nothing here is carried by colour alone.
pub(crate) fn a_bar(name: String, memory: u64, largest: u64) -> gpui::Div {
    let fraction = match largest > 0 {
        true => (memory as f32 / largest as f32).clamp(0., 1.),
        false => 0.,
    };
    h_flex()
        .debug_selector({
            let name = name.clone();
            move || format!("MEMORY-BAR-{name}")
        })
        .gap(cyberpunk::SPACE_4)
        .child(
            div()
                .w(PROCESS_NAME_WIDTH)
                .flex_none()
                .overflow_hidden()
                .child(Label::new(name).size(LabelSize::XSmall).truncate()),
        )
        .child(
            div()
                .flex_1()
                .h(BAR_HEIGHT)
                .rounded(px(2.))
                .bg(cyberpunk::surface())
                .child(
                    div()
                        .h_full()
                        .w(relative(fraction))
                        .rounded(px(2.))
                        .bg(cyberpunk::ramp(fraction)),
                ),
        )
        .child(
            h_flex()
                .w(PROCESS_VALUE_WIDTH)
                .flex_none()
                .justify_end()
                .child(Label::new(process_metrics::as_memory(memory)).size(LabelSize::XSmall)),
        )
}

impl Render for RunMetricsStatusItem {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(metrics) = self.metrics.clone() else {
            return div().into_any_element();
        };

        let cpu = match metrics.cpu {
            Some(cpu) => format!("{cpu:.1}%"),
            None => "-- reading".to_string(),
        };
        let processes = metrics.processes;

        let plaque = ButtonLike::new("run-metrics-plaque")
            .style(cyberpunk::Rank::Quiet.style())
            .size(ButtonSize::Compact)
            .child(
                h_flex()
                    .debug_selector(|| "run-metrics-status".to_string())
                    .gap_2()
                    .items_center()
                    .child(said("CPU", cpu))
                    .child(said("RAM", process_metrics::as_memory(metrics.memory)))
                    .when(processes > 1, |row| {
                        row.child(said("processes", processes.to_string()))
                    }),
            );

        plaque
            .tooltip(Tooltip::text("Show what the run is using"))
            .on_click(cx.listener(|item, _, window, cx| item.open_the_reading(window, cx)))
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

    use gpui::{Entity, KeyBinding, Modifiers, MouseButton, TestAppContext, VisualTestContext};
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
            // The shipped keymap binds escape to `menu::Cancel` with no context
            // at all; a test app loads no keymap, so the same binding is put
            // there by hand rather than the action being dispatched directly.
            cx.bind_keys([KeyBinding::new("escape", menu::Cancel, None)]);
        });
    }

    fn draw(cx: &mut VisualTestContext) {
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        cx.run_until_parked();
    }

    /// The window asks to be focused two frames after it opens, and a test has
    /// no platform frame loop to deliver those frames -- so they are delivered
    /// by hand. Without this a keystroke lands nowhere and every way out of the
    /// reading looks broken.
    fn settle(cx: &mut VisualTestContext) {
        for _ in 0..3 {
            draw(cx);
            cx.update(|window, cx| {
                window.simulate_next_frame(cx);
            });
        }
        draw(cx);
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
        // A multi-workspace root rather than a bare workspace: the modal layer
        // the reading opens into is drawn there, and under a bare workspace the
        // window opens without being painted at all.
        let (multi, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi.read_with(cx, |multi, _| multi.workspace().clone());
        let item = workspace.update_in(cx, |workspace, window, cx| {
            let item = cx.new(|cx| RunMetricsStatusItem::new(workspace, window, cx));
            workspace.status_bar().update(cx, |status_bar, cx| {
                status_bar.add_right_item(item.clone(), window, cx);
            });
            item
        });
        cx.run_until_parked();
        (item, cx.clone())
    }

    fn a_sample(pid: u32) -> Sample {
        named(pid, "a program", 8 * 1024 * 1024)
    }

    fn named(pid: u32, name: &str, memory: u64) -> Sample {
        started_by(pid, 1, name, memory)
    }

    fn started_by(pid: u32, parent: u32, name: &str, memory: u64) -> Sample {
        Sample {
            pid,
            parent,
            name: name.into(),
            ticks: 10,
            memory,
            threads: 2,
            state: 'S',
            started: 5_000,
        }
    }

    fn a_process(pid: u32, memory: u64) -> ProcessReading {
        ProcessReading {
            pid,
            parent: 1,
            name: "a program".into(),
            memory,
            cpu: None,
            threads: 1,
            state: 'S',
            uptime: None,
        }
    }

    /// Reads a run into the item, then draws, so the plaque is on screen with
    /// painted bounds a click can be aimed at.
    fn a_run_on_screen(
        item: &Entity<RunMetricsStatusItem>,
        cx: &mut VisualTestContext,
        watched: u32,
    ) {
        let running = [a_sample(watched)];
        item.update(cx, |item, _| {
            item.read_the_run(Some(watched), Some(&running), Instant::now(), None);
        });
        draw(cx);
    }

    /// A run wide enough to fill a window somebody pulled taller: one shell and
    /// the many short-lived children a build spawns.
    fn a_wide_run_on_screen(item: &Entity<RunMetricsStatusItem>, cx: &mut VisualTestContext) {
        let megabyte = 1024 * 1024;
        let mut running = vec![started_by(4242, 1, "the shell", 8 * megabyte)];
        running
            .extend((1..=24u32).map(|which| {
                started_by(4242 + which, 4242, "a compiler", which as u64 * megabyte)
            }));
        item.update(cx, |item, _| {
            item.read_the_run(Some(4242), Some(&running), Instant::now(), None);
        });
        draw(cx);
    }

    /// A run of three: the shell, what it started, and what that started in
    /// turn, so the tree drawn from it has a shape worth reading.
    fn a_tree_on_screen(item: &Entity<RunMetricsStatusItem>, cx: &mut VisualTestContext) {
        let running = [
            started_by(4242, 1, "the shell", 4 * 1024 * 1024),
            started_by(4243, 4242, "the build", 64 * 1024 * 1024),
            started_by(4244, 4243, "a compiler", 32 * 1024 * 1024),
        ];
        item.update(cx, |item, _| {
            item.read_the_run(Some(4242), Some(&running), Instant::now(), None);
        });
        draw(cx);
    }

    fn press_the_plaque(cx: &mut VisualTestContext) {
        let plaque = cx
            .debug_bounds("run-metrics-status")
            .expect("the plaque is on screen");
        cx.simulate_click(plaque.center(), Modifiers::none());
        settle(cx);
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
            assert!(item.read_the_run(Some(watched), Some(&running), Instant::now(), None));
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
            item.read_the_run(Some(watched), Some(&running), at, None);
        });
        draw(&mut cx);
        assert!(
            cx.debug_bounds("run-metrics-status").is_some(),
            "the reading is there while the run goes"
        );

        item.update(&mut cx, |item, _| {
            assert!(
                !item.read_the_run(Some(watched), None, at + Watcher::HOW_OFTEN, None),
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
            item.read_the_run(Some(watched), Some(&running), Instant::now(), None);
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

    /// The plaque is an action, and pressing it opens the reading. A real press
    /// into the painted bounds, because a plaque that reads as a button and
    /// does nothing under the pointer is the whole failure being guarded here.
    #[gpui::test]
    async fn pressing_the_plaque_opens_the_reading(cx: &mut TestAppContext) {
        let (item, mut cx) = an_item_of_its_own(cx).await;
        a_run_on_screen(&item, &mut cx, 4242);

        assert!(
            cx.debug_bounds("RUN-METRICS-MODAL").is_none(),
            "the reading stays closed until it is asked for"
        );

        press_the_plaque(&mut cx);

        assert!(
            cx.debug_bounds("RUN-METRICS-MODAL").is_some(),
            "pressing the plaque opens the reading"
        );
    }

    /// Escape is the way out of the reading.
    #[gpui::test]
    async fn escape_closes_the_reading(cx: &mut TestAppContext) {
        let (item, mut cx) = an_item_of_its_own(cx).await;
        a_run_on_screen(&item, &mut cx, 4242);
        press_the_plaque(&mut cx);
        assert!(cx.debug_bounds("RUN-METRICS-MODAL").is_some());

        cx.simulate_keystrokes("escape");
        settle(&mut cx);

        assert!(
            cx.debug_bounds("RUN-METRICS-MODAL").is_none(),
            "escape closes the reading"
        );
    }

    /// And so is pressing anywhere else.
    #[gpui::test]
    async fn a_press_outside_closes_the_reading(cx: &mut TestAppContext) {
        let (item, mut cx) = an_item_of_its_own(cx).await;
        a_run_on_screen(&item, &mut cx, 4242);
        press_the_plaque(&mut cx);
        let reading = cx
            .debug_bounds("RUN-METRICS-MODAL")
            .expect("the reading is open");

        // Well clear of the reading, and of the plaque that opened it: pressing
        // the plaque again is a toggle rather than a press outside.
        let elsewhere = point((reading.origin.x - px(60.)).max(px(4.)), reading.center().y);
        assert!(
            !reading.contains(&elsewhere),
            "the press has to land outside the reading for this to prove anything"
        );
        cx.simulate_click(elsewhere, Modifiers::none());
        settle(&mut cx);

        assert!(
            cx.debug_bounds("RUN-METRICS-MODAL").is_none(),
            "a press outside the reading closes it"
        );
    }

    /// A chart draws what was sampled and no more. Three readings are three
    /// points at the left edge, not three points stretched over two minutes.
    #[gpui::test]
    fn three_readings_plot_three_points(_cx: &mut TestAppContext) {
        let chart = Bounds::new(point(px(0.), px(0.)), size(px(120.), px(60.)));
        let three = vec![(0usize, 10.), (1, 20.), (2, 30.)];

        let points = plotted(&three, 100., chart);

        assert_eq!(points.len(), 3, "three readings are three points");
        let last = points.last().expect("three points are there");
        let expected = chart.size.width * (2. / (READINGS_KEPT - 1) as f32);
        assert_eq!(
            last.x - chart.origin.x,
            expected,
            "the third reading sits two steps in, not at the right edge"
        );
        assert!(
            last.x < chart.origin.x + chart.size.width * 0.5,
            "three readings out of {READINGS_KEPT} stay near the left edge"
        );
    }

    /// A full window fills the chart, which is what makes the short window
    /// above mean something.
    #[gpui::test]
    fn a_full_window_reaches_the_right_edge(_cx: &mut TestAppContext) {
        let chart = Bounds::new(point(px(0.), px(0.)), size(px(120.), px(60.)));
        let full: Vec<(usize, f32)> = (0..READINGS_KEPT).map(|at| (at, 50.)).collect();

        let points = plotted(&full, 100., chart);

        assert_eq!(points.len(), READINGS_KEPT);
        let last = points.last().expect("a full window is there");
        assert_eq!(
            last.x,
            chart.origin.x + chart.size.width,
            "the newest of a full window is at the right edge"
        );
    }

    /// The list is ordered by what each process holds and leaves nothing out:
    /// a run's twelfth-largest process is a row like any other.
    #[gpui::test]
    fn every_process_is_listed_largest_first(_cx: &mut TestAppContext) {
        let megabyte = 1024 * 1024;
        let tree: Vec<ProcessReading> = (1..=12)
            .map(|which| a_process(which, which as u64 * megabyte))
            .collect();

        let rows = by_memory(&tree);

        assert_eq!(
            rows.iter().map(|one| one.pid).collect::<Vec<_>>(),
            vec![12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1],
            "largest first, and all twelve of them"
        );
    }

    /// Two processes holding the same amount keep the same order between two
    /// readings, or the list jitters once a second for no reason a reader can see.
    #[gpui::test]
    fn processes_holding_the_same_amount_keep_their_order(_cx: &mut TestAppContext) {
        let tree: Vec<ProcessReading> = [4243, 4241, 4242]
            .into_iter()
            .map(|pid| a_process(pid, 1024))
            .collect();

        let rows = by_memory(&tree);

        assert_eq!(
            rows.iter().map(|one| one.pid).collect::<Vec<_>>(),
            vec![4241, 4242, 4243],
            "the pid decides when what they hold does not"
        );
    }

    /// A number nobody could measure says why, because a zero there would read
    /// as "the run is using none of this".
    #[gpui::test]
    fn what_the_machine_will_not_say_gives_a_reason_rather_than_a_zero(_cx: &mut TestAppContext) {
        let said = what_it_says(Err("needs rights this editor does not ask for"));

        assert_eq!(said, "-- needs rights this editor does not ask for");
        assert!(
            !said.contains('0'),
            "a reason, not a zero, and not a zero with a reason after it"
        );
        assert_eq!(
            what_it_says(Ok(84 * 1024 * 1024)),
            "84 MB",
            "and a number the machine does give is just the number"
        );
    }

    /// The reading of a run whose network and video memory the machine will not
    /// report still opens, and still says what it does know.
    #[gpui::test]
    async fn the_reading_opens_for_a_machine_that_will_not_say(cx: &mut TestAppContext) {
        let (item, mut cx) = an_item_of_its_own(cx).await;
        a_run_on_screen(&item, &mut cx, 4242);
        item.read_with(&cx, |item, _| {
            let metrics = item.metrics.as_ref().expect("the run was read");
            assert!(
                metrics.network.is_err() && metrics.video_memory.is_err(),
                "this platform reports neither, which is what the row has to survive"
            );
        });

        press_the_plaque(&mut cx);

        assert!(
            cx.debug_bounds("RUN-METRICS-MODAL").is_some(),
            "the reading opens whether or not every fact could be measured"
        );
    }

    /// A window short enough that the reading cannot fit below the plaque must
    /// still show the whole reading, not the top of it.
    #[gpui::test]
    async fn the_reading_is_not_clipped_at_a_short_window(cx: &mut TestAppContext) {
        let (item, mut cx) = an_item_of_its_own(cx).await;
        cx.simulate_resize(size(px(640.), px(260.)));
        a_run_on_screen(&item, &mut cx, 4242);
        press_the_plaque(&mut cx);

        let reading = cx
            .debug_bounds("RUN-METRICS-MODAL")
            .expect("the reading is open");
        let viewport = cx.update(|window, _| window.viewport_size());

        assert!(
            reading.origin.x >= px(0.) && reading.origin.y >= px(0.),
            "the reading does not start off the top or the left of a {viewport:?} window: {reading:?}"
        );
        assert!(
            reading.right() <= viewport.width && reading.bottom() <= viewport.height,
            "and it does not run off the right or the bottom of a {viewport:?} window: {reading:?}"
        );
    }

    /// A window pulled taller has to spend the height on the reading. The
    /// charts take whatever the rows and the facts below them do not, so a
    /// reader who makes room for a build's shape gets a bigger shape and not a
    /// bigger blank.
    #[gpui::test]
    async fn a_window_pulled_taller_draws_a_taller_chart(cx: &mut TestAppContext) {
        let (item, mut cx) = an_item_of_its_own(cx).await;
        cx.simulate_resize(size(px(1200.), px(900.)));
        a_run_on_screen(&item, &mut cx, 4242);
        press_the_plaque(&mut cx);

        let before = cx
            .debug_bounds("RUN-METRICS-CHART-CPU")
            .expect("the window opens on the overview");
        let window_before = cx
            .debug_bounds("RUN-METRICS-MODAL")
            .expect("the window is open")
            .size
            .height;

        let grip = cx
            .debug_bounds("DIALOG-GRIP-Bottom")
            .expect("a window the reader resizes has an edge to pull");
        let from = grip.center();
        let to = point(from.x, from.y + PULLED_BY);
        cx.simulate_mouse_move(from, None, Modifiers::none());
        cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(to, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::none());
        settle(&mut cx);

        let window = cx
            .debug_bounds("RUN-METRICS-MODAL")
            .expect("the window is still open after the pull");
        let grew_by = window.size.height - window_before;
        assert!(
            grew_by > PULLED_BY / 2.,
            "the pull reached the window at all: it was {window_before:?} tall and is {:?}",
            window.size.height
        );

        let after = cx
            .debug_bounds("RUN-METRICS-CHART-CPU")
            .expect("the chart is still drawn");
        assert!(
            after.size.height > before.size.height * 2.,
            "the chart took the height it can use: it was {:?} and is {:?}, \
             while the window grew by {grew_by:?}",
            before.size.height,
            after.size.height
        );

        assert!(
            after.size.height <= CHART_MOST_DRAWN,
            "and it stops where a plot stops reading better for being taller: {:?}",
            after.size.height
        );
    }

    /// How far the pull in the test above drags the window's bottom edge.
    const PULLED_BY: Pixels = px(320.);

    /// The tallest a chart is drawn, with the heading above it counted in. The
    /// cap itself lives in the window; this is what it looks like from outside.
    const CHART_MOST_DRAWN: Pixels = px(260.);

    /// The charts stop growing, so a window pulled taller than they can use has
    /// to spend the rest on the processes -- every one of them, not the first
    /// few with the remainder summed into a row nobody can act on.
    #[gpui::test]
    async fn a_pulled_window_lists_every_process(cx: &mut TestAppContext) {
        let (item, mut cx) = an_item_of_its_own(cx).await;
        cx.simulate_resize(size(px(1200.), px(900.)));
        a_wide_run_on_screen(&item, &mut cx);
        press_the_plaque(&mut cx);

        let window = cx
            .debug_bounds("RUN-METRICS-MODAL")
            .expect("the reading is open");
        // Spelled out rather than built from the pids, because a selector is
        // matched by a `&'static str`. 4266 holds the most and 4243 the least
        // of the twenty-four; every one of them is a row now, where the list
        // used to stop at eight and sum the remainder into one.
        for row in [
            "MEMORY-BAR-4266 · a compiler",
            "MEMORY-BAR-4258 · a compiler",
            "MEMORY-BAR-4243 · a compiler",
        ] {
            assert!(
                cx.debug_bounds(row).is_some(),
                "every process of the run has a bar: {row} has none"
            );
        }

        // Having a row is not being on screen: the ninth-largest is the first
        // one the old list summed away, so a window this tall has to draw it
        // without the reader scrolling for it.
        let ninth = cx
            .debug_bounds("MEMORY-BAR-4258 · a compiler")
            .expect("the ninth-largest process has a bar");
        assert!(
            ninth.origin.y >= window.origin.y && ninth.bottom() <= window.bottom(),
            "the ninth row is drawn inside the window it was given: {ninth:?} in {window:?}"
        );
    }

    /// The window opens on the overview, and the second tab is where the run's
    /// processes are drawn as the tree they are. A real press on each, because
    /// a tab that looks chosen and shows the other half is the failure here.
    #[gpui::test]
    async fn the_processes_tab_draws_the_run_as_a_tree(cx: &mut TestAppContext) {
        let (item, mut cx) = an_item_of_its_own(cx).await;
        a_tree_on_screen(&item, &mut cx);
        press_the_plaque(&mut cx);

        assert!(
            cx.debug_bounds("RUN-METRICS-CHART-CPU").is_some(),
            "the window opens on the overview"
        );
        assert!(
            cx.debug_bounds("FORK-ROW-4243").is_none(),
            "and the tree is not drawn until it is asked for"
        );

        let tab = cx
            .debug_bounds("BUTTON-Processes")
            .expect("the second tab is there to press");
        cx.simulate_click(tab.center(), Modifiers::none());
        settle(&mut cx);

        for row in ["FORK-ROW-4242", "FORK-ROW-4243", "FORK-ROW-4244"] {
            assert!(
                cx.debug_bounds(row).is_some(),
                "every process of the run has a row of its own: {row} has none"
            );
        }
        assert!(
            cx.debug_bounds("RUN-METRICS-CHART-CPU").is_none(),
            "and the overview gives the window over to it"
        );
    }

    /// Depth is what the tree says, and it says it by where a row starts. A
    /// child stands in from its parent, and a grandchild further still.
    #[gpui::test]
    async fn a_child_stands_in_from_the_process_that_started_it(cx: &mut TestAppContext) {
        let (item, mut cx) = an_item_of_its_own(cx).await;
        a_tree_on_screen(&item, &mut cx);
        press_the_plaque(&mut cx);
        let tab = cx
            .debug_bounds("BUTTON-Processes")
            .expect("the second tab is there to press");
        cx.simulate_click(tab.center(), Modifiers::none());
        settle(&mut cx);

        let at = |name: &'static str, cx: &mut VisualTestContext| {
            cx.debug_bounds(name)
                .unwrap_or_else(|| panic!("{name} is drawn"))
                .origin
                .x
        };
        let root = at("FORK-NAME-4242", &mut cx);
        let child = at("FORK-NAME-4243", &mut cx);
        let grandchild = at("FORK-NAME-4244", &mut cx);

        assert!(
            child > root,
            "what the run started stands in from the run itself: {child:?} vs {root:?}"
        );
        assert!(
            grandchild > child,
            "and what that started stands in again: {grandchild:?} vs {child:?}"
        );
    }

    /// The window is worth widening: the two charts stand side by side while
    /// there is room for both, and fall into a column when there is not, rather
    /// than being squeezed to a width that draws nothing.
    #[gpui::test]
    async fn the_charts_stand_side_by_side_only_while_there_is_room(cx: &mut TestAppContext) {
        let (item, mut cx) = an_item_of_its_own(cx).await;
        cx.simulate_resize(size(px(1400.), px(900.)));
        a_run_on_screen(&item, &mut cx, 4242);
        press_the_plaque(&mut cx);

        let processor = cx
            .debug_bounds("RUN-METRICS-CHART-CPU")
            .expect("the processor chart is drawn");
        let memory = cx
            .debug_bounds("RUN-METRICS-CHART-MEMORY")
            .expect("the memory chart is drawn");
        assert_eq!(
            processor.origin.y, memory.origin.y,
            "in a wide window the two charts share a row"
        );
        assert!(
            memory.origin.x > processor.origin.x,
            "and the memory chart is the one on the right"
        );

        cx.simulate_resize(size(px(640.), px(900.)));
        settle(&mut cx);

        let processor = cx
            .debug_bounds("RUN-METRICS-CHART-CPU")
            .expect("the processor chart is still drawn");
        let memory = cx
            .debug_bounds("RUN-METRICS-CHART-MEMORY")
            .expect("the memory chart is still drawn");
        assert!(
            memory.origin.y >= processor.bottom(),
            "in a narrow window the memory chart drops below the processor one \
             rather than standing beside it: {memory:?} against {processor:?}"
        );
    }
}
