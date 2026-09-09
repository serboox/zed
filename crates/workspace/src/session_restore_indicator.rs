use gpui::{
    App, Context, Empty, Entity, IntoElement, ParentElement, Render, Subscription, WeakEntity,
    Window, anchored, deferred, px,
};
use ui::{CommonAnimationExt, Tooltip, prelude::*};

use crate::running_work::RunningWorkRegistry;
use crate::{HideStatusItem, StatusItemView, Workspace, WorkspaceLoadPhase, item::ItemHandle};

/// Status-bar button naming the work still in flight, and the panel it opens.
///
/// The window chrome is painted before any of the start-up work finishes, so
/// without a signal a loading window looks blank and "ready" when it is not.
/// The panel exists because several things run at once and the one number on
/// the button is their average: a reader who wants to know what is actually
/// holding them up, or to stop one of the things, has nowhere else to look.
pub struct SessionRestoreIndicator {
    workspace: WeakEntity<Workspace>,
    listing: bool,
    _observation: Subscription,
}

impl SessionRestoreIndicator {
    pub fn new(workspace: Entity<Workspace>, cx: &mut Context<Self>) -> Self {
        let observation = cx.observe(&workspace, |_, _, cx| cx.notify());
        Self {
            workspace: workspace.downgrade(),
            listing: false,
            _observation: observation,
        }
    }

    /// Every piece of work in flight: the editor's own start-up phases, then
    /// whatever else registered itself.
    ///
    /// The phases come first and carry no cross. Loading the editor's modules
    /// cannot be abandoned half way -- a window without its file tree is not a
    /// window anybody asked for -- so a cross beside them would be a promise
    /// nothing could keep.
    fn everything_running(&self, cx: &App) -> Vec<Running> {
        let mut running: Vec<Running> = Vec::new();

        if let Ok(phases) = self.workspace.read_with(cx, |workspace, _| {
            WorkspaceLoadPhase::ALL
                .into_iter()
                .filter(|phase| workspace.phase_is_running(*phase))
                .map(|phase| (phase.label(), workspace.how_far_through_phase(phase)))
                .collect::<Vec<_>>()
        }) {
            running.extend(phases.into_iter().map(|(label, how_far)| Running {
                id: None,
                name: SharedString::from(label),
                how_far: Some(how_far),
            }));
        }

        running.extend(
            RunningWorkRegistry::all(cx)
                .into_iter()
                .map(|(id, work)| Running {
                    id: Some(id),
                    name: work.name.clone(),
                    how_far: work.how_far,
                }),
        );
        running
    }

    fn render_panel(&self, running: &[Running], cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        v_flex()
            .w(px(320.))
            .p_2()
            .gap_1()
            .rounded_md()
            .bg(colors.elevated_surface_background)
            .border_1()
            .border_color(colors.border)
            .shadow_lg()
            .children(running.iter().map(|one| {
                h_flex()
                    .gap_2()
                    .justify_between()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(Label::new(one.name.clone()).size(LabelSize::Small))
                            .when_some(one.how_far, |el, how_far| {
                                el.child(
                                    Label::new(format!("{}%", (how_far * 100.0).round() as u32))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                )
                            }),
                    )
                    // A cross only where stopping is something the work can
                    // actually do. Everything else shows none, rather than one
                    // that does nothing.
                    .when_some(one.id, |el, id| {
                        el.child(
                            IconButton::new(("stop-running-work", id), IconName::Close)
                                .icon_size(IconSize::XSmall)
                                .tooltip(Tooltip::text("Cancel"))
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    RunningWorkRegistry::stop(cx, id);
                                    cx.notify();
                                })),
                        )
                    })
            }))
            .into_any_element()
    }
}

struct Running {
    /// The registry's id, and so also whether it can be stopped: the editor's
    /// own start-up phases have none.
    id: Option<usize>,
    name: SharedString,
    how_far: Option<f32>,
}

impl Render for SessionRestoreIndicator {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let running = self.everything_running(cx);
        if running.is_empty() {
            // Nothing running is nothing to say, and a button that is always
            // there but usually blank is a hole in the bar.
            return Empty.into_any_element();
        }

        let done: f32 = {
            let known: Vec<f32> = running.iter().filter_map(|one| one.how_far).collect();
            if known.is_empty() {
                0.0
            } else {
                known.iter().sum::<f32>() / known.len() as f32
            }
        };
        let leading = running
            .first()
            .map(|one| one.name.clone())
            .unwrap_or_else(|| SharedString::from("Working"));
        let others = running.len().saturating_sub(1);

        let button = ui::ButtonLike::new("running-work")
            .child(
                Icon::new(IconName::ArrowCircle)
                    .size(IconSize::XSmall)
                    .color(Color::Muted)
                    .with_rotate_animation(2),
            )
            .child(
                Label::new(leading)
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .single_line(),
            )
            .child(
                // A spinner says only that something is happening. A number
                // says how much of it is left, which is what the reader wanted
                // to know -- and it is the same number the panel over the
                // window shows, so the two never disagree.
                Label::new(format!("{}%", (done * 100.0).round() as u32))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .when(others > 0, |el| {
                // Said only when there is more than one thing, because that is
                // the only time it tells the reader anything -- and it is what
                // makes clicking worth doing.
                el.child(
                    Label::new(format!("+{others}"))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            })
            .tooltip(Tooltip::text("Show what is running"))
            .on_click(cx.listener(|this, _, _, cx| {
                this.listing = !this.listing;
                cx.notify();
            }));

        div()
            .id("running-work-indicator")
            .debug_selector(|| "running-work-indicator".to_string())
            .child(button)
            .when(self.listing, |el| {
                // Deferred and anchored so the panel paints above everything
                // that comes after it in the bar, instead of being clipped by
                // the row it hangs from.
                el.child(deferred(
                    anchored()
                        .anchor(gpui::Anchor::BottomRight)
                        .child(self.render_panel(&running, cx)),
                ))
            })
            .into_any_element()
    }
}

impl StatusItemView for SessionRestoreIndicator {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        None
    }
}
