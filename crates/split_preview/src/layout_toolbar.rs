use editor::Editor;
use gpui::{Entity, EventEmitter, WeakEntity};
use ui::{Tooltip, prelude::*};
use workspace::item::ItemHandle;
use workspace::{ToolbarItemEvent, ToolbarItemLocation, ToolbarItemView, Workspace};

use crate::open_split_preview;
use crate::split_preview_view::{PreviewLayout, SplitPreviewView};

/// The layout switch, shown above the document it applies to. A document this
/// editor knows how to preview -- Markdown, HTML, an OpenAPI contract -- offers
/// the choice from its own tab, so a reader does not have to know that a command
/// exists in order to discover that a preview is available at all.
pub struct PreviewLayoutToolbar {
    workspace: WeakEntity<Workspace>,
    active: Option<ActiveDocument>,
}

#[derive(Clone)]
enum ActiveDocument {
    /// A plain editor whose contents can be previewed.
    Previewable(Entity<Editor>),
    /// A document already opened next to its preview.
    Split(Entity<SplitPreviewView>),
}

impl PreviewLayoutToolbar {
    pub fn new(workspace: WeakEntity<Workspace>) -> Self {
        Self {
            workspace,
            active: None,
        }
    }

    fn selected_layout(&self, cx: &App) -> PreviewLayout {
        match &self.active {
            Some(ActiveDocument::Split(view)) => view.read(cx).layout(),
            _ => PreviewLayout::Editor,
        }
    }

    fn choose(&mut self, layout: PreviewLayout, window: &mut Window, cx: &mut Context<Self>) {
        match self.active.clone() {
            Some(ActiveDocument::Split(view)) => {
                view.update(cx, |view, cx| view.set_layout(layout, window, cx));
            }
            Some(ActiveDocument::Previewable(editor)) => {
                // The editor-only layout is what a plain editor tab already is.
                if layout == PreviewLayout::Editor {
                    return;
                }
                self.workspace
                    .update(cx, |workspace, cx| {
                        open_split_preview::open_for_editor(workspace, &editor, layout, window, cx);
                    })
                    .ok();
            }
            None => {}
        }
    }
}

impl EventEmitter<ToolbarItemEvent> for PreviewLayoutToolbar {}

impl ToolbarItemView for PreviewLayoutToolbar {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        cx.notify();
        self.active = None;

        let Some(item) = active_pane_item else {
            return ToolbarItemLocation::Hidden;
        };

        if let Some(view) = item.downcast::<SplitPreviewView>() {
            self.active = Some(ActiveDocument::Split(view));
            return ToolbarItemLocation::PrimaryRight;
        }

        // Offered only when there is something to preview: a plain YAML file is
        // not an OpenAPI contract, and the switch must not claim otherwise.
        if let Some(editor) = item.act_as::<Editor>(cx)
            && open_split_preview::preview_kind_for(&editor, cx).is_some()
        {
            self.active = Some(ActiveDocument::Previewable(editor));
            return ToolbarItemLocation::PrimaryRight;
        }

        ToolbarItemLocation::Hidden
    }
}

impl Render for PreviewLayoutToolbar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.active.is_none() {
            return div().into_any_element();
        }
        let selected = self.selected_layout(cx);

        h_flex()
            .gap_px()
            .children(PreviewLayout::ALL.map(|layout| {
                IconButton::new(("preview-layout", layout.to_db() as usize), layout.icon())
                    .icon_size(IconSize::Small)
                    .toggle_state(layout == selected)
                    .tooltip(Tooltip::text(layout.label()))
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.choose(layout, window, cx)),
                    )
            }))
            .into_any_element()
    }
}
