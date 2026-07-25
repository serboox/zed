use gpui::{
    App, Context, Empty, Entity, IntoElement, ParentElement, Render, Subscription, WeakEntity,
    Window,
};
use ui::{CommonAnimationExt, prelude::*};

use crate::{HideStatusItem, StatusItemView, Workspace, item::ItemHandle};

/// Status-bar indicator naming the workspace start-up work still in flight (see
/// [`crate::WorkspaceLoadPhase`]). The window chrome is painted before any of
/// that work finishes, so without a signal a loading window looks blank and
/// "ready" when it is not.
pub struct SessionRestoreIndicator {
    workspace: WeakEntity<Workspace>,
    _observation: Subscription,
}

impl SessionRestoreIndicator {
    pub fn new(workspace: Entity<Workspace>, cx: &mut Context<Self>) -> Self {
        let observation = cx.observe(&workspace, |_, _, cx| cx.notify());
        Self {
            workspace: workspace.downgrade(),
            _observation: observation,
        }
    }
}

impl Render for SessionRestoreIndicator {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let active_phase = self
            .workspace
            .read_with(cx, |workspace, _| workspace.active_load_phase())
            .ok()
            .flatten();

        let Some(active_phase) = active_phase else {
            return Empty.into_any_element();
        };

        h_flex()
            .gap_1()
            .child(
                Icon::new(IconName::ArrowCircle)
                    .size(IconSize::XSmall)
                    .color(Color::Muted)
                    .with_rotate_animation(2),
            )
            .child(
                Label::new(active_phase.label())
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
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
