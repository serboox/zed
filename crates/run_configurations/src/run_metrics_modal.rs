use std::collections::{HashMap, HashSet};
use std::time::Duration;

use gpui::{
    App, Context, DismissEvent, EventEmitter, FocusHandle, Focusable, ScrollHandle, SharedString,
    Subscription, WeakEntity, Window, prelude::*,
};
use ui::{
    Disclosure, Divider, ScrollAxes, Scrollbars, Tooltip, WithScrollbar, cyberpunk, prelude::*,
};
use workspace::{ModalView, Workspace};

use crate::goroutines::{GoroutineReading, GoroutineSource};
use crate::process_metrics::{self, Metrics, ProcessReading, ThreadReading};
use crate::run_metrics_status_item::{RunMetricsStatusItem, a_chart};

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

/// The least width the tree's columns need between them. Below it the table
/// scrolls sideways rather than squeezing the name column to nothing.
const TREE_WIDTH: Pixels = px(640.);

/// The least width a chart is given before the row holding the charts wraps and
/// stands them one above the other instead.
const CHART_LEAST_WIDTH: Pixels = px(220.);

/// How tall the row of charts stands. Fixed and modest: this reading is watched
/// beside the work a run is doing, not the reason the window was opened, so it
/// gets a sparkline and its current value rather than a plot somebody studies.
const CHART_ROW_HEIGHT: Pixels = px(80.);

/// What the run is using, in full: the two minutes behind the status bar's
/// numbers, the run's processes drawn as the tree they are, and every fact the
/// machine will say that has no series of its own.
///
/// A window rather than the popover this replaces. A popover is as wide as it
/// was written to be and goes away the moment anything else is pressed, which
/// is the wrong shape for a reading somebody watches while a build runs: this
/// one is carried, resized and left open beside the work. It is a single
/// scrolled page rather than tabs, because a process one has to switch a tab to
/// see is a process that reads as hidden.
pub struct RunMetricsModal {
    item: WeakEntity<RunMetricsStatusItem>,
    focus: FocusHandle,
    body_scroll: ScrollHandle,
    /// Which processes have their thread and goroutine detail shown. A pid
    /// stays in this set across readings, so a row a reader opened does not
    /// close itself the moment the numbers under it change.
    expanded: HashSet<u32>,
    /// The root process the reading was last opened on, so that root can be
    /// expanded by default exactly once and a reader who collapses it again is
    /// not overridden on the next reading a second later.
    default_expanded_for: Option<u32>,
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
            body_scroll: ScrollHandle::new(),
            expanded: HashSet::new(),
            default_expanded_for: None,
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

    /// Shows or hides one process's thread and goroutine detail.
    fn toggle_process(&mut self, pid: u32, cx: &mut Context<Self>) {
        if !self.expanded.remove(&pid) {
            self.expanded.insert(pid);
        }
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

/// What a number nobody could measure says instead of a zero, which would read
/// as "it is using none of this" -- said as a plain reason rather than the
/// dashed-out form the status bar uses, since this line names what it is about
/// itself.
fn as_fact(value: Result<u64, &'static str>) -> String {
    match value {
        Ok(bytes) => process_metrics::as_memory(bytes),
        Err(reason) => reason.to_string(),
    }
}

/// One figure of the summary row: what it is, small and quiet, with the number
/// itself after it in the reading's own voice.
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

/// One cell of the table's numeric columns, right-aligned so the digits of one
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

/// The naming row of the table, so a column of bare numbers says what it counts.
fn process_head() -> gpui::Div {
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
        .child(figure(CPU_WIDTH, "CPU".to_string(), true))
        .child(figure(MEMORY_WIDTH, "Memory".to_string(), true))
        .child(figure(THREADS_WIDTH, "Threads".to_string(), true))
        .child(figure(STATE_WIDTH, "State".to_string(), true))
}

/// One process of the tree: how far in it sits, what it is, everything the
/// machine says about it, and a toggle for the thread and goroutine detail
/// underneath it. The whole row answers a click, not only the toggle itself --
/// a target the size of a triangle is a target that is missed.
fn process_row(
    depth: usize,
    one: &ProcessReading,
    is_open: bool,
    cx: &mut Context<RunMetricsModal>,
) -> gpui::Stateful<gpui::Div> {
    let pid = one.pid;
    h_flex()
        .id(SharedString::from(format!("run-metrics-process-{pid}")))
        .debug_selector(move || format!("RUN-METRICS-PROCESS-{pid}"))
        .w_full()
        .min_w(TREE_WIDTH)
        .px_2()
        .py_0p5()
        .gap(cyberpunk::SPACE_8)
        .rounded(px(3.))
        .cursor_pointer()
        .hover(|row| row.bg(cyberpunk::row_hovered()))
        .on_click(cx.listener(move |modal, _, _, cx| modal.toggle_process(pid, cx)))
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
                .child(Disclosure::new(
                    SharedString::from(format!("run-metrics-disclose-{pid}")),
                    is_open,
                ))
                .child(
                    h_flex()
                        .gap_1p5()
                        .min_w_0()
                        .debug_selector({
                            let pid = one.pid;
                            move || format!("RUN-METRICS-PROCESS-NAME-{pid}")
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
        .child(figure(CPU_WIDTH, as_cpu(one.cpu), false))
        .child(figure(
            MEMORY_WIDTH,
            process_metrics::as_memory(one.memory),
            false,
        ))
        .child(figure(THREADS_WIDTH, one.threads.to_string(), false))
        .child(figure(STATE_WIDTH, doing(one.state).to_string(), true))
}

/// Every thread a process is running, busiest first, as one wrapped line rather
/// than a column of rows: a thread's whole story is its name, what it is doing,
/// and its share of a core, which reads fine run together.
fn threads_line(pid: u32, threads: &[ThreadReading]) -> gpui::Div {
    let count = threads.len();
    h_flex()
        .w_full()
        .items_start()
        .gap_1p5()
        .debug_selector(move || format!("RUN-METRICS-THREADS-{pid}"))
        .child(
            Label::new("threads")
                .size(LabelSize::XSmall)
                .color(Color::Muted),
        )
        .child(if threads.is_empty() {
            h_flex().child(
                Label::new("-- no per-thread detail on this platform")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
        } else {
            h_flex()
                .flex_1()
                .flex_wrap()
                .gap_1p5()
                .children(threads.iter().enumerate().map(|(at, thread)| {
                    h_flex()
                        .gap_1p5()
                        .when(at > 0, |row| {
                            row.child(Label::new("·").size(LabelSize::XSmall).color(Color::Muted))
                        })
                        .child(
                            Label::new(format!(
                                "{} {} {}",
                                thread.name,
                                doing(thread.state),
                                as_cpu(thread.cpu)
                            ))
                            .size(LabelSize::XSmall),
                        )
                }))
                .child(
                    Label::new(format!("({count})"))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
        })
}

/// Where a Go program's goroutines were read from, said in a few words.
fn goroutine_source(source: &GoroutineSource) -> String {
    match source {
        GoroutineSource::Debugger => "the debugger".to_string(),
        GoroutineSource::Pprof(address) => format!("pprof at {address}"),
    }
}

/// A run's goroutines, when it is a Go program: the total and how many are in
/// each state, or -- when they could not be read -- the reason said as what the
/// reader can do about it.
fn goroutines_line(reading: &GoroutineReading) -> gpui::Div {
    h_flex()
        .w_full()
        .items_start()
        .gap_1p5()
        .debug_selector(|| "RUN-METRICS-GOROUTINES".to_string())
        .child(
            Label::new("goroutines")
                .size(LabelSize::XSmall)
                .color(Color::Muted),
        )
        .child(match reading {
            GoroutineReading::Read(goroutines) => {
                let by_state = goroutines
                    .by_state
                    .iter()
                    .map(|(state, count)| format!("{state} {count}"))
                    .collect::<Vec<_>>()
                    .join(" · ");
                h_flex()
                    .flex_1()
                    .flex_wrap()
                    .gap_1p5()
                    .child(
                        Label::new(format!("{by_state}  ({})", goroutines.total))
                            .size(LabelSize::XSmall),
                    )
                    .child(
                        Label::new(format!("· {}", goroutine_source(&goroutines.source)))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
            }
            GoroutineReading::Unavailable(hint) => h_flex().child(
                Label::new(hint.clone())
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            ),
        })
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
        let (metrics, readings, goroutines) = {
            let item = item.read(cx);
            (item.reading(), item.series(), item.goroutines())
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

        // The root is expanded by default -- but only the first time this
        // reading's root is seen, so a reader who collapses it again is not
        // overridden a second later by the same run's next reading.
        if self.default_expanded_for != Some(metrics.pid) {
            self.expanded.insert(metrics.pid);
            self.default_expanded_for = Some(metrics.pid);
        }

        let body = self.body(&metrics, &readings, &goroutines, window, cx);

        shell.child(cyberpunk::dialog_body().child(body))
    }
}

impl RunMetricsModal {
    fn body(
        &self,
        metrics: &Metrics,
        readings: &[(Option<f32>, u64)],
        goroutines: &Option<GoroutineReading>,
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

        let rows = forks_of(&metrics.tree);

        div()
            .id("run-metrics-body")
            .debug_selector(|| "RUN-METRICS-BODY".to_string())
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.body_scroll)
            .p(cyberpunk::SPACE_14)
            .child(
                v_flex()
                    .w_full()
                    // The column is at least as tall as the window it scrolls
                    // in, so a window stretched taller than its reading has the
                    // surplus to give rather than leaving it blank.
                    .min_h_full()
                    .gap(cyberpunk::SPACE_18)
                    .child(self.summary(metrics))
                    .child(Divider::horizontal())
                    .child(self.charts(metrics, processor, held, most_held))
                    .child(Divider::horizontal())
                    .child(self.processes(&rows, metrics.pid, goroutines, cx))
                    .child(self.footer_facts(metrics)),
            )
            .custom_scrollbars(
                Scrollbars::always_visible(ScrollAxes::Vertical)
                    .tracked_scroll_handle(&self.body_scroll),
                window,
                cx,
            )
    }

    /// The header row: the root process on the left, and everything the status
    /// bar summed up on the right. Four short words are not worth a row of
    /// their own when there is room for them beside what is already there.
    fn summary(&self, metrics: &Metrics) -> gpui::Div {
        let root_name = metrics
            .tree
            .first()
            .map(|process| process.name.to_string())
            .unwrap_or_default();
        h_flex()
            .w_full()
            .flex_none()
            .flex_wrap()
            .justify_between()
            .items_center()
            .gap(cyberpunk::SPACE_14)
            .debug_selector(|| "RUN-METRICS-SUMMARY".to_string())
            .child(
                h_flex()
                    .gap_1p5()
                    .items_baseline()
                    .child(Label::new(root_name).size(LabelSize::Default))
                    .child(
                        Label::new(format!("PID {}", metrics.pid))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(format!("up {}", as_uptime(metrics.uptime)))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .child(
                h_flex()
                    .gap(cyberpunk::SPACE_14)
                    .flex_wrap()
                    .justify_end()
                    .child(headline("CPU", as_cpu(metrics.cpu)))
                    .child(headline("RAM", process_metrics::as_memory(metrics.memory)))
                    .child(headline("proc", metrics.processes.to_string()))
                    .child(headline("thr", metrics.threads.to_string())),
            )
    }

    /// Two compact sparklines, side by side while there is room for both and
    /// stacked when there is not. Fixed and modest in height: this is what the
    /// numbers above looked like over the last two minutes, not a plot somebody
    /// studies on its own.
    fn charts(
        &self,
        metrics: &Metrics,
        processor: Vec<(usize, f32)>,
        held: Vec<(usize, f32)>,
        most_held: u64,
    ) -> gpui::Div {
        h_flex()
            .w_full()
            .flex_none()
            .flex_wrap()
            .gap(cyberpunk::SPACE_18)
            .child(
                div()
                    .flex_1()
                    .h(CHART_ROW_HEIGHT)
                    .min_w(CHART_LEAST_WIDTH)
                    .overflow_hidden()
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
                    .h(CHART_ROW_HEIGHT)
                    .min_w(CHART_LEAST_WIDTH)
                    .overflow_hidden()
                    .debug_selector(|| "RUN-METRICS-CHART-MEMORY".to_string())
                    .child(a_chart(
                        "Memory",
                        process_metrics::as_memory(metrics.memory),
                        held,
                        most_held as f32,
                        cyberpunk::series_memory(),
                        vec![(0., process_metrics::as_memory(most_held))],
                    )),
            )
    }

    /// Every process of the run, as the tree it is, with a toggle on each row
    /// for the thread and goroutine detail underneath it.
    fn processes(
        &self,
        rows: &[(usize, ProcessReading)],
        root_pid: u32,
        goroutines: &Option<GoroutineReading>,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        v_flex()
            .w_full()
            .flex_none()
            .gap(cyberpunk::SPACE_4)
            .child(
                Label::new("Processes")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(process_head())
            .child(Divider::horizontal())
            .children(rows.iter().map(|(depth, process)| {
                self.process_block(*depth, process, root_pid, goroutines, cx)
            }))
    }

    /// One process, and -- when it is open -- its thread detail and, for the
    /// run's own root, its goroutines.
    fn process_block(
        &self,
        depth: usize,
        process: &ProcessReading,
        root_pid: u32,
        goroutines: &Option<GoroutineReading>,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let pid = process.pid;
        let is_open = self.expanded.contains(&pid);
        v_flex()
            .w_full()
            .child(process_row(depth, process, is_open, cx))
            .when(is_open, |this| {
                this.child(
                    v_flex()
                        .w_full()
                        .pl(STEP * (depth as f32 + 2.))
                        .pr_2()
                        .py_1()
                        .gap(cyberpunk::SPACE_4)
                        .child(threads_line(pid, &process.thread_readings))
                        .when(pid == root_pid, |this| {
                            this.children(goroutines.as_ref().map(goroutines_line))
                        }),
                )
            })
    }

    /// The facts that have no series of their own, as one muted line at the
    /// foot of the scrolled content.
    fn footer_facts(&self, metrics: &Metrics) -> gpui::Div {
        div()
            .w_full()
            .flex_none()
            .debug_selector(|| "RUN-METRICS-FACTS".to_string())
            .child(
                Label::new(format!(
                    "Network: {} · Video memory: {}",
                    as_fact(metrics.network),
                    as_fact(metrics.video_memory),
                ))
                .size(LabelSize::XSmall)
                .color(Color::Muted),
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
            thread_readings: Vec::new(),
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

    /// A number nobody could measure says why, in this line's own words rather
    /// than the status bar's dashed-out form.
    #[test]
    fn a_fact_nobody_could_measure_says_why_without_a_dash() {
        assert_eq!(
            as_fact(Err("needs rights this editor does not ask for")),
            "needs rights this editor does not ask for"
        );
        assert_eq!(as_fact(Ok(84 * 1024 * 1024)), "84 MB");
    }

    // The tests below draw the real modal and click into it, the way a reader
    // would. They rely on `RunMetricsStatusItem::set_reading_for_test`, a
    // test-only setter that stands in for the watcher's poll -- see this
    // crate's report on the redesign for its exact signature.

    use gpui::{Entity, Modifiers, TestAppContext, VisualTestContext};
    use project::{FakeFs, Project};
    use serde_json::json;
    use util::path;

    use crate::goroutines::Goroutines;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            crate::init(cx);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            cx.bind_keys([gpui::KeyBinding::new("escape", menu::Cancel, None)]);
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
    /// by hand, the same way `run_metrics_status_item`'s tests do.
    fn settle(cx: &mut VisualTestContext) {
        for _ in 0..3 {
            draw(cx);
            cx.update(|window, cx| {
                window.simulate_next_frame(cx);
            });
        }
        draw(cx);
    }

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

    fn a_thread(tid: u32, name: &str, cpu: f32) -> ThreadReading {
        ThreadReading {
            tid,
            name: name.into(),
            cpu: Some(cpu),
            state: 'S',
        }
    }

    /// A run of two: the process this crate tracks, and one process it started
    /// which has no per-thread detail of its own -- the shape a real Go run and
    /// its shell take.
    fn a_run() -> (u32, u32, Metrics) {
        let root_pid = 4241;
        let child_pid = 4242;
        let root = ProcessReading {
            pid: root_pid,
            parent: 1,
            name: "cmd-api".into(),
            memory: 311 * 1024 * 1024,
            cpu: Some(0.),
            threads: 2,
            state: 'S',
            uptime: Some(Duration::from_secs(125)),
            thread_readings: vec![a_thread(1, "main", 0.), a_thread(2, "gc", 0.)],
        };
        let child = ProcessReading {
            pid: child_pid,
            parent: root_pid,
            name: "bash".into(),
            memory: 4 * 1024 * 1024,
            cpu: Some(0.),
            threads: 1,
            state: 'S',
            uptime: Some(Duration::from_secs(125)),
            thread_readings: Vec::new(),
        };
        let metrics = Metrics {
            pid: root_pid,
            processes: 2,
            cpu: Some(0.),
            memory: 315 * 1024 * 1024,
            network: Err("needs rights this editor does not ask for"),
            video_memory: Err("nothing is using it"),
            threads: 3,
            uptime: Some(Duration::from_secs(125)),
            tree: vec![root, child],
        };
        (root_pid, child_pid, metrics)
    }

    fn press_the_plaque(cx: &mut VisualTestContext) {
        let plaque = cx
            .debug_bounds("run-metrics-status")
            .expect("the plaque is on screen");
        cx.simulate_click(plaque.center(), Modifiers::none());
        settle(cx);
    }

    /// Every process, and both charts, are painted from the same open reading:
    /// nothing here is a tab a reader has to switch to.
    #[gpui::test]
    async fn processes_and_charts_share_one_page_without_tabs(cx: &mut TestAppContext) {
        let (item, mut cx) = an_item_of_its_own(cx).await;
        let (root_pid, child_pid, metrics) = a_run();
        item.update(&mut cx, |item, cx| {
            item.set_reading_for_test(Some(metrics), None, cx);
        });
        draw(&mut cx);

        press_the_plaque(&mut cx);

        assert!(
            cx.debug_bounds("RUN-METRICS-CHART-CPU").is_some(),
            "the processor chart is on the one page"
        );
        assert!(
            cx.debug_bounds("RUN-METRICS-CHART-MEMORY").is_some(),
            "the memory chart is on the same page"
        );
        assert!(
            cx.debug_bounds(format!("RUN-METRICS-PROCESS-{root_pid}").leak())
                .is_some(),
            "and so is the root process, without pressing anything to see it"
        );
        assert!(
            cx.debug_bounds(format!("RUN-METRICS-PROCESS-{child_pid}").leak())
                .is_some(),
            "and every process it started"
        );
        assert!(
            cx.debug_bounds("BUTTON-Processes").is_none(),
            "there is no tab that hides the processes behind it"
        );
        assert!(
            cx.debug_bounds("BUTTON-Overview").is_none(),
            "nor a tab for the charts"
        );
    }

    /// Clicking a process row is what shows its threads. The root is expanded
    /// on its own, so this presses the row that starts closed.
    #[gpui::test]
    async fn clicking_a_process_row_shows_its_threads(cx: &mut TestAppContext) {
        let (item, mut cx) = an_item_of_its_own(cx).await;
        let (_root_pid, child_pid, metrics) = a_run();
        item.update(&mut cx, |item, cx| {
            item.set_reading_for_test(Some(metrics), None, cx);
        });
        draw(&mut cx);
        press_the_plaque(&mut cx);

        assert!(
            cx.debug_bounds(format!("RUN-METRICS-THREADS-{child_pid}").leak())
                .is_none(),
            "a process nobody has asked about keeps its threads closed"
        );

        let row = cx
            .debug_bounds(format!("RUN-METRICS-PROCESS-{child_pid}").leak())
            .expect("the process has a row to press");
        cx.simulate_click(row.center(), Modifiers::none());
        settle(&mut cx);

        assert!(
            cx.debug_bounds(format!("RUN-METRICS-THREADS-{child_pid}").leak())
                .is_some(),
            "and pressing its row is what opens them"
        );
    }

    /// A run's goroutines show up as their own line, once the status item has a
    /// reading of them -- and not before.
    #[gpui::test]
    async fn a_goroutines_line_appears_once_the_status_item_has_a_reading(cx: &mut TestAppContext) {
        let (item, mut cx) = an_item_of_its_own(cx).await;
        let (_root_pid, _child_pid, metrics) = a_run();
        item.update(&mut cx, |item, cx| {
            item.set_reading_for_test(Some(metrics.clone()), None, cx);
        });
        draw(&mut cx);
        press_the_plaque(&mut cx);

        assert!(
            cx.debug_bounds("RUN-METRICS-GOROUTINES").is_none(),
            "nothing to show while the run has no goroutine reading"
        );

        let goroutines = GoroutineReading::Read(Goroutines {
            total: 37,
            by_state: vec![("running".into(), 2), ("waiting".into(), 35)],
            source: GoroutineSource::Debugger,
        });
        item.update(&mut cx, |item, cx| {
            item.set_reading_for_test(Some(metrics), Some(goroutines), cx);
        });
        settle(&mut cx);

        assert!(
            cx.debug_bounds("RUN-METRICS-GOROUTINES").is_some(),
            "and the line shows up the moment the status item has one"
        );
    }

    /// The charts read at a glance, not as a plot to study: fixed and modest,
    /// nowhere near the near-empty half-window the old two charts stood in.
    #[gpui::test]
    async fn the_charts_are_compact(cx: &mut TestAppContext) {
        let (item, mut cx) = an_item_of_its_own(cx).await;
        let (_root_pid, _child_pid, metrics) = a_run();
        item.update(&mut cx, |item, cx| {
            item.set_reading_for_test(Some(metrics), None, cx);
        });
        draw(&mut cx);
        press_the_plaque(&mut cx);

        let processor = cx
            .debug_bounds("RUN-METRICS-CHART-CPU")
            .expect("the processor chart is drawn");
        let memory = cx
            .debug_bounds("RUN-METRICS-CHART-MEMORY")
            .expect("the memory chart is drawn");

        assert!(
            processor.size.height < px(90.),
            "a chart beside its numbers does not need a plot this tall: {:?}",
            processor.size.height
        );
        assert!(
            memory.size.height < px(90.),
            "and neither does the other one: {:?}",
            memory.size.height
        );
    }
}
