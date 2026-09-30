use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use gpui::{
    App, Bounds, Context, Hsla, PathBuilder, Point, Subscription, Task, WeakEntity, Window, canvas,
    fill, point, prelude::*, size,
};
use settings::Settings;
use terminal::TaskStatus;
use ui::{ButtonLike, Tooltip, cyberpunk, prelude::*};
use workspace::{HideStatusItem, StatusItemView, Workspace, item::ItemHandle};

use crate::configurations_file::{self, Kind};
use crate::configurations_store;
use crate::goroutines::{self, GoroutineReading, GoroutineSource, Goroutines};
use crate::process_metrics::{self, Metrics, Sample, Watcher};
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
const AXIS_WIDTH: Pixels = px(72.);

const LINE_WIDTH: Pixels = px(1.5);

/// The status-bar plaque saying what the project's running configurations are
/// using: CPU and memory added up over every run, and the process count when
/// there is more than one, and the run count when there are several.
/// Pressing it opens the reading in full -- one tab per run, each with the two
/// minutes behind its numbers, which of its processes hold the memory, and the
/// facts that have no time series. Nothing at all is painted while nothing
/// runs.
pub struct RunMetricsStatusItem {
    workspace: WeakEntity<Workspace>,
    /// Every run going on, in the order the terminal panel holds them, which is
    /// the order they were started in.
    runs: Vec<WatchedRun>,
    /// Whether this window is the one in front. A poll nobody can see is a poll
    /// for nothing, so it stops the moment focus leaves this window and starts
    /// again the moment focus comes back.
    window_active: bool,
    _watching_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

/// One run and everything read about it, kept apart from the others: two
/// programs share a machine, but not a baseline for their processor rates, nor
/// their goroutines, nor their history.
struct WatchedRun {
    pid: u32,
    /// The label and command of the task that started it.
    label: String,
    command: Option<String>,
    /// The machine the run was sent to, when it runs over ssh: the process
    /// measured is then the local ssh client.
    remote: Option<String>,
    metrics: Option<Metrics>,
    /// The last [`READINGS_KEPT`] readings, oldest first. The charts draw only
    /// these, so a run a few seconds old draws a few seconds.
    readings: VecDeque<Reading>,
    watcher: Watcher,
    goroutines: Option<crate::goroutines::GoroutineReading>,
    /// When the goroutines reading was last refreshed. A debugger round trip
    /// or an HTTP request costs far more than the process-metrics poll that
    /// drives this loop, so it is not worth paying every tick of it.
    last_goroutines_poll: Option<Instant>,
    /// The process of the run that is a Go program, when there is one. A run
    /// is often a script or a shell that starts it.
    go_program: Option<u32>,
    /// Which of the run's processes were found to be Go programs, so a binary
    /// is looked at once per process rather than on every poll.
    known_programs: HashMap<u32, bool>,
    /// A pprof fetch in flight. Dropping it -- because the run ended, or
    /// another reading started first -- cancels it, so a slow answer from a
    /// run that is already gone never lands on top of what came after it.
    _goroutines_task: Option<Task<()>>,
}

impl WatchedRun {
    fn new(context: &RunContext) -> Self {
        Self {
            pid: context.pid,
            label: context.label.clone(),
            command: context.command.clone(),
            remote: context.remote.clone(),
            metrics: None,
            readings: VecDeque::new(),
            watcher: Watcher::default(),
            goroutines: None,
            last_goroutines_poll: None,
            go_program: None,
            known_programs: HashMap::new(),
            _goroutines_task: None,
        }
    }

    /// Replaces the reading, saying whether it actually changed -- so a poll
    /// that read the same thing again does not trigger a repaint for nothing.
    fn set_goroutines(&mut self, reading: Option<GoroutineReading>) -> bool {
        if self.goroutines == reading {
            return false;
        }
        self.goroutines = reading;
        true
    }

    fn find_go_program(&mut self) -> Option<u32> {
        let tree = &self.metrics.as_ref()?.tree;
        let known = &mut self.known_programs;
        known.retain(|pid, _| tree.iter().any(|process| process.pid == *pid));
        tree.iter().map(|process| process.pid).find(|pid| {
            *known
                .entry(*pid)
                .or_insert_with(|| goroutines::is_go_program(*pid))
        })
    }
}

/// What a window that draws one run in full needs of it.
#[derive(Clone)]
pub(crate) struct RunReading {
    /// The run's root process, which is what tells one run from another.
    pub pid: u32,
    /// The label of the task that started it.
    pub label: String,
    pub metrics: Metrics,
    /// What the processor read and how much memory was held, oldest first.
    pub series: Vec<(Option<f32>, u64)>,
    /// The run's goroutines, when it is a Go program. None when it is not one,
    /// or nothing has been read yet.
    pub goroutines: Option<crate::goroutines::GoroutineReading>,
    /// The run's process that is a Go program, if one is.
    pub go_program: Option<u32>,
    /// The machine the run was sent to, when it runs over ssh.
    pub remote: Option<String>,
}

/// What is needed about a run to decide where its goroutines, if any, come
/// from: the process to measure, and the label and command of the task that
/// started it.
#[derive(Clone)]
struct RunContext {
    pid: u32,
    label: String,
    command: Option<String>,
    /// The machine the run was sent to, when it runs over ssh.
    remote: Option<String>,
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
    /// Starts watching the project's running configurations the moment the
    /// status bar is built: runs already going should be reported at once,
    /// not a second after the bar is first drawn.
    pub fn new(workspace: &Workspace, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let subscriptions = vec![
            cx.observe_window_activation(window, Self::window_activation_changed),
            // Turning the reading off by setting should stop the poll right
            // away, not wait for the window to lose and regain focus first.
            cx.observe_global::<settings::SettingsStore>(|item, cx| item.watch_the_runs(cx)),
        ];
        let mut item = Self {
            workspace: workspace.weak_handle(),
            runs: Vec::new(),
            window_active: window.is_window_active(),
            _watching_task: None,
            _subscriptions: subscriptions,
        };
        item.watch_the_runs(cx);
        item
    }

    /// The window this item lives in gained or lost focus. Losing it is
    /// exactly the moment nobody can see the reading, so the poll is stopped
    /// along with it; gaining it back starts the poll again.
    fn window_activation_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.window_active = window.is_window_active();
        self.watch_the_runs(cx);
    }

    /// Reads what the runs are using, once a second, for as long as this item
    /// is on screen, its window has focus, and the reader has not turned the
    /// reading off. The reading itself happens off the drawing thread: `/proc`
    /// holds a few hundred files and none of that belongs in a frame, and it
    /// is read once for all the runs rather than once for each.
    fn watch_the_runs(&mut self, cx: &mut Context<Self>) {
        if !self.window_active || !RunConfigurationsSettings::get_global(cx).show_process_metrics {
            // Neither of these means the runs themselves stopped, but nobody can
            // see the reading right now, or the reader turned it off -- either
            // way it is not worth keeping stale numbers around for.
            self._watching_task = None;
            self.runs.clear();
            return;
        }
        self._watching_task = Some(cx.spawn(async move |item, cx| {
            loop {
                let Ok(contexts) = item.read_with(cx, |item, cx| item.run_contexts(cx)) else {
                    return;
                };
                let roots: Vec<u32> = contexts.iter().map(|context| context.pid).collect();
                let read = match contexts.is_empty() {
                    false => {
                        cx.background_spawn(async move {
                            let mut everything = process_metrics::everything_running();
                            if let Some(everything) = everything.as_mut() {
                                process_metrics::read_threads_under(everything, &roots);
                            }
                            (everything, process_metrics::machine_uptime())
                        })
                        .await
                    }
                    true => (None, None),
                };
                let (samples, machine_uptime) = read;
                let now = Instant::now();
                if item
                    .update(cx, |item, cx| {
                        let mut changed =
                            item.read_the_runs(&contexts, samples.as_deref(), now, machine_uptime);
                        if item.poll_goroutines(cx) {
                            changed = true;
                        }
                        if changed {
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

    /// One reading of every run in `contexts`. `samples` is every process the
    /// machine talked about, or nothing when it did not answer. Says whether
    /// anything changed.
    ///
    /// A run the machine has nothing to say about is over, and the reading
    /// says so. A machine that did not answer at all leaves the readings as
    /// they were, rather than reporting running things as gone. A run that
    /// is no longer in `contexts` is forgotten, with its history and its
    /// goroutines; one that is new starts from nothing.
    fn read_the_runs(
        &mut self,
        contexts: &[RunContext],
        samples: Option<&[Sample]>,
        now: Instant,
        machine_uptime: Option<std::time::Duration>,
    ) -> bool {
        let mut changed = false;
        let mut kept = Vec::with_capacity(contexts.len());
        // The runs already known stay where they were, so the tabs of a
        // window somebody is reading do not swap places under the pointer.
        for mut run in std::mem::take(&mut self.runs) {
            match contexts.iter().find(|context| context.pid == run.pid) {
                Some(context) => {
                    run.label.clone_from(&context.label);
                    run.command.clone_from(&context.command);
                    kept.push(run);
                }
                None => changed = true,
            }
        }
        // Those seen for the first time, in the order the machine numbered them:
        // several started within one poll are found in the order the terminal
        // panel holds them, which is not the order they began in.
        let mut fresh: Vec<&RunContext> = contexts
            .iter()
            .filter(|context| !kept.iter().any(|run| run.pid == context.pid))
            .collect();
        fresh.sort_by_key(|context| context.pid);
        for context in fresh {
            kept.push(WatchedRun::new(context));
            changed = true;
        }
        self.runs = kept;

        let Some(samples) = samples else {
            return changed;
        };
        for run in &mut self.runs {
            let read = run
                .watcher
                .metrics_of(run.pid, samples, now, machine_uptime);
            match &read {
                Some(metrics) => {
                    if run.readings.len() >= READINGS_KEPT {
                        run.readings.pop_front();
                    }
                    run.readings.push_back(Reading {
                        cpu: metrics.cpu,
                        memory: metrics.memory,
                    });
                }
                None => run.readings.clear(),
            }
            if read != run.metrics {
                changed = true;
            }
            run.metrics = read;
        }
        changed
    }

    /// One reading of a single run, or of none when `pid` is `None`.
    #[cfg(test)]
    fn read_the_run(
        &mut self,
        pid: Option<u32>,
        samples: Option<&[Sample]>,
        now: Instant,
        machine_uptime: Option<std::time::Duration>,
    ) -> bool {
        let contexts: Vec<RunContext> = pid
            .map(|pid| RunContext {
                pid,
                label: "a run".to_string(),
                command: None,
                remote: None,
            })
            .into_iter()
            .collect();
        self.read_the_runs(&contexts, samples, now, machine_uptime)
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

    /// What the last reading said about every run that has one, in the order
    /// the runs were started, for a window that draws them in full.
    pub(crate) fn runs(&self) -> Vec<RunReading> {
        self.runs
            .iter()
            .filter_map(|run| {
                Some(RunReading {
                    pid: run.pid,
                    label: run.label.clone(),
                    metrics: run.metrics.clone()?,
                    series: run
                        .readings
                        .iter()
                        .map(|reading| (reading.cpu, reading.memory))
                        .collect(),
                    goroutines: run.goroutines.clone(),
                    go_program: run.go_program,
                    remote: run.remote.clone(),
                })
            })
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn set_remote_for_test(&mut self, machine: Option<&str>, cx: &mut Context<Self>) {
        if let Some(run) = self.runs.last_mut() {
            run.remote = machine.map(str::to_string);
        }
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn set_go_program_for_test(&mut self, pid: Option<u32>, cx: &mut Context<Self>) {
        if let Some(run) = self.runs.last_mut() {
            run.go_program = pid;
        }
        cx.notify();
    }

    /// Puts a single run, with this reading, in place of whatever was there.
    #[cfg(test)]
    pub(crate) fn set_reading_for_test(
        &mut self,
        metrics: Option<Metrics>,
        goroutines: Option<crate::goroutines::GoroutineReading>,
        cx: &mut Context<Self>,
    ) {
        self.runs.clear();
        if let Some(metrics) = metrics {
            let mut run = WatchedRun::new(&RunContext {
                pid: metrics.pid,
                label: "a run".to_string(),
                command: None,
                remote: None,
            });
            run.metrics = Some(metrics);
            run.goroutines = goroutines;
            self.runs.push(run);
        }
        cx.notify();
    }

    /// Puts these runs in place of whatever was there: a label, a reading and a
    /// history of memory readings each.
    #[cfg(test)]
    pub(crate) fn set_runs_for_test(
        &mut self,
        runs: Vec<(String, Metrics, Vec<u64>)>,
        cx: &mut Context<Self>,
    ) {
        self.runs = runs
            .into_iter()
            .map(|(label, metrics, memory)| {
                let mut run = WatchedRun::new(&RunContext {
                    pid: metrics.pid,
                    label,
                    command: None,
                    remote: None,
                });
                run.readings = memory
                    .into_iter()
                    .map(|memory| Reading { cpu: None, memory })
                    .collect();
                run.metrics = Some(metrics);
                run
            })
            .collect();
        cx.notify();
    }

    /// Every task run going on in this workspace, and what started each: a
    /// task terminal that is still running, wherever it was put -- the
    /// terminal panel or the centre of the window. One that has ended keeps
    /// its tab, but it is not a run any more, and reading it would keep the
    /// poll going for nothing.
    fn run_contexts(&self, cx: &App) -> Vec<RunContext> {
        let Some(workspace) = self.workspace.upgrade() else {
            return Vec::new();
        };
        let mut contexts: Vec<RunContext> = Vec::new();
        for terminal in crate::run_instances::task_terminals(workspace.read(cx), cx) {
            let terminal = terminal.read(cx);
            let (Some(task), Some(pid)) = (terminal.task(), terminal.pid()) else {
                continue;
            };
            let pid = pid.as_u32();
            if task.status != TaskStatus::Running
                || contexts.iter().any(|context| context.pid == pid)
            {
                continue;
            }
            contexts.push(RunContext {
                pid,
                label: task.spawned_task.full_label.clone(),
                command: task.spawned_task.command.clone(),
                remote: crate::over_ssh::destination_of(
                    task.spawned_task.command.as_deref(),
                    &task.spawned_task.args,
                ),
            });
        }
        // A program run through the debugger is a process like any other, but
        // its output goes to the debugger and no terminal of a task holds it.
        for session in crate::run_instances::live_sessions(workspace.read(cx), cx) {
            let session = session.read(cx);
            let Some(pid) = session.debuggee_process_id() else {
                continue;
            };
            if contexts.iter().any(|context| context.pid == pid) {
                continue;
            }
            contexts.push(RunContext {
                pid,
                label: session
                    .label()
                    .map(|label| label.to_string())
                    .unwrap_or_else(|| "debug session".to_string()),
                command: None,
                remote: None,
            });
        }
        contexts
    }

    /// Refreshes the goroutines reading of every run, each at most once every
    /// [`goroutines::POLL_INTERVAL`]. Says whether a reading changed right
    /// away; a pprof fetch that is still on its way notifies on its own once
    /// it comes back.
    fn poll_goroutines(&mut self, cx: &mut Context<Self>) -> bool {
        let mut changed = false;
        for at in 0..self.runs.len() {
            if self.poll_goroutines_of(at, cx) {
                changed = true;
            }
        }
        changed
    }

    fn poll_goroutines_of(&mut self, at: usize, cx: &mut Context<Self>) -> bool {
        let now = Instant::now();
        let run_count = self.runs.len();
        let (pid, label, command) = {
            let run = &mut self.runs[at];
            if run
                .last_goroutines_poll
                .is_some_and(|last| now.duration_since(last) < goroutines::POLL_INTERVAL)
            {
                return false;
            }
            run.last_goroutines_poll = Some(now);
            run.go_program = run.find_go_program();
            (run.pid, run.label.clone(), run.command.clone())
        };

        if let Some(reading) = self.debugger_goroutines(&label, run_count, cx) {
            let run = &mut self.runs[at];
            run._goroutines_task = None;
            return run.set_goroutines(Some(reading));
        }

        if let Some(address) = self.configured_pprof_address(&label, cx) {
            let http_client = cx.http_client();
            self.runs[at]._goroutines_task = Some(cx.spawn(async move |item, cx| {
                let reading = goroutines::read_pprof(http_client, &address).await;
                item.update(cx, |item, cx| {
                    let Some(run) = item.runs.iter_mut().find(|run| run.pid == pid) else {
                        return;
                    };
                    if run.set_goroutines(Some(reading)) {
                        cx.notify();
                    }
                })
                .ok();
            }));
            return false;
        }

        self.runs[at]._goroutines_task = None;
        let looks_like_go = command
            .as_deref()
            .is_some_and(goroutines::looks_like_go_command)
            || self.runs[at].go_program.is_some()
            || self.configured_adapter_is_delve(&label, cx);
        let reading = looks_like_go
            .then(|| GoroutineReading::Unavailable(goroutines::no_reader_configured()));
        self.runs[at].set_goroutines(reading)
    }

    /// The run's goroutines as the debugger sees them: the threads of a
    /// running Delve session, Delve being the only debugger that stands for
    /// Go's own goroutines. The session is the one named like the run; a
    /// session named otherwise is taken for the run's only when the run is the
    /// only one going and the session is the only Delve one, since with
    /// several of either there is no telling whose it is, and another run's
    /// goroutines are worse than none. `None` when no such session is
    /// running, not when one is running and reports zero -- a zero from a
    /// session that has not answered yet would read as "there are none".
    fn debugger_goroutines(
        &self,
        label: &str,
        run_count: usize,
        cx: &mut Context<Self>,
    ) -> Option<GoroutineReading> {
        let workspace = self.workspace.upgrade()?;
        let project = workspace.read(cx).project().clone();
        let dap_store = project.read(cx).dap_store();
        let mut delve_sessions = Vec::new();
        for session in dap_store.read(cx).sessions() {
            let (is_delve, named_like_the_run) = {
                let session = session.read(cx);
                (
                    !session.is_terminated() && session.adapter().as_ref() == "Delve",
                    session
                        .label()
                        .is_some_and(|session_label| session_label.as_ref() == label),
                )
            };
            if is_delve {
                delve_sessions.push((session.clone(), named_like_the_run));
            }
        }
        let session = match delve_sessions
            .iter()
            .find(|(_, named_like_the_run)| *named_like_the_run)
        {
            Some((session, _)) => session.clone(),
            None if run_count == 1 && delve_sessions.len() == 1 => delve_sessions[0].0.clone(),
            None => return None,
        };
        let total = session.update(cx, |session, cx| session.threads(cx).len());
        Some(GoroutineReading::Read(Goroutines {
            total,
            by_state: Vec::new(),
            source: GoroutineSource::Debugger,
        }))
    }

    /// The pprof address the run configuration named `label` gives, if the
    /// project has a saved task by that name and it names one.
    fn configured_pprof_address(&self, label: &str, cx: &mut Context<Self>) -> Option<String> {
        let workspace = self.workspace.upgrade()?;
        let project = workspace.read(cx).project().clone();
        let store = configurations_store::store_for(&project, cx);
        store
            .read(cx)
            .of_kind(Kind::Task)
            .configurations
            .iter()
            .find(|configuration| configuration.label == label)
            .and_then(|configuration| configurations_file::pprof_of(&configuration.as_written))
    }

    /// Whether a saved debug configuration named `label` debugs with Delve --
    /// the cheap half of "this looks like a Go program" for a run that is not
    /// under the debugger right now.
    fn configured_adapter_is_delve(&self, label: &str, cx: &mut Context<Self>) -> bool {
        let Some(workspace) = self.workspace.upgrade() else {
            return false;
        };
        let project = workspace.read(cx).project().clone();
        let store = configurations_store::store_for(&project, cx);
        store
            .read(cx)
            .of_kind(Kind::Debug)
            .configurations
            .iter()
            .any(|configuration| {
                configuration.label == label
                    && configuration
                        .scenario
                        .as_ref()
                        .is_some_and(|scenario| scenario.adapter.as_ref() == "Delve")
            })
    }
}

/// What every run added together comes to, for the plaque.
struct Totals {
    runs: usize,
    processes: usize,
    /// Added over the runs that have a rate yet; `None` when none has.
    cpu: Option<f32>,
    memory: u64,
}

fn totals_of(runs: &[WatchedRun]) -> Option<Totals> {
    let read: Vec<&Metrics> = runs.iter().filter_map(|run| run.metrics.as_ref()).collect();
    if read.is_empty() {
        return None;
    }
    let cpu = read
        .iter()
        .filter_map(|metrics| metrics.cpu)
        .fold(None, |sum, cpu| Some(sum.unwrap_or(0.) + cpu));
    Some(Totals {
        runs: read.len(),
        processes: read.iter().map(|metrics| metrics.processes).sum(),
        cpu,
        memory: read.iter().map(|metrics| metrics.memory).sum(),
    })
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
                        .size(LabelSize::Default)
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
                                    .size(LabelSize::Default)
                                    .color(Color::Muted)
                                    .single_line(),
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

impl Render for RunMetricsStatusItem {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(totals) = totals_of(&self.runs) else {
            return div().into_any_element();
        };

        let cpu = match totals.cpu {
            Some(cpu) => format!("{cpu:.1}%"),
            None => "-- reading".to_string(),
        };
        let (runs, processes) = (totals.runs, totals.processes);

        let plaque = ButtonLike::new("run-metrics-plaque")
            .style(cyberpunk::Rank::Quiet.style())
            .size(ButtonSize::Compact)
            .child(
                h_flex()
                    .debug_selector(|| "run-metrics-status".to_string())
                    .gap_2()
                    .items_center()
                    .when(runs > 1, |row| row.child(said("runs", runs.to_string())))
                    .child(said("CPU", cpu))
                    .child(said("RAM", process_metrics::as_memory(totals.memory)))
                    .when(processes > 1, |row| {
                        row.child(said("processes", processes.to_string()))
                    }),
            );

        let tooltip = match runs > 1 {
            true => "Show what each of the runs is using",
            false => "Show what the run is using",
        };
        plaque
            .tooltip(Tooltip::text(tooltip))
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
            thread_samples: Vec::new(),
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

    /// The reading of a run whose network and video memory the machine will not
    /// report still opens, and still says what it does know.
    #[gpui::test]
    async fn the_reading_opens_for_a_machine_that_will_not_say(cx: &mut TestAppContext) {
        let (item, mut cx) = an_item_of_its_own(cx).await;
        a_run_on_screen(&item, &mut cx, 4242);
        item.read_with(&cx, |item, _| {
            let metrics = item.runs[0].metrics.as_ref().expect("the run was read");
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

    /// Every process of a wide run has a row of its own, and a window this tall
    /// shows the ninth-largest without the reader scrolling for it.
    #[gpui::test]
    async fn a_tall_window_lists_every_process(cx: &mut TestAppContext) {
        let (item, mut cx) = an_item_of_its_own(cx).await;
        cx.simulate_resize(size(px(1200.), px(900.)));
        a_wide_run_on_screen(&item, &mut cx);
        press_the_plaque(&mut cx);

        let window = cx
            .debug_bounds("RUN-METRICS-MODAL")
            .expect("the reading is open");
        for row in [
            "RUN-METRICS-PROCESS-4266",
            "RUN-METRICS-PROCESS-4258",
            "RUN-METRICS-PROCESS-4243",
        ] {
            assert!(
                cx.debug_bounds(row).is_some(),
                "every process of the run has a row: {row} has none"
            );
        }
        let ninth = cx
            .debug_bounds("RUN-METRICS-PROCESS-4250")
            .expect("the ninth process has a row");
        assert!(
            ninth.origin.y >= window.origin.y && ninth.bottom() <= window.bottom(),
            "the ninth row is drawn inside the window it was given: {ninth:?} in {window:?}"
        );
    }

    /// The tree is drawn on the same page as the charts: no tab to press first.
    #[gpui::test]
    async fn the_run_is_drawn_as_a_tree_beside_the_charts(cx: &mut TestAppContext) {
        let (item, mut cx) = an_item_of_its_own(cx).await;
        a_tree_on_screen(&item, &mut cx);
        press_the_plaque(&mut cx);

        assert!(
            cx.debug_bounds("RUN-METRICS-CHART-CPU").is_some(),
            "the charts are drawn"
        );
        for row in [
            "RUN-METRICS-PROCESS-4242",
            "RUN-METRICS-PROCESS-4243",
            "RUN-METRICS-PROCESS-4244",
        ] {
            assert!(
                cx.debug_bounds(row).is_some(),
                "and every process of the run has a row beside them: {row} has none"
            );
        }
    }

    /// Depth is what the tree says, and it says it by where a row starts. A
    /// child stands in from its parent, and a grandchild further still.
    #[gpui::test]
    async fn a_child_stands_in_from_the_process_that_started_it(cx: &mut TestAppContext) {
        let (item, mut cx) = an_item_of_its_own(cx).await;
        a_tree_on_screen(&item, &mut cx);
        press_the_plaque(&mut cx);

        let at = |name: &'static str, cx: &mut VisualTestContext| {
            cx.debug_bounds(name)
                .unwrap_or_else(|| panic!("{name} is drawn"))
                .origin
                .x
        };
        let root = at("RUN-METRICS-PROCESS-NAME-4242", &mut cx);
        let child = at("RUN-METRICS-PROCESS-NAME-4243", &mut cx);
        let grandchild = at("RUN-METRICS-PROCESS-NAME-4244", &mut cx);

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

        cx.simulate_resize(size(px(480.), px(900.)));
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
        let first_process = cx
            .debug_bounds("RUN-METRICS-PROCESS-4242")
            .expect("the run's own process is listed");
        assert!(
            first_process.origin.y >= memory.bottom(),
            "the dropped chart pushes the process list down rather than covering it: \
             {first_process:?} against {memory:?}"
        );
    }

    fn context(pid: u32, label: &str) -> RunContext {
        RunContext {
            pid,
            label: label.to_string(),
            command: None,
            remote: None,
        }
    }

    /// Three runs, and the machine's processes: each run is read from its own
    /// root, with its own children, and none of the numbers spill into another.
    #[gpui::test]
    async fn every_run_is_read_from_its_own_process_tree(cx: &mut TestAppContext) {
        let (item, mut cx) = an_item_of_its_own(cx).await;
        let megabyte = 1024 * 1024;
        let machine = [
            started_by(4242, 1, "the api", 10 * megabyte),
            started_by(4243, 4242, "a helper", 5 * megabyte),
            started_by(4300, 1, "the worker", 20 * megabyte),
            started_by(4400, 1, "the site", 40 * megabyte),
            started_by(4401, 4400, "a bundler", 40 * megabyte),
            started_by(4402, 4400, "a watcher", megabyte),
        ];
        let contexts = [
            context(4242, "api"),
            context(4300, "worker"),
            context(4400, "site"),
        ];
        item.update(&mut cx, |item, _| {
            assert!(item.read_the_runs(&contexts, Some(&machine), Instant::now(), None));
            let runs = item.runs();
            let seen: Vec<_> = runs
                .iter()
                .map(|run| {
                    (
                        run.label.as_str(),
                        run.pid,
                        run.metrics.processes,
                        run.metrics.memory / megabyte,
                    )
                })
                .collect();
            assert_eq!(
                seen,
                vec![
                    ("api", 4242, 2, 15),
                    ("worker", 4300, 1, 20),
                    ("site", 4400, 3, 81)
                ],
                "each run counts its own processes and memory, in the order the runs started"
            );
        });
    }

    /// A run that is gone from the list is forgotten with its history; one
    /// whose process the machine no longer knows has no reading; and a machine
    /// that says nothing leaves what was read as it was.
    #[gpui::test]
    async fn a_run_that_ends_is_forgotten_and_a_silent_machine_forgets_nothing(
        cx: &mut TestAppContext,
    ) {
        let (item, mut cx) = an_item_of_its_own(cx).await;
        let machine = [
            named(4242, "the api", 8 << 20),
            named(4300, "the worker", 8 << 20),
        ];
        let both = [context(4242, "api"), context(4300, "worker")];
        let at = Instant::now();
        item.update(&mut cx, |item, _| {
            item.read_the_runs(&both, Some(&machine), at, None);
            item.read_the_runs(&both, Some(&machine), at + Watcher::HOW_OFTEN, None);
            assert_eq!(item.runs().len(), 2);
            assert_eq!(item.runs()[0].series.len(), 2, "history is kept per run");

            assert!(
                !item.read_the_runs(&both, None, at + Watcher::HOW_OFTEN * 2, None),
                "a machine that did not answer changes nothing"
            );
            assert_eq!(item.runs().len(), 2);

            // The api's process is gone from the machine, and the worker's run
            // is what the panel still holds.
            let only_worker = [named(4300, "the worker", 8 << 20)];
            assert!(item.read_the_runs(
                &both,
                Some(&only_worker),
                at + Watcher::HOW_OFTEN * 3,
                None
            ));
            let labels: Vec<_> = item.runs().iter().map(|run| run.label.clone()).collect();
            assert_eq!(
                labels,
                ["worker"],
                "a run the machine no longer knows has no reading"
            );

            // The api's terminal is closed altogether.
            assert!(item.read_the_runs(
                &[context(4300, "worker")],
                Some(&only_worker),
                at + Watcher::HOW_OFTEN * 4,
                None
            ));
            assert_eq!(item.runs().len(), 1);
            assert_eq!(
                item.runs()[0].series.len(),
                4,
                "the worker's own history went on"
            );
        });
    }

    /// The plaque adds the runs up: memory, processes and (where there is a rate)
    /// processor, and says how many runs there are.
    #[test]
    fn the_plaque_adds_every_run_up() {
        let read = |pid: u32, processes: usize, cpu: Option<f32>, memory: u64| WatchedRun {
            metrics: Some(Metrics {
                pid,
                processes,
                cpu,
                memory,
                network: Err("no"),
                video_memory: Err("no"),
                threads: 1,
                uptime: None,
                tree: Vec::new(),
            }),
            ..WatchedRun::new(&context(pid, "a run"))
        };
        let runs = vec![
            read(1, 2, Some(10.), 100),
            read(2, 1, None, 50),
            read(3, 3, Some(2.5), 25),
            WatchedRun::new(&context(4, "not read yet")),
        ];
        let totals = totals_of(&runs).expect("three runs have a reading");
        assert_eq!(totals.runs, 3, "a run with no reading yet is not counted");
        assert_eq!(totals.processes, 6);
        assert_eq!(totals.memory, 175);
        assert_eq!(
            totals.cpu,
            Some(12.5),
            "added over the runs that have a rate"
        );

        assert!(totals_of(&[WatchedRun::new(&context(4, "not read yet"))]).is_none());
        let unrated = vec![read(1, 1, None, 1)];
        assert_eq!(totals_of(&unrated).and_then(|totals| totals.cpu), None);
    }

    /// Several runs are one plaque, with the number of runs in it.
    #[gpui::test]
    async fn several_runs_are_one_plaque_that_says_how_many(cx: &mut TestAppContext) {
        let (item, mut cx) = an_item_of_its_own(cx).await;
        let machine = [
            named(4242, "the api", 8 << 20),
            named(4300, "the worker", 8 << 20),
            named(4400, "the site", 8 << 20),
        ];
        let contexts = [
            context(4242, "api"),
            context(4300, "worker"),
            context(4400, "site"),
        ];
        item.update(&mut cx, |item, _| {
            item.read_the_runs(&contexts, Some(&machine), Instant::now(), None);
        });
        draw(&mut cx);
        let with_three = cx
            .debug_bounds("run-metrics-status")
            .expect("the plaque is painted for three runs")
            .size
            .width;

        item.update(&mut cx, |item, _| {
            item.read_the_runs(&contexts[..1], Some(&machine), Instant::now(), None);
        });
        draw(&mut cx);
        let with_one = cx
            .debug_bounds("run-metrics-status")
            .expect("the plaque is painted for one run")
            .size
            .width;
        assert!(
            with_three > with_one,
            "the plaque of several runs carries their number as well: {with_three:?} against {with_one:?}"
        );
    }

    /// The runs already known keep their places when others begin or end, so
    /// the tabs of a window somebody is reading do not swap under the pointer,
    /// whatever order the terminals are found in.
    #[gpui::test]
    async fn runs_keep_the_order_they_were_first_seen_in(cx: &mut TestAppContext) {
        let (item, mut cx) = an_item_of_its_own(cx).await;
        let machine = [
            named(4242, "the api", 8 << 20),
            named(4300, "the worker", 8 << 20),
            named(4400, "the site", 8 << 20),
        ];
        let labels = |item: &RunMetricsStatusItem| -> Vec<String> {
            item.runs().into_iter().map(|run| run.label).collect()
        };
        item.update(&mut cx, |item, _| {
            item.read_the_runs(
                &[context(4300, "worker")],
                Some(&machine),
                Instant::now(),
                None,
            );
            // Two that are new, found in the opposite order to their numbers.
            item.read_the_runs(
                &[
                    context(4400, "site"),
                    context(4300, "worker"),
                    context(4242, "api"),
                ],
                Some(&machine),
                Instant::now(),
                None,
            );
            assert_eq!(
                labels(item),
                ["worker", "api", "site"],
                "the worker was there first; the two new ones follow in the order they began"
            );
        });
    }

    /// A program run through the debugger is a run of its own: the debugger says
    /// which process it is, and its numbers are read like any other run's. A
    /// session that has not said yet, or has ended, is not one.
    #[gpui::test]
    async fn a_debug_session_with_a_process_is_a_run(cx: &mut TestAppContext) {
        let (item, mut cx) = an_item_of_its_own(cx).await;
        let project = item.read_with(&cx, |item, cx| {
            item.workspace
                .upgrade()
                .expect("open")
                .read(cx)
                .project()
                .clone()
        });
        let new_session = |label: &str, cx: &mut VisualTestContext| {
            project.update(cx, |project, cx| {
                project.dap_store().update(cx, |dap_store, cx| {
                    dap_store.new_session(
                        Some(label.to_string().into()),
                        dap::adapters::DebugAdapterName("Delve".into()),
                        task::SharedTaskContext::default(),
                        None,
                        Default::default(),
                        cx,
                    )
                })
            })
        };
        let this_process = std::process::id();
        let running = new_session("Debug API", &mut cx);
        running.update(&mut cx, |session, _| {
            session.set_debuggee_process_id_for_test(Some(this_process))
        });
        let _not_said_yet = new_session("Debug worker", &mut cx);
        let ended = new_session("Debug site", &mut cx);
        ended.update(&mut cx, |session, cx| {
            session.set_debuggee_process_id_for_test(Some(this_process + 1));
            session.shutdown(cx).detach();
        });

        let contexts = item.read_with(&cx, |item, cx| item.run_contexts(cx));
        assert_eq!(
            contexts
                .iter()
                .map(|context| (context.label.as_str(), context.pid))
                .collect::<Vec<_>>(),
            [("Debug API", this_process)],
            "only the session that named its process, and is alive, is a run"
        );

        let Some(machine) = crate::process_metrics::everything_running() else {
            return;
        };
        item.update(&mut cx, |item, _| {
            item.read_the_runs(&contexts, Some(&machine), Instant::now(), None);
            let runs = item.runs();
            assert_eq!(runs.len(), 1);
            assert_eq!(runs[0].label, "Debug API");
            assert!(runs[0].pid == this_process);
        });
    }

    /// A task terminal placed in the centre of the window is a run like one in
    /// the terminal panel; one whose task has ended is not a run any more.
    #[gpui::test]
    async fn every_running_task_terminal_is_a_run_and_an_ended_one_is_not(cx: &mut TestAppContext) {
        let (item, mut cx) = an_item_of_its_own(cx).await;
        cx.background_executor.allow_parking();
        let workspace = item.read_with(&cx, |item, _| item.workspace.upgrade().expect("open"));
        let project = workspace.read_with(&cx, |workspace, _| workspace.project().clone());

        let mut terminals = Vec::new();
        for (label, program, args) in [
            ("server", "sleep", "60"),
            ("worker", "sleep", "61"),
            ("quick", "true", ""),
        ] {
            let template = task::TaskTemplate {
                label: label.to_string(),
                command: program.to_string(),
                args: args.split_whitespace().map(|arg| arg.to_string()).collect(),
                ..Default::default()
            };
            let mut spawned = template
                .resolve_task("run configurations", &task::TaskContext::default())
                .expect("the template resolves against an empty context")
                .resolved;
            spawned.cwd = Some(std::env::temp_dir());
            let terminal = project
                .update(&mut cx, |project, cx| {
                    project.create_terminal_task(spawned, cx)
                })
                .await
                .expect("the run starts");
            let view = cx.update(|window, cx| {
                cx.new(|cx| {
                    terminal_view::TerminalView::new(
                        terminal.clone(),
                        workspace.downgrade(),
                        None,
                        project.downgrade(),
                        window,
                        cx,
                    )
                })
            });
            let pane = workspace.read_with(&cx, |workspace, _| workspace.active_pane().clone());
            pane.update_in(&mut cx, |pane, window, cx| {
                pane.add_item(Box::new(view), false, false, None, window, cx);
            });
            terminals.push(terminal);
        }

        let mut found = Vec::new();
        for _ in 0..300 {
            cx.run_until_parked();
            let ended = terminals[2].read_with(&cx, |terminal, _| {
                terminal
                    .task()
                    .is_some_and(|task| task.status != terminal::TaskStatus::Running)
            });
            found = item.read_with(&cx, |item, cx| {
                item.run_contexts(cx)
                    .into_iter()
                    .map(|context| context.label)
                    .collect::<Vec<_>>()
            });
            if ended && found.len() >= 2 {
                break;
            }
            cx.background_executor
                .timer(std::time::Duration::from_millis(20))
                .await;
        }
        found.sort();
        assert_eq!(
            found,
            ["server", "worker"],
            "both running tasks are runs, wherever they stand, and the one that ended is not"
        );

        for terminal in &terminals {
            terminal.update(&mut cx, |terminal, _| terminal.kill_active_task());
        }
    }
}
