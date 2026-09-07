use std::collections::VecDeque;
use std::time::Instant;

use gpui::{
    Anchor, App, Bounds, Context, DismissEvent, EventEmitter, FocusHandle, Focusable, Hsla,
    MouseDownEvent, PathBuilder, Point, Subscription, Task, WeakEntity, Window, canvas, fill,
    point, prelude::*, size,
};
use settings::Settings;
use ui::{ButtonLike, PopoverMenu, Tooltip, cyberpunk, prelude::*};
use workspace::{HideStatusItem, StatusItemView, Workspace, item::ItemHandle};

use crate::process_metrics::{self, Metrics, ProcessMemory, Sample, Watcher};
use crate::run_configurations_settings::RunConfigurationsSettings;

/// How many readings are kept for the charts: two minutes at
/// [`Watcher::HOW_OFTEN`], which is long enough to see a build ramp up and come
/// back down without turning the chart into a smear.
const READINGS_KEPT: usize = 120;

/// How many processes get a bar of their own before the rest becomes one row of
/// their total. Past eight categorical rows a reader stops comparing them to
/// each other.
const PROCESSES_LISTED: usize = 8;

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
            self.readings.clear();
            return self.metrics.take().is_some();
        };
        let Some(samples) = samples else {
            return false;
        };
        let read = self.watcher.metrics_of(pid, samples, now);
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
fn said(label: &'static str, value: String) -> gpui::Div {
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
fn what_it_says(value: Result<u64, &'static str>) -> String {
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

/// The processes a reading lists, largest first, and how many it left out with
/// what they hold between them.
///
/// Past [`PROCESSES_LISTED`] rows a reader stops comparing them to each other,
/// so the rest becomes one row of their total rather than a list nobody reads
/// to the end.
fn listed(tree: &[ProcessMemory]) -> (Vec<ProcessMemory>, Option<(usize, u64)>) {
    let mut sorted = tree.to_vec();
    sorted.sort_by(|left, right| {
        right
            .memory
            .cmp(&left.memory)
            .then_with(|| left.pid.cmp(&right.pid))
    });
    if sorted.len() <= PROCESSES_LISTED {
        return (sorted, None);
    }
    let rest = sorted.split_off(PROCESSES_LISTED);
    let total = rest
        .iter()
        .fold(0u64, |sum, one| sum.saturating_add(one.memory));
    (sorted, Some((rest.len(), total)))
}

/// One chart: what it is, what it reads right now, and the two minutes behind
/// that number.
///
/// The axis labels sit in a column beside the chart rather than inside it, so
/// that the chart alone carries the fixed height and no text is ever inside a
/// box that cannot grow for it.
fn a_chart(
    heading: &'static str,
    reading_now: String,
    readings: Vec<(usize, f32)>,
    ceiling: f32,
    hue: Hsla,
    axis: Vec<(f32, String)>,
) -> gpui::Div {
    let gridlines: Vec<f32> = axis.iter().map(|(at, _)| *at).collect();
    let grid = cyberpunk::border_dim();
    v_flex()
        .gap(cyberpunk::SPACE_4)
        .child(
            h_flex()
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
                    div().flex_1().h(CHART_HEIGHT).child(
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
fn a_bar(name: String, memory: u64, largest: u64) -> gpui::Div {
    let fraction = match largest > 0 {
        true => (memory as f32 / largest as f32).clamp(0., 1.),
        false => 0.,
    };
    h_flex()
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

        let item = cx.entity().downgrade();
        PopoverMenu::new("run-metrics-reading-menu")
            .anchor(Anchor::BottomLeft)
            .menu(move |_window, cx| {
                let item = item.clone();
                Some(cx.new(|cx| RunMetricsReading::new(item, cx)))
            })
            .trigger_with_tooltip(plaque, Tooltip::text("Show what the run is using"))
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

/// The reading in full, opened from the plaque: the two minutes behind the
/// numbers in the status bar, which of the run's processes hold the memory, and
/// the facts that have no time series.
pub struct RunMetricsReading {
    item: WeakEntity<RunMetricsStatusItem>,
    focus_handle: FocusHandle,
    _observation: Option<Subscription>,
}

impl RunMetricsReading {
    fn new(item: WeakEntity<RunMetricsStatusItem>, cx: &mut Context<Self>) -> Self {
        let observation = item
            .upgrade()
            .map(|item| cx.observe(&item, |_, _, cx| cx.notify()));
        Self {
            item,
            focus_handle: cx.focus_handle(),
            _observation: observation,
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl Focusable for RunMetricsReading {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for RunMetricsReading {}

impl Render for RunMetricsReading {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let shell = v_flex()
            .id("run-metrics-reading")
            .debug_selector(|| "run-metrics-reading".to_string())
            .key_context("RunMetricsReading")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::cancel))
            .on_mouse_down_out(cx.listener(|_, _: &MouseDownEvent, _, cx| {
                cx.emit(DismissEvent);
            }))
            .elevation_2(cx)
            .w(px(320.))
            // A window can be shorter than the reading is tall, and a surface
            // that hangs past the bottom edge takes the last row -- the facts
            // -- with it. The reading gives way and scrolls instead.
            .max_h(vh(0.8, window))
            .overflow_y_scroll()
            .p(cyberpunk::SPACE_8)
            .gap(cyberpunk::SPACE_8);

        let Some(item) = self.item.upgrade() else {
            return shell.into_any_element();
        };
        let (metrics, readings) = {
            let item = item.read(cx);
            (
                item.metrics.clone(),
                item.readings.iter().copied().collect::<Vec<_>>(),
            )
        };
        let Some(metrics) = metrics else {
            return shell.into_any_element();
        };

        let processor = readings
            .iter()
            .enumerate()
            .filter_map(|(at, reading)| reading.cpu.map(|cpu| (at, cpu)))
            .collect();
        let held = readings
            .iter()
            .enumerate()
            .map(|(at, reading)| (at, reading.memory as f32))
            .collect::<Vec<_>>();
        let most_held = readings
            .iter()
            .fold(0u64, |most, reading| most.max(reading.memory));

        let (rows, the_rest) = listed(&metrics.tree);
        let largest = rows.first().map(|one| one.memory).unwrap_or(0);

        shell
            .child(a_chart(
                "Processor",
                match metrics.cpu {
                    Some(cpu) => format!("{cpu:.1}%"),
                    None => "-- reading".to_string(),
                },
                processor,
                100.,
                cyberpunk::series_processor(),
                vec![(0., "100".to_string()), (0.5, "50".to_string())],
            ))
            .child(a_chart(
                "Memory",
                process_metrics::as_memory(metrics.memory),
                held,
                most_held as f32,
                cyberpunk::series_memory(),
                vec![(0., process_metrics::as_memory(most_held))],
            ))
            .child(
                v_flex()
                    .gap(cyberpunk::SPACE_4)
                    .child(
                        Label::new("By process")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .children(rows.iter().map(|one| {
                        a_bar(format!("{} · {}", one.pid, one.name), one.memory, largest)
                    }))
                    .when_some(the_rest, |list, (count, total)| {
                        list.child(a_bar(format!("+{count} more"), total, largest))
                    }),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .gap(cyberpunk::SPACE_8)
                    .child(said("PID", metrics.pid.to_string()))
                    .child(said("processes", metrics.processes.to_string()))
                    .child(said("network", what_it_says(metrics.network)))
                    .child(said("video memory", what_it_says(metrics.video_memory))),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use gpui::{Entity, KeyBinding, Modifiers, TestAppContext, VisualTestContext};
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

    /// The popover asks to be focused two frames after it opens, and a test has
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
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
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
        Sample {
            pid,
            parent: 1,
            name: name.into(),
            ticks: 10,
            memory,
            started: 5_000,
        }
    }

    fn a_process(pid: u32, memory: u64) -> ProcessMemory {
        ProcessMemory {
            pid,
            name: "a program".into(),
            memory,
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
            item.read_the_run(Some(watched), Some(&running), Instant::now());
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

    /// The plaque is an action, and pressing it opens the reading. A real press
    /// into the painted bounds, because a plaque that reads as a button and
    /// does nothing under the pointer is the whole failure being guarded here.
    #[gpui::test]
    async fn pressing_the_plaque_opens_the_reading(cx: &mut TestAppContext) {
        let (item, mut cx) = an_item_of_its_own(cx).await;
        a_run_on_screen(&item, &mut cx, 4242);

        assert!(
            cx.debug_bounds("run-metrics-reading").is_none(),
            "the reading stays closed until it is asked for"
        );

        press_the_plaque(&mut cx);

        assert!(
            cx.debug_bounds("run-metrics-reading").is_some(),
            "pressing the plaque opens the reading"
        );
    }

    /// Escape is the way out of the reading.
    #[gpui::test]
    async fn escape_closes_the_reading(cx: &mut TestAppContext) {
        let (item, mut cx) = an_item_of_its_own(cx).await;
        a_run_on_screen(&item, &mut cx, 4242);
        press_the_plaque(&mut cx);
        assert!(cx.debug_bounds("run-metrics-reading").is_some());

        cx.simulate_keystrokes("escape");
        settle(&mut cx);

        assert!(
            cx.debug_bounds("run-metrics-reading").is_none(),
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
            .debug_bounds("run-metrics-reading")
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
            cx.debug_bounds("run-metrics-reading").is_none(),
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

    /// The list is ordered by what each process holds, stops at eight rows, and
    /// says what the rest hold between them rather than dropping them.
    #[gpui::test]
    fn the_processes_listed_are_the_largest_eight_and_the_rest_is_their_total(
        _cx: &mut TestAppContext,
    ) {
        let megabyte = 1024 * 1024;
        let tree: Vec<ProcessMemory> = (1..=12)
            .map(|which| a_process(which, which as u64 * megabyte))
            .collect();

        let (rows, the_rest) = listed(&tree);

        assert_eq!(rows.len(), PROCESSES_LISTED, "eight rows and no more");
        assert_eq!(
            rows.iter().map(|one| one.pid).collect::<Vec<_>>(),
            vec![12, 11, 10, 9, 8, 7, 6, 5],
            "largest first"
        );
        let (count, total) = the_rest.expect("four processes were left out");
        assert_eq!(count, 4);
        assert_eq!(
            total,
            (1 + 2 + 3 + 4) * megabyte,
            "the one row stands for everything the rest hold"
        );
    }

    /// Eight processes exactly are eight rows, with nothing summed up.
    #[gpui::test]
    fn eight_processes_need_no_row_for_the_rest(_cx: &mut TestAppContext) {
        let tree: Vec<ProcessMemory> = (1..=8).map(|which| a_process(which, 1024)).collect();

        let (rows, the_rest) = listed(&tree);

        assert_eq!(rows.len(), 8);
        assert_eq!(the_rest, None, "nothing was left out, so nothing is summed");
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
            cx.debug_bounds("run-metrics-reading").is_some(),
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
            .debug_bounds("run-metrics-reading")
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
}
