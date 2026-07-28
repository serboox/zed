use std::any::TypeId;
use std::sync::Arc;

use anyhow::Result;
use editor::{Editor, EditorEvent};
use gpui::{
    AnyEntity, AnyView, App, Entity, EventEmitter, FocusHandle, Focusable, Hsla, IntoElement,
    ParentElement, Render, SharedString, Styled, Subscription, Task, Window, div,
};
use project::{Project, ProjectPath};
use ui::{Tooltip, prelude::*};
use workspace::Workspace;
use workspace::item::{Item, ItemEvent, SaveOptions, TabContentParams};
use workspace::searchable::SearchableItemHandle;

use crate::{CycleLayout, ShowEditorAndPreview, ShowEditorOnly, ShowPreviewOnly};

/// How visible the floating layout switch is when the pointer is elsewhere.
const RESTING_SWITCH_OPACITY: f32 = 0.35;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreviewLayout {
    Editor,
    EditorAndPreview,
    Preview,
}

impl PreviewLayout {
    pub const ALL: [PreviewLayout; 3] = [Self::Editor, Self::EditorAndPreview, Self::Preview];

    pub fn label(self) -> &'static str {
        match self {
            Self::Editor => "Editor",
            Self::EditorAndPreview => "Editor and Preview",
            Self::Preview => "Preview",
        }
    }

    fn icon(self) -> IconName {
        match self {
            Self::Editor => IconName::Pencil,
            Self::EditorAndPreview => IconName::SplitAlt,
            Self::Preview => IconName::Eye,
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Editor => Self::EditorAndPreview,
            Self::EditorAndPreview => Self::Preview,
            Self::Preview => Self::Editor,
        }
    }

    pub fn to_db(self) -> i64 {
        match self {
            Self::Editor => 0,
            Self::EditorAndPreview => 1,
            Self::Preview => 2,
        }
    }

    pub fn from_db(value: i64) -> Self {
        match value {
            0 => Self::Editor,
            2 => Self::Preview,
            _ => Self::EditorAndPreview,
        }
    }
}

/// One tab holding a document's source editor next to its rendered preview,
/// with a three-way layout switch. The preview is supplied by the caller as a
/// plain view, so every document kind (Markdown, HTML, an OpenAPI contract)
/// reuses this host instead of inventing its own split.
pub struct SplitPreviewView {
    editor: Entity<Editor>,
    /// Must track its own focus handle in `render`. Focus moves onto it in the
    /// preview-only layout, and a handle that belongs to no painted element
    /// takes the whole tab out of the focus path, which silently kills every
    /// keyboard action here, layout switching included.
    preview: AnyView,
    preview_focus_handle: FocusHandle,
    focus_handle: FocusHandle,
    layout: PreviewLayout,
    _editor_subscription: Subscription,
}

impl SplitPreviewView {
    pub fn new<P: Render + Focusable>(
        editor: Entity<Editor>,
        preview: Entity<P>,
        layout: PreviewLayout,
        cx: &mut Context<Self>,
    ) -> Self {
        let preview_focus_handle = preview.read(cx).focus_handle(cx);
        // The tab has to follow the source document's title and dirty marker,
        // which only the editor knows about.
        let subscription = cx.subscribe(&editor, |_, _, event: &EditorEvent, cx| {
            cx.emit(event.clone())
        });
        Self {
            editor,
            preview: preview.into(),
            preview_focus_handle,
            focus_handle: cx.focus_handle(),
            layout,
            _editor_subscription: subscription,
        }
    }

    pub fn editor(&self) -> &Entity<Editor> {
        &self.editor
    }

    pub fn layout(&self) -> PreviewLayout {
        self.layout
    }

    pub fn set_layout(
        &mut self,
        layout: PreviewLayout,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.layout == layout {
            return;
        }
        self.layout = layout;
        // Focus has to land somewhere that is actually on screen, or typing
        // would go to a hidden editor.
        match layout {
            PreviewLayout::Preview => self.preview_focus_handle.focus(window, cx),
            PreviewLayout::Editor | PreviewLayout::EditorAndPreview => {
                self.editor.focus_handle(cx).focus(window, cx)
            }
        }
        cx.emit(EditorEvent::TitleChanged);
        cx.notify();
    }

    fn render_layout_switch(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .id("split-preview-layout-switch")
            .debug_selector(|| "split-preview-layout-switch".into())
            .absolute()
            .top_1()
            .right_3()
            .p_0p5()
            .gap_px()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().elevated_surface_background)
            .shadow_sm()
            // The panel floats over the document, so it stays faint until the
            // pointer is on it -- present enough to be found, quiet enough not
            // to sit on top of the text being read.
            .opacity(RESTING_SWITCH_OPACITY)
            .hover(|style| style.opacity(1.0))
            .children(PreviewLayout::ALL.map(|layout| {
                let selected = self.layout == layout;
                IconButton::new(
                    ("split-preview-layout", layout.to_db() as usize),
                    layout.icon(),
                )
                .icon_size(IconSize::Small)
                .toggle_state(selected)
                .tooltip(Tooltip::text(layout.label()))
                .on_click(
                    cx.listener(move |this, _, window, cx| this.set_layout(layout, window, cx)),
                )
            }))
    }

    fn render_body(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let editor = self.editor.clone().into_any_element();
        let preview = self.preview.clone().into_any_element();

        // Each half needs its own size directive: a flex child with no width of
        // its own collapses to nothing and the pane renders empty.
        match self.layout {
            PreviewLayout::Editor => div()
                .debug_selector(|| "split-preview-editor".into())
                .size_full()
                .child(editor)
                .into_any_element(),
            PreviewLayout::Preview => div()
                .debug_selector(|| "split-preview-preview".into())
                .size_full()
                .child(preview)
                .into_any_element(),
            PreviewLayout::EditorAndPreview => h_flex()
                .size_full()
                .items_stretch()
                .child(
                    div()
                        .debug_selector(|| "split-preview-editor".into())
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .child(editor),
                )
                .child(
                    div()
                        .w_px()
                        .h_full()
                        .flex_none()
                        .bg(cx.theme().colors().border),
                )
                .child(
                    div()
                        .debug_selector(|| "split-preview-preview".into())
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .child(preview),
                )
                .into_any_element(),
        }
    }
}

impl Focusable for SplitPreviewView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match self.layout {
            PreviewLayout::Preview => self.preview_focus_handle.clone(),
            PreviewLayout::Editor | PreviewLayout::EditorAndPreview => self.editor.focus_handle(cx),
        }
    }
}

impl EventEmitter<EditorEvent> for SplitPreviewView {}

impl Render for SplitPreviewView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("SplitPreview")
            .relative()
            .size_full()
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &ShowEditorOnly, window, cx| {
                this.set_layout(PreviewLayout::Editor, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ShowEditorAndPreview, window, cx| {
                this.set_layout(PreviewLayout::EditorAndPreview, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ShowPreviewOnly, window, cx| {
                this.set_layout(PreviewLayout::Preview, window, cx)
            }))
            .on_action(cx.listener(|this, _: &CycleLayout, window, cx| {
                let next = this.layout.next();
                this.set_layout(next, window, cx)
            }))
            .child(self.render_body(cx))
            .child(self.render_layout_switch(cx))
    }
}

impl Item for SplitPreviewView {
    type Event = EditorEvent;

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        match event {
            EditorEvent::Saved
            | EditorEvent::DirtyChanged
            | EditorEvent::TitleChanged
            | EditorEvent::FileHandleChanged => f(ItemEvent::UpdateTab),
            EditorEvent::Edited { .. } => {
                f(ItemEvent::Edit);
                f(ItemEvent::UpdateTab);
            }
            EditorEvent::BreadcrumbsChanged => f(ItemEvent::UpdateBreadcrumbs),
            _ => {}
        }
    }

    fn tab_content(&self, params: TabContentParams, window: &Window, cx: &App) -> gpui::AnyElement {
        Item::tab_content(self.editor.read(cx), params, window, cx)
    }

    fn tab_content_text(&self, detail: usize, cx: &App) -> SharedString {
        Item::tab_content_text(self.editor.read(cx), detail, cx)
    }

    fn tab_icon(&self, window: &Window, cx: &App) -> Option<Icon> {
        Item::tab_icon(self.editor.read(cx), window, cx)
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        Item::tab_tooltip_text(self.editor.read(cx), cx)
    }

    fn tab_background_color(&self, cx: &App) -> Option<Hsla> {
        Item::tab_background_color(self.editor.read(cx), cx)
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Split Preview Opened")
    }

    fn can_split(&self) -> bool {
        false
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        Item::for_each_project_item(self.editor.read(cx), cx, f)
    }

    fn buffer_kind(&self, cx: &App) -> workspace::item::ItemBufferKind {
        Item::buffer_kind(self.editor.read(cx), cx)
    }

    fn active_project_path(&self, cx: &App) -> Option<ProjectPath> {
        Item::active_project_path(self.editor.read(cx), cx)
    }

    fn is_dirty(&self, cx: &App) -> bool {
        Item::is_dirty(self.editor.read(cx), cx)
    }

    fn has_conflict(&self, cx: &App) -> bool {
        Item::has_conflict(self.editor.read(cx), cx)
    }

    fn has_deleted_file(&self, cx: &App) -> bool {
        Item::has_deleted_file(self.editor.read(cx), cx)
    }

    fn capability(&self, cx: &App) -> language::Capability {
        Item::capability(self.editor.read(cx), cx)
    }

    fn can_save(&self, cx: &App) -> bool {
        Item::can_save(self.editor.read(cx), cx)
    }

    fn can_save_as(&self, cx: &App) -> bool {
        Item::can_save_as(self.editor.read(cx), cx)
    }

    fn save(
        &mut self,
        options: SaveOptions,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.editor.update(cx, |editor, cx| {
            Item::save(editor, options, project, window, cx)
        })
    }

    fn save_as(
        &mut self,
        project: Entity<Project>,
        path: ProjectPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.editor.update(cx, |editor, cx| {
            Item::save_as(editor, project, path, window, cx)
        })
    }

    fn reload(
        &mut self,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.editor
            .update(cx, |editor, cx| Item::reload(editor, project, window, cx))
    }

    fn navigate(
        &mut self,
        data: Arc<dyn std::any::Any + Send>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.editor
            .update(cx, |editor, cx| Item::navigate(editor, data, window, cx))
    }

    fn set_nav_history(
        &mut self,
        history: workspace::ItemNavHistory,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            Item::set_nav_history(editor, history, window, cx)
        })
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            Item::added_to_workspace(editor, workspace, window, cx)
        })
    }

    fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor
            .update(cx, |editor, cx| Item::deactivated(editor, window, cx))
    }

    fn breadcrumb_location(&self, cx: &App) -> workspace::ToolbarItemLocation {
        Item::breadcrumb_location(self.editor.read(cx), cx)
    }

    fn breadcrumbs(
        &self,
        cx: &App,
    ) -> Option<(Vec<language::HighlightedText>, Option<gpui::Font>)> {
        Item::breadcrumbs(self.editor.read(cx), cx)
    }

    fn as_searchable(&self, _: &Entity<Self>, cx: &App) -> Option<Box<dyn SearchableItemHandle>> {
        Item::as_searchable(self.editor.read(cx), &self.editor, cx)
    }

    /// Reported as the inner editor as well, so everything that looks for "the
    /// editor in this tab" -- saving, following, the outline, go-to-definition
    /// -- keeps working while the preview is on screen.
    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        _: &'a App,
    ) -> Option<AnyEntity> {
        if TypeId::of::<Self>() == type_id {
            Some(self_handle.clone().into())
        } else if TypeId::of::<Editor>() == type_id {
            Some(self.editor.clone().into())
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};
    use language::Buffer;

    struct StubPreview {
        focus_handle: FocusHandle,
    }

    // Mirrors what every real preview does; see the note on the `preview` field.

    impl Focusable for StubPreview {
        fn focus_handle(&self, _: &App) -> FocusHandle {
            self.focus_handle.clone()
        }
    }

    impl Render for StubPreview {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().track_focus(&self.focus_handle)
        }
    }

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            workspace::AppState::test(cx);
            editor::init(cx);
        });
    }

    /// Paints a frame without running the executor, so an assertion sees the
    /// layout that is actually on screen rather than internal state.
    fn draw(cx: &mut VisualTestContext) {
        cx.update(|window, cx| {
            window.refresh();
            window.draw(cx).clear();
        });
    }

    #[gpui::test]
    async fn each_layout_paints_only_the_panes_it_promises(cx: &mut TestAppContext) {
        init_test(cx);

        let (view, cx) = cx.add_window_view(|window, cx| {
            let buffer = cx.new(|cx| Buffer::local("openapi: 3.0.3\n", cx));
            let editor = cx.new(|cx| Editor::for_buffer(buffer, None, window, cx));
            let preview = cx.new(|cx| StubPreview {
                focus_handle: cx.focus_handle(),
            });
            SplitPreviewView::new(editor, preview, PreviewLayout::EditorAndPreview, cx)
        });
        draw(cx);

        let side_by_side_editor = cx
            .debug_bounds("split-preview-editor")
            .expect("editor pane");
        let side_by_side_preview = cx
            .debug_bounds("split-preview-preview")
            .expect("preview pane");
        assert!(
            side_by_side_editor.size.width > gpui::px(0.),
            "the editor half must occupy real screen area"
        );
        assert!(
            side_by_side_preview.size.width > gpui::px(0.),
            "the preview half must occupy real screen area"
        );
        assert!(
            side_by_side_editor.origin.x < side_by_side_preview.origin.x,
            "the editor belongs on the left of the preview"
        );
        assert!(
            cx.debug_bounds("split-preview-layout-switch").is_some(),
            "the layout switch has to be reachable in every layout"
        );

        view.update_in(cx, |view, window, cx| {
            view.set_layout(PreviewLayout::Editor, window, cx)
        });
        draw(cx);
        let editor_only = cx
            .debug_bounds("split-preview-editor")
            .expect("editor pane");
        assert!(
            cx.debug_bounds("split-preview-preview").is_none(),
            "the preview must not be painted in the editor-only layout"
        );
        assert!(
            editor_only.size.width > side_by_side_editor.size.width,
            "the editor takes the whole width once the preview is hidden"
        );

        view.update_in(cx, |view, window, cx| {
            view.set_layout(PreviewLayout::Preview, window, cx)
        });
        draw(cx);
        assert!(
            cx.debug_bounds("split-preview-editor").is_none(),
            "the editor must not be painted in the preview-only layout"
        );
        let preview_only = cx
            .debug_bounds("split-preview-preview")
            .expect("preview pane");
        assert!(preview_only.size.width > side_by_side_preview.size.width);
        assert!(cx.debug_bounds("split-preview-layout-switch").is_some());
    }

    #[gpui::test]
    async fn focus_follows_the_visible_pane(cx: &mut TestAppContext) {
        init_test(cx);

        let (view, cx) = cx.add_window_view(|window, cx| {
            let buffer = cx.new(|cx| Buffer::local("openapi: 3.0.3\n", cx));
            let editor = cx.new(|cx| Editor::for_buffer(buffer, None, window, cx));
            let preview = cx.new(|cx| StubPreview {
                focus_handle: cx.focus_handle(),
            });
            SplitPreviewView::new(editor, preview, PreviewLayout::EditorAndPreview, cx)
        });

        // With the editor hidden, typing must not be routed to it.
        view.update_in(cx, |view, window, cx| {
            view.set_layout(PreviewLayout::Preview, window, cx);
            assert!(
                !view.editor.focus_handle(cx).is_focused(window),
                "a hidden editor must not keep focus"
            );
            assert!(view.preview_focus_handle.is_focused(window));
        });

        view.update_in(cx, |view, window, cx| {
            view.set_layout(PreviewLayout::Editor, window, cx);
            assert!(view.editor.focus_handle(cx).is_focused(window));
        });
    }

    #[gpui::test]
    async fn the_cycle_action_walks_every_layout_and_wraps(cx: &mut TestAppContext) {
        init_test(cx);

        let (view, cx) = cx.add_window_view(|window, cx| {
            let buffer = cx.new(|cx| Buffer::local("openapi: 3.0.3\n", cx));
            let editor = cx.new(|cx| Editor::for_buffer(buffer, None, window, cx));
            let preview = cx.new(|cx| StubPreview {
                focus_handle: cx.focus_handle(),
            });
            SplitPreviewView::new(editor, preview, PreviewLayout::Editor, cx)
        });
        // An action travels the focus path, so the tab has to hold focus for the
        // dispatch below to reach its handler at all.
        view.update_in(cx, |view, window, cx| {
            view.focus_handle.focus(window, cx);
        });
        draw(cx);

        // Dispatched as a real action so the handler and the `SplitPreview` key
        // context are covered, not only the enum's own stepping.
        let mut seen = vec![view.read_with(cx, |view, _| view.layout())];
        for _ in 0..PreviewLayout::ALL.len() {
            cx.dispatch_action(CycleLayout);
            draw(cx);
            seen.push(view.read_with(cx, |view, _| view.layout()));
        }

        assert_eq!(
            seen,
            vec![
                PreviewLayout::Editor,
                PreviewLayout::EditorAndPreview,
                PreviewLayout::Preview,
                PreviewLayout::Editor,
            ],
            "cycling must visit every layout in order and wrap around"
        );
    }
}
