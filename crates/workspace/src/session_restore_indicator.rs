use gpui::{
    App, Context, Empty, Entity, IntoElement, ParentElement, Render, Subscription, WeakEntity,
    Window,
};
use ui::{CommonAnimationExt, prelude::*};

use crate::{HideStatusItem, StatusItemView, Workspace, item::ItemHandle};

/// Status-bar indicator shown while a saved session is being restored. It keeps
/// a restoring window from being mistaken for an empty first-run window: the
/// editors and panels appear only once deserialization finishes, so without a
/// signal the window looks blank and "ready" when it is not.
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
        let restoring = self
            .workspace
            .read_with(cx, |workspace, _| workspace.is_restoring_session())
            .unwrap_or(false);

        if !restoring {
            return Empty.into_any_element();
        }

        h_flex()
            .gap_1()
            .child(
                Icon::new(IconName::ArrowCircle)
                    .size(IconSize::XSmall)
                    .color(Color::Muted)
                    .with_rotate_animation(2),
            )
            .child(
                Label::new("Restoring session…")
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
