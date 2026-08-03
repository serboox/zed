use std::time::Duration;

use gpui::{AnyElement, App, Context, FocusHandle, Focusable as _, Window};
use ui::{CommonAnimationExt, ProgressBar, prelude::*};

use crate::{Workspace, WorkspaceLoadPhase};

/// How long start-up work may run before the reader is shown a panel about it.
/// A small project finishes well inside this, so opening one does not flash a
/// panel over a window that is already usable.
const BEFORE_SAYING_ANYTHING: Duration = Duration::from_millis(500);

/// How often the panel refreshes while it is up. The numbers on it -- files
/// found, seconds waited -- change without anyone notifying the workspace, so
/// nothing else would bring them up to date.
const WHILE_WAITING: Duration = Duration::from_millis(250);

/// A window is painted long before the project inside it is ready: the tree is
/// empty, the tab bar is empty, and nothing says whether that is a project still
/// arriving or a project that failed to open. This keeps the reader informed and
/// keeps their keystrokes and clicks out of a half-built workspace until it is.
#[derive(Default)]
pub(crate) struct LoadingReport {
    focus_handle: Option<FocusHandle>,
    watching: bool,
    /// Set once [`BEFORE_SAYING_ANYTHING`] has passed with work still in flight.
    speaking: bool,
    /// The reader asked to be let through. Holds until this load finishes, so
    /// the panel does not come back a moment later.
    waved_through: bool,
    waited: Duration,
}

impl LoadingReport {
    /// The panel blocks, so the reader must be able to get past it: a project on
    /// a network share can keep scanning for minutes, and an editor that cannot
    /// be reached until it stops is worse than one that looks unfinished.
    pub(crate) fn wave_through(&mut self) {
        self.waved_through = true;
    }

    /// Forgets a finished load while keeping what outlives it: the handle, so
    /// focus can still be handed back from a frame that comes after this, and
    /// nothing else. A second load must not inherit the first one's waiting, and
    /// above all must not inherit having been waved through.
    fn forget(&mut self) {
        *self = Self {
            focus_handle: self.focus_handle.take(),
            ..Default::default()
        };
    }

    /// Begins a load, from a state that may be a finished one.
    pub(crate) fn start_over(&mut self) {
        self.speaking = false;
        self.waved_through = false;
        self.waited = Duration::ZERO;
    }
}

impl Workspace {
    /// Starts watching, unless something is already being watched. Called for
    /// every unit of start-up work, of which there are many.
    pub(crate) fn watch_the_loading(&mut self, cx: &mut Context<Self>) {
        if self.loading.watching {
            return;
        }
        self.loading.watching = true;
        self.loading.waited = Duration::ZERO;
        // Detached rather than held in a field: the loop below is what clears
        // `watching`, and a task that dropped itself to do so could never.
        cx.spawn(async move |workspace, cx| {
            loop {
                cx.background_executor().timer(WHILE_WAITING).await;
                // Noticing that the work is done and standing down happen in the
                // same update as each other, so a load that begins in between
                // cannot find `watching` still set with nothing watching: either
                // it is seen here and watching continues, or it arms its own.
                let watching = workspace.update(cx, |workspace, cx| {
                    if workspace.active_load_phase().is_none() {
                        workspace.loading.forget();
                        cx.notify();
                        return false;
                    }
                    let loading = &mut workspace.loading;
                    loading.waited += WHILE_WAITING;
                    if !loading.speaking && loading.waited >= BEFORE_SAYING_ANYTHING {
                        loading.speaking = true;
                    }
                    cx.notify();
                    true
                });
                // A closed window has no loading left to report either.
                if !matches!(watching, Ok(true)) {
                    break;
                }
            }
        })
        .detach();
    }

    /// Whether the panel is up. Asked again inside every deferred callback: a
    /// modal can open, the work can finish and the reader can wave the panel
    /// through, all between a frame and the end of its effect cycle.
    fn is_reporting(&self) -> bool {
        self.active_load_phase().is_some() && self.loading.speaking && !self.loading.waved_through
    }

    /// What fraction of the start-up work is done. Every phase counts the same,
    /// however many units of work it turned out to hold, so the bar does not
    /// spend its whole travel on whichever phase happens to be counted in the
    /// most pieces.
    pub(crate) fn how_far_loaded(&self) -> f32 {
        let mut sum = 0.0;
        for phase in WorkspaceLoadPhase::ALL {
            sum += self.how_far_through(phase);
        }
        sum / WorkspaceLoadPhase::COUNT as f32
    }

    /// What fraction of one phase is done. Work that never began is not work
    /// left to do, so a phase with nothing to restore reads as finished.
    fn how_far_through(&self, phase: WorkspaceLoadPhase) -> f32 {
        let left = self.load_phase_counts[phase as usize];
        if left == 0 {
            return 1.0;
        }
        let total = self.load_phase_totals[phase as usize].max(left);
        (total - left) as f32 / total as f32
    }

    /// How many files the project has turned up so far. There is no total to
    /// compare it against -- the scanner finds out as it walks -- but a number
    /// that keeps climbing is the difference between waiting and wondering.
    fn files_found(&self, cx: &App) -> usize {
        self.project
            .read(cx)
            .visible_worktrees(cx)
            .map(|worktree| worktree.read(cx).file_count())
            .sum()
    }

    pub(crate) fn render_loading_report(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.is_reporting() {
            // The panel took focus and is no longer drawn, so give it back rather
            // than leave the window focused on an element nobody can see and a
            // keyboard that does nothing until the reader clicks somewhere.
            //
            // The handle is kept rather than taken: taking it on the first frame
            // that stops showing would throw away the only way back if focus
            // arrives a moment later, which it can -- taking focus is itself
            // deferred.
            if self
                .loading
                .focus_handle
                .as_ref()
                .is_some_and(|focus_handle| focus_handle.is_focused(window))
            {
                cx.defer_in(window, |workspace, window, cx| {
                    let ours_still_has_it = workspace
                        .loading
                        .focus_handle
                        .as_ref()
                        .is_some_and(|focus_handle| focus_handle.is_focused(window));
                    if workspace.is_reporting() || !ours_still_has_it {
                        return;
                    }
                    let pane = workspace.active_pane.focus_handle(cx);
                    window.focus(&pane, cx);
                });
            }
            return None;
        }

        // A modal over a loading window is something only the reader can answer
        // -- trusting the project, above all, which the load itself waits on.
        // Taking focus from it would leave both of them stuck.
        let a_modal_is_asking = self.has_active_modal(window, cx);
        let focus_handle = self
            .loading
            .focus_handle
            .get_or_insert_with(|| cx.focus_handle())
            .clone();
        if !a_modal_is_asking && !focus_handle.is_focused(window) {
            cx.defer_in(window, {
                let focus_handle = focus_handle.clone();
                move |workspace, window, cx| {
                    // By the time this runs the work can be over, the reader can
                    // have waved the panel through, or a modal can have opened.
                    // Focusing an undrawn panel then would leave the keyboard
                    // dead, and taking focus from a modal would leave the load
                    // waiting on an answer nobody can give.
                    if !workspace.is_reporting() || workspace.has_active_modal(window, cx) {
                        return;
                    }
                    window.focus(&focus_handle, cx);
                }
            });
        }

        let colors = cx.theme().colors();
        let done = self.how_far_loaded();
        let files_found = self.files_found(cx);
        let waited = self.loading.waited.as_secs();

        let phases = WorkspaceLoadPhase::ALL.map(|phase| {
            let left = self.load_phase_counts[phase as usize];
            let total = self.load_phase_totals[phase as usize].max(left);
            let counted = (left > 0 && total > 1).then(|| format!("{} of {}", total - left, total));
            (phase, left == 0, counted)
        });

        let card =
            v_flex()
                .w(px(420.))
                .p_5()
                .gap_3()
                .rounded_lg()
                .bg(colors.elevated_surface_background)
                .border_1()
                .border_color(colors.border)
                .shadow_lg()
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Icon::new(IconName::ArrowCircle)
                                .size(IconSize::Small)
                                .color(Color::Accent)
                                .with_rotate_animation(2),
                        )
                        .child(Label::new("Opening the project").size(LabelSize::Large)),
                )
                .child(
                    ProgressBar::new("loading-report", done, 1.0, cx)
                        .fg_color(colors.text_accent)
                        .bg_color(colors.element_background),
                )
                .child(
                    h_flex()
                        .justify_between()
                        .child(
                            Label::new(format!("{}%", (done * 100.0).round() as u32))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .child(
                            Label::new(format!("{waited}s"))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                )
                .children(phases.into_iter().map(|(phase, finished, counted)| {
                    h_flex()
                        .gap_2()
                        .child(if finished {
                            Icon::new(IconName::Check)
                                .size(IconSize::XSmall)
                                .color(Color::Success)
                                .into_any_element()
                        } else {
                            Icon::new(IconName::ArrowCircle)
                                .size(IconSize::XSmall)
                                .color(Color::Muted)
                                .with_rotate_animation(2)
                                .into_any_element()
                        })
                        .child(Label::new(phase.label()).size(LabelSize::Small).color(
                            if finished {
                                Color::Muted
                            } else {
                                Color::Default
                            },
                        ))
                        .when_some(counted, |row, counted| {
                            row.child(
                                Label::new(counted)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                        })
                }))
                .when(files_found > 0, |card| {
                    card.child(
                        Label::new(format!("{files_found} files found so far"))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                })
                .child(
                    Label::new("Press escape to use the editor while this finishes")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                );

        Some(
            div()
                .absolute()
                .inset_0()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                // Enough of the window shows through to watch it fill in, which
                // is the point: the reader wanted to know it was working.
                .bg(colors.background.opacity(0.8))
                .when(!a_modal_is_asking, |overlay| overlay.occlude())
                .track_focus(&focus_handle)
                .key_context("LoadingReport")
                .debug_selector(|| "loading-report".into())
                .child(card)
                .into_any_element(),
        )
    }
}
