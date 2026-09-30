use std::time::Duration;

use gpui::{App, Entity, Task};
use task::TaskTemplate;
use terminal::{TaskStatus, Terminal};
use terminal_view::{TerminalView, terminal_panel::TerminalPanel};
use workspace::Workspace;

use crate::process_metrics::{self, Caught};

/// How long a caught process is given to end after each signal before the
/// next, firmer one is sent.
const GIVEN_TO_END: Duration = Duration::from_secs(3);
const LOOKED_AT_EVERY: Duration = Duration::from_millis(50);

/// Every terminal of this workspace still running `task`, wherever it was put:
/// the terminal panel or the centre of the window.
///
/// A run is matched by the hash of the template it was resolved from, or by its
/// label. The hash alone forgets a run the moment its configuration is edited,
/// since the edited one hashes differently -- and a run that is forgotten is not
/// stopped when the configuration is started again.
pub fn runs_of(workspace: &Workspace, task: &TaskTemplate, cx: &App) -> Vec<Entity<Terminal>> {
    let terminals = task_terminals(workspace, cx);
    let mut runs: Vec<Entity<Terminal>> = Vec::new();
    for terminal in terminals {
        let Some(state) = terminal.read(cx).task() else {
            continue;
        };
        let is_this_task = task.was_resolved_into(&state.spawned_task.id)
            || state.spawned_task.label == task.label;
        if state.status == TaskStatus::Running
            && is_this_task
            && runs
                .iter()
                .all(|run| run.entity_id() != terminal.entity_id())
        {
            runs.push(terminal);
        }
    }
    runs
}

/// Every terminal of this workspace that a task was started in, running or
/// not, wherever it was put: the terminal panel or the centre of the window.
pub fn task_terminals(workspace: &Workspace, cx: &App) -> Vec<Entity<Terminal>> {
    let mut terminals: Vec<Entity<Terminal>> = workspace
        .items(cx)
        .filter_map(|item| item.downcast::<TerminalView>())
        .map(|view| view.read(cx).terminal().clone())
        .collect();
    if let Some(panel) = workspace.panel::<TerminalPanel>(cx) {
        for pane in panel.read(cx).panes() {
            terminals.extend(
                pane.read(cx)
                    .items()
                    .filter_map(|item| item.downcast::<TerminalView>())
                    .map(|view| view.read(cx).terminal().clone()),
            );
        }
    }
    let mut unique: Vec<Entity<Terminal>> = Vec::new();
    for terminal in terminals {
        if terminal.read(cx).task().is_some()
            && unique
                .iter()
                .all(|known| known.entity_id() != terminal.entity_id())
        {
            unique.push(terminal);
        }
    }
    unique
}

/// Every task terminal of this workspace whose task is still running, whether
/// or not the run configurations list knows it: a run started from the tasks
/// modal, the gutter or an unsaved form is as much a run.
pub fn running_terminals(workspace: &Workspace, cx: &App) -> Vec<Entity<Terminal>> {
    task_terminals(workspace, cx)
        .into_iter()
        .filter(|terminal| {
            terminal
                .read(cx)
                .task()
                .is_some_and(|task| task.status == TaskStatus::Running)
        })
        .collect()
}

/// Stops a run and resolves only once nothing it started is left running,
/// with whether that is so: false when a caught process outlived `SIGKILL`.
pub fn stop_for_good(terminal: &Entity<Terminal>, cx: &mut App) -> Task<bool> {
    stop_all_for_good(vec![terminal.clone()], cx)
}

/// Stops every one of `terminals` and resolves only once nothing any of them
/// started is left running, with whether that is so.
///
/// The terminal ends its foreground process group and its shell. Whatever the
/// program put in a group or session of its own, or whatever takes its time
/// over the signal, would otherwise keep running beside the next start, holding
/// its port. So the whole trees are caught first, and what outlives the
/// terminals is sent `SIGTERM`, then `SIGKILL`.
///
/// The machine is looked at once for all of them, off the drawing thread, and
/// before any of them is ended: a process whose parent has just been ended is
/// handed to another parent, and is no longer found under its run.
pub fn stop_all_for_good(terminals: Vec<Entity<Terminal>>, cx: &mut App) -> Task<bool> {
    let roots: Vec<Vec<u32>> = terminals
        .iter()
        .map(|terminal| roots_of(terminal.read(cx)))
        .collect();
    let executor = cx.background_executor().clone();
    cx.spawn(async move |cx| {
        let caught = executor
            .spawn(async move { process_metrics::processes_under_each(&roots) })
            .await;
        let mut stopping = Vec::with_capacity(terminals.len());
        for (terminal, caught) in terminals.iter().zip(caught) {
            let gone = terminal.update(cx, |terminal, cx| {
                terminal.kill_active_task();
                terminal.wait_for_completed_task(cx)
            });
            stopping.push(finish_stopping(caught, gone, executor.clone()));
        }
        futures::future::join_all(stopping)
            .await
            .into_iter()
            .all(|ended| ended)
    })
}

async fn finish_stopping(
    caught: Vec<Caught>,
    gone: Task<Option<std::process::ExitStatus>>,
    executor: gpui::BackgroundExecutor,
) -> bool {
    if caught.is_empty() {
        gone.await;
        return true;
    }
    smol::future::or(
        async {
            gone.await;
        },
        executor.timer(GIVEN_TO_END),
    )
    .await;
    // Only a moment for the group the terminal has already sent `SIGKILL`
    // to: whatever is left after that was out of its reach.
    executor.timer(LOOKED_AT_EVERY).await;
    let left = process_metrics::still_running(&caught);
    if left.is_empty() {
        return true;
    }
    #[cfg(unix)]
    process_metrics::signal(&left, libc::SIGTERM);
    let left = until_ended(left, &executor).await;
    if left.is_empty() {
        return true;
    }
    #[cfg(unix)]
    process_metrics::signal(&left, libc::SIGKILL);
    let left = until_ended(left, &executor).await;
    if !left.is_empty() {
        log::error!("processes of a stopped run are still running: {left:?}");
    }
    left.is_empty()
}

fn roots_of(terminal: &Terminal) -> Vec<u32> {
    let mut roots = Vec::new();
    if let Some(getter) = terminal.pid_getter() {
        roots.push(getter.fallback_pid().as_u32());
    }
    if let Some(foreground) = terminal.pid() {
        roots.push(foreground.as_u32());
    }
    roots
}

/// Waits up to [`GIVEN_TO_END`] for the caught processes to end, and returns
/// those that did not.
async fn until_ended(caught: Vec<Caught>, executor: &gpui::BackgroundExecutor) -> Vec<Caught> {
    let mut left = process_metrics::still_running(&caught);
    let mut waited = Duration::ZERO;
    while !left.is_empty() && waited < GIVEN_TO_END {
        executor.timer(LOOKED_AT_EVERY).await;
        waited += LOOKED_AT_EVERY;
        left = process_metrics::still_running(&left);
    }
    left
}

/// Stops every run of `task` in the workspace, and resolves once all of them
/// are gone -- so whatever starts `task` next never runs beside an earlier one.
pub fn stop_every_run_of(workspace: &Workspace, task: &TaskTemplate, cx: &mut App) -> Task<bool> {
    stop_all_for_good(runs_of(workspace, task, cx), cx)
}
