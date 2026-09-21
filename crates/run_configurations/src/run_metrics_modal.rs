use std::collections::{HashMap, HashSet};
use std::time::Duration;

use gpui::{
    App, Context, DismissEvent, EventEmitter, FocusHandle, Focusable, ScrollHandle, SharedString,
    Subscription, WeakEntity, Window, prelude::*,
};
use ui::{Divider, ScrollAxes, Scrollbars, Tooltip, WithScrollbar, cyberpunk, prelude::*};
use workspace::{ModalView, Workspace};

use crate::process_metrics::{self, ProcessReading};
use crate::run_metrics_status_item::{
    RunMetricsStatusItem, a_bar, a_chart, by_memory, said, what_it_says,
};

/// How far one step of the fork tree moves a row in from its parent. Wide enough
/// that the step is visible at a glance, narrow enough that a tree eight deep
/// still leaves room for the name.
const STEP: Pixels = px(16.);

/// The widths of the columns after a process's name. Fixed, so the numbers of
/// one row sit under the numbers of the next; the name column is what gives way.
const STATE_WIDTH: Pixels = px(96.);
const THREADS_WIDTH: Pixels = px(64.);
const CPU_WIDTH: Pixels = px(72.);
const MEMORY_WIDTH: Pixels = px(84.);
const SHARE_WIDTH: Pixels = px(96.);
const UPTIME_WIDTH: Pixels = px(78.);

/// The least width the tree's columns need between them. Below it the table
/// scrolls sideways rather than squeezing the name column to nothing.
const TREE_WIDTH: Pixels = px(720.);

/// The least width a chart is given before the row holding the charts wraps and
/// stands them one above the other instead.
const CHART_LEAST_WIDTH: Pixels = px(300.);

/// How short the row of charts may be squeezed before the window scrolls to it
/// instead. A heading and a plot, and nothing spare.
const CHART_LEAST_HEIGHT: Pixels = px(88.);

/// How tall a chart grows before the height is better spent on the processes
/// below it. Two minutes of readings drawn much taller than this turns a short
/// burst into a needle and a steady figure into a wall.
const CHART_MOST_HEIGHT: Pixels = px(240.);

/// Which half of the window is being read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tab {
    /// The two minutes behind the numbers, and every fact that has no series.
    Overview,
    /// Every process of the run, each under the one that started it.
    Forks,
}

impl Tab {
    fn label(self) -> &'static str {
        match self {
            Tab::Overview => "Overview",
            Tab::Forks => "Processes",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Tab::Overview => "overview",
            Tab::Forks => "forks",
        }
    }
}

/// What the run is using, in full: the two minutes behind the status bar's
/// numbers, every fact the machine will say, and the run's processes drawn as
/// the tree they are.
///
/// A window rather than the popover this replaces. A popover is as wide as it
/// was written to be and goes away the moment anything else is pressed, which
/// is the wrong shape for a reading somebody watches while a build runs: this
/// one is carried, resized and left open beside the work.
pub struct RunMetricsModal {
    item: WeakEntity<RunMetricsStatusItem>,
    focus: FocusHandle,
    tab: Tab,
    overview_scroll: ScrollHandle,
    forks_scroll: ScrollHandle,
    _observation: Option<Subscription>,
}

impl RunMetricsModal {
    pub fn new(item: WeakEntity<RunMetricsStatusItem>, cx: &mut Context<Self>) -> Self {
        let observation = item
            .upgrade()
            .map(|item| cx.observe(&item, |_, _, cx| cx.notify()));
        Self {
            item,
            focus: cx.focus_handle(),
            tab: Tab::Overview,
            overview_scroll: ScrollHandle::new(),
            forks_scroll: ScrollHandle::new(),
            _observation: observation,
        }
    }

    /// Opens the reading over the workspace the status bar lives in.
    pub fn open(
        workspace: &mut Workspace,
        item: WeakEntity<RunMetricsStatusItem>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        workspace.toggle_modal(window, cx, move |_window, cx| Self::new(item, cx));
    }

    fn show(&mut self, tab: Tab, cx: &mut Context<Self>) {
        self.tab = tab;
        cx.notify();
    }
}

impl EventEmitter<DismissEvent> for RunMetricsModal {}
impl ModalView for RunMetricsModal {}

impl Focusable for RunMetricsModal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// The run's processes in the order a tree is read: the root, then what it
/// started, each under its own parent, with how far in the row belongs.
///
/// The reading lists the processes with the root first and every parent before
/// its children, which says who started whom but not where a row sits. This puts
/// each child directly under its parent so the indent means something, and holds
/// siblings in the order the machine numbered them so the list does not reshuffle
/// itself every second.
pub fn forks_of(tree: &[ProcessReading]) -> Vec<(usize, ProcessReading)> {
    let Some(root) = tree.first() else {
        return Vec::new();
    };
    // Who started whom, worked out once. Asking the whole list for each process's
    // children instead would read it once per process, and a run can hold
    // thousands while the window redraws every second.
    let mut children: HashMap<u32, Vec<&ProcessReading>> = HashMap::new();
    for one in tree.iter().filter(|one| one.pid != root.pid) {
        children.entry(one.parent).or_default().push(one);
    }
    for theirs in children.values_mut() {
        theirs.sort_by_key(|one| one.pid);
    }

    let mut rows = Vec::with_capacity(tree.len());
    let mut walk: Vec<(usize, &ProcessReading)> = vec![(0, root)];
    // A process already drawn is never drawn again: /proc can name a parent that
    // is also a descendant while processes come and go, and a tree that follows
    // that names the same pid for ever.
    let mut taken: HashSet<u32> = HashSet::from([root.pid]);
    while let Some((depth, one)) = walk.pop() {
        rows.push((depth, one.clone()));
        let Some(theirs) = children.get(&one.pid) else {
            continue;
        };
        // Pushed back to front, because the last one pushed is the first one
        // taken off again -- and the lowest number is the one that should come
        // out first.
        for child in theirs.iter().rev() {
            if taken.insert(child.pid) {
                walk.push((depth + 1, child));
            }
        }
    }
    rows
}

/// The highest processor reading of a run so far, or nothing when no rate has
/// been worked out yet.
///
/// The first reading of a run has nothing to measure a rate against, so a run a
/// second old holds no rates at all -- and a busiest of `0.0%` there reads as a
/// run that is doing nothing rather than one nobody has timed yet.
pub fn busiest_of(readings: &[(Option<f32>, u64)]) -> Option<f32> {
    readings
        .iter()
        .filter_map(|(cpu, _)| *cpu)
        .fold(None, |most: Option<f32>, cpu| {
            Some(most.map_or(cpu, |most| most.max(cpu)))
        })
}

/// What the machine says a process is doing, as a word rather than the letter
/// `/proc` spells it with.
pub fn doing(state: char) -> &'static str {
    match state {
        'R' => "running",
        'S' => "sleeping",
        'D' => "waiting on disk",
        'T' | 't' => "stopped",
        'Z' => "ended, unreaped",
        'I' => "idle",
        'X' | 'x' => "dead",
        _ => "unknown",
    }
}

/// How long something has been alive, in the largest two units that say it.
///
/// Nothing comes back as `0s` when the machine would not say how long it has
/// itself been up: a run reported as having started this instant, every second,
/// is worse than no answer.
pub fn as_uptime(alive: Option<Duration>) -> String {
    let Some(alive) = alive else {
        return "--".to_string();
    };
    let seconds = alive.as_secs();
    let (days, rest) = (seconds / 86_400, seconds % 86_400);
    let (hours, rest) = (rest / 3_600, rest % 3_600);
    let (minutes, seconds) = (rest / 60, rest % 60);
    match (days, hours, minutes) {
        (0, 0, 0) => format!("{seconds}s"),
        (0, 0, _) => format!("{minutes}m {seconds:02}s"),
        (0, _, _) => format!("{hours}h {minutes:02}m"),
        _ => format!("{days}d {hours:02}h"),
    }
}

/// A percentage of one core, or the reason there is not one yet.
fn as_cpu(cpu: Option<f32>) -> String {
    match cpu {
        Some(cpu) => format!("{cpu:.1}%"),
        None => "--".to_string(),
    }
}

/// One figure of the row above the body: what it is, small and quiet, with the
/// number itself after it in the reading's own voice.
fn headline(label: &'static str, value: String) -> gpui::Div {
    h_flex()
        .gap_1()
        .items_baseline()
        .child(
            Label::new(label)
                .size(LabelSize::XSmall)
                .color(Color::Muted),
        )
        .child(Label::new(value).size(LabelSize::Default))
}

/// One cell of the tree's numeric columns, right-aligned so the digits of one
/// row line up under the digits of the next.
fn figure(width: Pixels, value: String, muted: bool) -> gpui::Div {
    h_flex().w(width).flex_none().justify_end().child(
        Label::new(value)
            .size(LabelSize::XSmall)
            .color(match muted {
                true => Color::Muted,
                false => Color::Default,
            })
            .single_line(),
    )
}

/// The naming row of the tree, so a column of bare numbers says what it counts.
fn tree_head() -> gpui::Div {
    h_flex()
        .w_full()
        .min_w(TREE_WIDTH)
        .px_2()
        .py_1()
        .gap(cyberpunk::SPACE_8)
        .child(
            div().flex_1().min_w(px(160.)).child(
                Label::new("Process")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            ),
        )
        .child(figure(STATE_WIDTH, "State".to_string(), true))
        .child(figure(THREADS_WIDTH, "Threads".to_string(), true))
        .child(figure(UPTIME_WIDTH, "Alive".to_string(), true))
        .child(figure(CPU_WIDTH, "CPU".to_string(), true))
        .child(figure(MEMORY_WIDTH, "Memory".to_string(), true))
        .child(
            h_flex().w(SHARE_WIDTH).flex_none().justify_end().child(
                Label::new("Share")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            ),
        )
}

/// One process of the tree: how far in it sits, what it is, and everything the
/// machine says about it.
fn fork_row(depth: usize, one: &ProcessReading, largest: u64) -> gpui::Stateful<gpui::Div> {
    let share = match largest > 0 {
        true => (one.memory as f32 / largest as f32).clamp(0., 1.),
        false => 0.,
    };
    h_flex()
        .id(SharedString::from(format!("fork-{}", one.pid)))
        .debug_selector({
            let pid = one.pid;
            move || format!("FORK-ROW-{pid}")
        })
        .w_full()
        .min_w(TREE_WIDTH)
        .px_2()
        .py_0p5()
        .gap(cyberpunk::SPACE_8)
        .rounded(px(3.))
        .hover(|row| row.bg(cyberpunk::row_hovered()))
        .child(
            h_flex()
                .flex_1()
                .min_w(px(160.))
                .gap_1p5()
                .overflow_hidden()
                // The indent is a spacer of its own rather than padding, so the
                // guide beside it marks where this row's parent stands.
                .child(
                    div()
                        .w(STEP * depth as f32)
                        .flex_none()
                        .when(depth > 0, |rail| {
                            rail.border_r_1().border_color(cyberpunk::border_dim())
                        }),
                )
                .child(
                    h_flex()
                        .gap_1p5()
                        .min_w_0()
                        .debug_selector({
                            let pid = one.pid;
                            move || format!("FORK-NAME-{pid}")
                        })
                        .child(
                            Label::new(one.pid.to_string())
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(
                            Label::new(one.name.to_string())
                                .size(LabelSize::XSmall)
                                .truncate(),
                        ),
                ),
        )
        .child(figure(STATE_WIDTH, doing(one.state).to_string(), true))
        .child(figure(THREADS_WIDTH, one.threads.to_string(), false))
        .child(figure(UPTIME_WIDTH, as_uptime(one.uptime), true))
        .child(figure(CPU_WIDTH, as_cpu(one.cpu), false))
        .child(figure(
            MEMORY_WIDTH,
            process_metrics::as_memory(one.memory),
            false,
        ))
        .child(
            div()
                .w(SHARE_WIDTH)
                .flex_none()
                .h(px(6.))
                .rounded(px(2.))
                .bg(cyberpunk::surface())
                .child(
                    div()
                        .h_full()
                        .w(relative(share))
                        .rounded(px(2.))
                        .bg(cyberpunk::ramp(share)),
                ),
        )
}

impl Render for RunMetricsModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let shell = cyberpunk::dialog_shell("Run metrics", window, cx)
            .key_context("RunMetrics")
            .track_focus(&self.focus)
            .debug_selector(|| "RUN-METRICS-MODAL".to_string())
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .child(
                cyberpunk::dialog_header("What the run is using", cx).child(
                    IconButton::new("run-metrics-dismiss", IconName::Close)
                        .icon_size(IconSize::Small)
                        .style(cyberpunk::Rank::Quiet.style())
                        .tooltip(Tooltip::text("Close"))
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                ),
            );

        let Some(item) = self.item.upgrade() else {
            return shell;
        };
        let (metrics, readings) = {
            let item = item.read(cx);
            (item.reading(), item.series())
        };
        let Some(metrics) = metrics else {
            return shell.child(
                cyberpunk::dialog_body().child(
                    div().p(cyberpunk::SPACE_14).child(
                        Label::new("Nothing is running.")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
                ),
            );
        };

        let tab = self.tab;
        let tabs = cyberpunk::segmented([Tab::Overview, Tab::Forks].into_iter().map(|which| {
            Button::new(
                SharedString::from(format!("run-metrics-tab-{}", which.name())),
                which.label(),
            )
            .label_size(LabelSize::Small)
            .style(match which == tab {
                true => cyberpunk::Rank::Accent.style(),
                false => cyberpunk::Rank::Quiet.style(),
            })
            .on_click(cx.listener(move |modal, _, _, cx| modal.show(which, cx)))
            .into_any_element()
        }));

        // The headline figures ride at the far end of the tab row rather than in
        // a row of their own: they are four short words, and a row spent on four
        // short words is a row the charts do not get.
        let chrome = h_flex()
            .flex_none()
            .w_full()
            .px_3()
            .pb_2()
            .gap(cyberpunk::SPACE_8)
            .items_center()
            .child(tabs)
            .child(div().flex_1())
            .child(
                h_flex()
                    .gap(cyberpunk::SPACE_14)
                    .flex_wrap()
                    .justify_end()
                    .child(headline("CPU", as_cpu(metrics.cpu)))
                    .child(headline("RAM", process_metrics::as_memory(metrics.memory)))
                    .child(headline("threads", metrics.threads.to_string()))
                    .child(headline("alive", as_uptime(metrics.uptime))),
            );

        let body = match tab {
            Tab::Overview => self.overview(&metrics, &readings, window, cx),
            Tab::Forks => self.forks(&metrics, window, cx),
        };

        shell
            .child(chrome)
            .child(Divider::horizontal())
            .child(cyberpunk::dialog_body().child(body))
    }
}

impl RunMetricsModal {
    fn overview(
        &self,
        metrics: &crate::process_metrics::Metrics,
        readings: &[(Option<f32>, u64)],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let processor: Vec<(usize, f32)> = readings
            .iter()
            .enumerate()
            .filter_map(|(at, (cpu, _))| cpu.map(|cpu| (at, cpu)))
            .collect();
        let held: Vec<(usize, f32)> = readings
            .iter()
            .enumerate()
            .map(|(at, (_, memory))| (at, *memory as f32))
            .collect();
        let most_held = readings
            .iter()
            .fold(0u64, |most, (_, memory)| most.max(*memory));
        let busiest = busiest_of(readings);

        let rows = by_memory(&metrics.tree);
        let largest = rows.first().map(|one| one.memory).unwrap_or(0);

        div()
            .id("run-metrics-overview")
            .debug_selector(|| "RUN-METRICS-OVERVIEW".to_string())
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.overview_scroll)
            .p(cyberpunk::SPACE_14)
            .child(
                v_flex()
                    .w_full()
                    // The column is at least as tall as the window it scrolls
                    // in, so a window stretched taller than its reading has the
                    // surplus to give rather than leaving it blank.
                    .min_h_full()
                    .gap(cyberpunk::SPACE_18)
                    // The two charts stand side by side while there is room for
                    // both and fall into a column when there is not, which is
                    // what makes the window worth widening. They grow with it
                    // too, up to the height past which a plot reads worse rather
                    // than better; the processes below take the rest.
                    .child(
                        h_flex()
                            .w_full()
                            .flex_grow_1()
                            .flex_shrink_0()
                            .min_h(CHART_LEAST_HEIGHT)
                            .max_h(CHART_MOST_HEIGHT)
                            .flex_wrap()
                            .items_stretch()
                            .gap(cyberpunk::SPACE_18)
                            .child(
                                div()
                                    .flex_1()
                                    .h_full()
                                    .min_w(CHART_LEAST_WIDTH)
                                    .debug_selector(|| "RUN-METRICS-CHART-CPU".to_string())
                                    .child(a_chart(
                                        "Processor",
                                        as_cpu(metrics.cpu),
                                        processor,
                                        100.,
                                        cyberpunk::series_processor(),
                                        vec![(0., "100".to_string()), (0.5, "50".to_string())],
                                    )),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .h_full()
                                    .min_w(CHART_LEAST_WIDTH)
                                    .debug_selector(|| "RUN-METRICS-CHART-MEMORY".to_string())
                                    .child(a_chart(
                                        "Memory",
                                        process_metrics::as_memory(metrics.memory),
                                        held,
                                        most_held as f32,
                                        cyberpunk::series_memory(),
                                        vec![(0., process_metrics::as_memory(most_held))],
                                    )),
                            ),
                    )
                    .child(
                        v_flex()
                            .w_full()
                            // What the charts stop taking, the processes get:
                            // every one of them, in whatever room is left. It
                            // grows into free height but never shrinks below its
                            // rows -- this column is scrolled, not clipped.
                            .flex_grow_1()
                            .flex_shrink_0()
                            .gap(cyberpunk::SPACE_4)
                            .child(
                                Label::new("Memory by process")
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .children(rows.iter().map(|one| {
                                a_bar(format!("{} · {}", one.pid, one.name), one.memory, largest)
                            })),
                    )
                    .child(
                        v_flex()
                            .w_full()
                            .flex_none()
                            .debug_selector(|| "RUN-METRICS-FACTS".to_string())
                            .gap(cyberpunk::SPACE_4)
                            .child(
                                Label::new("Everything else the machine says")
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(
                                h_flex()
                                    .w_full()
                                    .flex_wrap()
                                    .gap(cyberpunk::SPACE_14)
                                    .child(said("PID", metrics.pid.to_string()))
                                    .child(said("processes", metrics.processes.to_string()))
                                    .child(said("threads", metrics.threads.to_string()))
                                    .child(said("alive", as_uptime(metrics.uptime)))
                                    .child(said("busiest", as_cpu(busiest)))
                                    .child(said(
                                        "most memory",
                                        process_metrics::as_memory(most_held),
                                    ))
                                    .child(said("network", what_it_says(metrics.network)))
                                    .child(said(
                                        "video memory",
                                        what_it_says(metrics.video_memory),
                                    )),
                            ),
                    ),
            )
            .custom_scrollbars(
                Scrollbars::always_visible(ScrollAxes::Vertical)
                    .tracked_scroll_handle(&self.overview_scroll),
                window,
                cx,
            )
    }

    fn forks(
        &self,
        metrics: &crate::process_metrics::Metrics,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let rows = forks_of(&metrics.tree);
        let largest = metrics
            .tree
            .iter()
            .fold(0u64, |most, one| most.max(one.memory));

        div()
            .id("run-metrics-forks")
            .debug_selector(|| "RUN-METRICS-FORKS".to_string())
            .size_full()
            // Sideways as well as up and down: the columns have a width they
            // cannot go under, and a narrow window scrolls to them rather than
            // squeezing the names away.
            .overflow_scroll()
            .track_scroll(&self.forks_scroll)
            .p(cyberpunk::SPACE_8)
            .child(
                v_flex()
                    .w_full()
                    .child(tree_head())
                    .child(Divider::horizontal())
                    .children(
                        rows.iter()
                            .map(|(depth, one)| fork_row(*depth, one, largest)),
                    ),
            )
            .custom_scrollbars(
                Scrollbars::always_visible(ScrollAxes::Both)
                    .tracked_scroll_handle(&self.forks_scroll),
                window,
                cx,
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_process(pid: u32, parent: u32) -> ProcessReading {
        ProcessReading {
            pid,
            parent,
            name: "a program".into(),
            memory: 1024,
            cpu: None,
            threads: 1,
            state: 'S',
            uptime: None,
        }
    }

    /// The reading lists the processes with every parent before its children,
    /// which is not the same as every child under its own parent: a breadth-first
    /// list draws both children of the root, then both grandchildren, and an
    /// indent over that order points at the wrong row.
    #[test]
    fn every_child_is_drawn_under_the_process_that_started_it() {
        let breadth_first = vec![
            a_process(100, 1),
            a_process(101, 100),
            a_process(102, 100),
            a_process(103, 101),
        ];

        let rows = forks_of(&breadth_first);

        assert_eq!(
            rows.iter()
                .map(|(depth, one)| (*depth, one.pid))
                .collect::<Vec<_>>(),
            vec![(0, 100), (1, 101), (2, 103), (1, 102)],
            "the grandchild belongs under its own parent, not after the last child"
        );
    }

    /// Siblings keep the order the machine numbered them in, so a list somebody
    /// is watching does not reshuffle itself once a second.
    #[test]
    fn siblings_are_drawn_in_the_order_the_machine_numbered_them() {
        let out_of_order = vec![
            a_process(100, 1),
            a_process(140, 100),
            a_process(120, 100),
            a_process(130, 100),
        ];

        let rows = forks_of(&out_of_order);

        assert_eq!(
            rows.iter().map(|(_, one)| one.pid).collect::<Vec<_>>(),
            vec![100, 120, 130, 140]
        );
    }

    /// `/proc` can name a parent that is also a descendant while processes come
    /// and go. A tree that follows that draws the same pid for ever.
    #[test]
    fn a_process_that_names_its_own_descendant_as_its_parent_is_drawn_once() {
        let circular = vec![a_process(100, 101), a_process(101, 100)];

        let rows = forks_of(&circular);

        assert_eq!(rows.len(), 2, "each process once and no more");
        assert_eq!(
            rows.iter().map(|(_, one)| one.pid).collect::<Vec<_>>(),
            vec![100, 101]
        );
    }

    #[test]
    fn nothing_running_is_no_rows_rather_than_a_panic() {
        assert!(forks_of(&[]).is_empty());
    }

    /// A run nested deeper than any bound somebody might think to put on it is
    /// still drawn to its end. The guard against a cycle is the set of processes
    /// already drawn; a depth limit on top of it drops real processes.
    #[test]
    fn a_very_deep_run_keeps_its_last_process() {
        let deep: Vec<ProcessReading> = std::iter::once(a_process(100, 1))
            .chain((1..200).map(|step| a_process(100 + step, 100 + step - 1)))
            .collect();

        let rows = forks_of(&deep);

        assert_eq!(rows.len(), deep.len(), "every process of the run is drawn");
        assert_eq!(
            rows.last().map(|(depth, one)| (*depth, one.pid)),
            Some((199, 299)),
            "including the last one, however deep it sits"
        );
    }

    /// A run nobody has timed yet has no busiest reading, and says so. Zero
    /// there reads as a run that is doing nothing.
    #[test]
    fn a_run_with_no_rate_yet_has_no_busiest_reading() {
        assert_eq!(busiest_of(&[]), None);
        assert_eq!(
            busiest_of(&[(None, 1_000), (None, 1_000)]),
            None,
            "two readings without a rate between them are still no rate"
        );
        assert_eq!(
            busiest_of(&[(None, 1_000), (Some(12.), 1_000), (Some(40.), 1_000)]),
            Some(40.)
        );
    }

    /// A machine that will not say how long it has been up leaves the age of a
    /// process unknown, and unknown is said as such: `0s` every second would
    /// read as a run that keeps restarting.
    #[test]
    fn an_age_nobody_could_work_out_is_not_said_as_a_moment_ago() {
        assert_eq!(as_uptime(None), "--");
        assert_eq!(as_uptime(Some(Duration::from_secs(9))), "9s");
        assert_eq!(as_uptime(Some(Duration::from_secs(125))), "2m 05s");
        assert_eq!(as_uptime(Some(Duration::from_secs(3_725))), "1h 02m");
        assert_eq!(as_uptime(Some(Duration::from_secs(180_000))), "2d 02h");
    }

    #[test]
    fn what_a_process_is_doing_is_said_in_words() {
        assert_eq!(doing('R'), "running");
        assert_eq!(doing('Z'), "ended, unreaped");
        assert_eq!(
            doing('?'),
            "unknown",
            "a letter nobody knows is not invented"
        );
    }
}
