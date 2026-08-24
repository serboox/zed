use std::any::TypeId;
use std::sync::Arc;

use anyhow::Result;
use editor::{Editor, EditorEvent};
use gpui::{
    AnyEntity, AnyView, App, Entity, EventEmitter, FocusHandle, Focusable, Hsla, IntoElement,
    ParentElement, Render, SharedString, Styled, Subscription, Task, Window, div,
};
use project::{Project, ProjectPath};
use ui::prelude::*;
use workspace::Workspace;
use workspace::item::{Item, ItemEvent, SaveOptions, TabContentParams};
use workspace::searchable::SearchableItemHandle;

use crate::{CycleLayout, ShowEditorAndPreview, ShowEditorOnly, ShowPreviewOnly};

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

    pub fn icon(self) -> IconName {
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

    /// A tab whose preview cannot be built yet, showing its editor until it is.
    /// An SVG page can only be built while the workspace is free to be borrowed,
    /// and it is not while a tab is being built for a file that is opening, so
    /// the page arrives a beat later through [`Self::install_preview`]. Until it
    /// does -- and if it never does -- the tab is the document's editor, which is
    /// what a reader would have got anyway.
    pub fn awaiting_preview(editor: Entity<Editor>, cx: &mut Context<Self>) -> Self {
        let pending = cx.new(|cx| PendingPreview {
            focus_handle: cx.focus_handle(),
        });
        Self::new(editor, pending, PreviewLayout::Editor, cx)
    }

    pub fn install_preview<P: Render + Focusable>(
        &mut self,
        preview: Entity<P>,
        layout: PreviewLayout,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A file can be opened without being the one the reader is looking at,
        // and such a tab must not pull focus over to itself when its preview
        // turns up.
        let is_being_read = self.focus_handle(cx).contains_focused(window, cx);
        self.preview_focus_handle = preview.read(cx).focus_handle(cx);
        self.preview = preview.into();
        self.layout = layout;
        if is_being_read {
            self.focus_the_visible_pane(window, cx);
        }
        cx.emit(EditorEvent::TitleChanged);
        cx.notify();
    }

    pub fn editor(&self) -> &Entity<Editor> {
        &self.editor
    }

    pub fn preview(&self) -> &AnyView {
        &self.preview
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
        self.apply_layout(layout, window, cx);
    }

    fn apply_layout(&mut self, layout: PreviewLayout, window: &mut Window, cx: &mut Context<Self>) {
        self.layout = layout;
        self.focus_the_visible_pane(window, cx);
        cx.emit(EditorEvent::TitleChanged);
        cx.notify();
    }

    /// Focus has to land somewhere that is actually on screen, or typing would go
    /// to a hidden editor.
    fn focus_the_visible_pane(&self, window: &mut Window, cx: &mut Context<Self>) {
        match self.layout {
            PreviewLayout::Preview => self.preview_focus_handle.focus(window, cx),
            PreviewLayout::Editor | PreviewLayout::EditorAndPreview => {
                self.editor.focus_handle(cx).focus(window, cx)
            }
        }
    }

    /// Zoom belongs to whatever is being read. The editor's font size is an
    /// application-wide setting handled on the workspace, which is why zooming
    /// in a split tab used to scale the code beside the page instead of the page
    /// itself. Here the page is scaled while it is on screen, and the action is
    /// left alone when only the editor is.
    fn zoom_preview(&mut self, delta: Pixels, cx: &mut Context<Self>) -> bool {
        if matches!(self.layout, PreviewLayout::Editor) || !self.preview_reads_the_page_font() {
            return false;
        }
        theme_settings::adjust_markdown_preview_font_size(cx, |size| size + delta);
        true
    }

    /// Whether this preview is drawn at the page font size. A contract preview is
    /// not: taking its zoom over would move a setting it never reads and leave
    /// the keystroke doing nothing at all.
    fn preview_reads_the_page_font(&self) -> bool {
        self.preview
            .clone()
            .downcast::<markdown_preview::markdown_preview_view::MarkdownPreviewView>()
            .is_ok()
            || self
                .preview
                .clone()
                .downcast::<html_preview::html_preview_view::HtmlPreviewView>()
                .is_ok()
    }

    /// How far down the preview's own controls reach at the top left. Only a
    /// browser page keeps any there.
    fn preview_inset(&self, cx: &App) -> gpui::Pixels {
        match self
            .preview
            .clone()
            .downcast::<html_preview::html_preview_view::HtmlPreviewView>()
        {
            Ok(page) => Item::floating_controls_inset(page.read(cx), cx),
            Err(_) => gpui::Pixels::ZERO,
        }
    }

    /// An action handler here stops the action by default, so everything this
    /// tab does not mean to take over has to be handed back on with
    /// `cx.propagate()` -- otherwise zoom would go dead in the editor-only
    /// layout, and the palette's persisting commands with it.
    fn increase_preview_font_size(
        &mut self,
        action: &zed_actions::IncreaseBufferFontSize,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Only the keyboard's own zoom is taken over. The persisting variants come
        // from the palette and mean "change the setting", which stays global.
        if action.persist || !self.zoom_preview(px(1.), cx) {
            cx.propagate();
        }
    }

    fn decrease_preview_font_size(
        &mut self,
        action: &zed_actions::DecreaseBufferFontSize,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if action.persist || !self.zoom_preview(px(-1.), cx) {
            cx.propagate();
        }
    }

    fn reset_preview_font_size(
        &mut self,
        action: &zed_actions::ResetBufferFontSize,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if action.persist
            || matches!(self.layout, PreviewLayout::Editor)
            || !self.preview_reads_the_page_font()
        {
            cx.propagate();
            return;
        }
        theme_settings::reset_markdown_preview_font_size(cx);
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
                    v_flex()
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
                    // A column, so that a preview which sizes itself with `flex_1`
                    // grows down the half rather than across it. In a row it took
                    // its height from its content and the page ended up clipped to
                    // the height of its own padding.
                    v_flex()
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

/// Stands in for a preview that is still to be built. It is never on screen: the
/// tab shows its editor until the real preview replaces this one.
struct PendingPreview {
    focus_handle: FocusHandle,
}

impl Focusable for PendingPreview {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PendingPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().track_focus(&self.focus_handle)
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
            .on_action(cx.listener(Self::increase_preview_font_size))
            .on_action(cx.listener(Self::decrease_preview_font_size))
            .on_action(cx.listener(Self::reset_preview_font_size))
            .child(self.render_body(cx))
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

    fn preserve_preview(&self, cx: &App) -> bool {
        Item::preserve_preview(self.editor.read(cx), cx)
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

    /// The path above the document is the editor's, and it says nothing that the
    /// tab does not: with the editor hidden there is nothing for it to point at,
    /// and it costs a row of the window.
    fn breadcrumb_location(&self, cx: &App) -> workspace::ToolbarItemLocation {
        match self.layout {
            PreviewLayout::Preview => workspace::ToolbarItemLocation::Hidden,
            _ => Item::breadcrumb_location(self.editor.read(cx), cx),
        }
    }

    /// With only the preview showing, the corner belongs to the preview.
    fn floating_controls_inset(&self, cx: &App) -> gpui::Pixels {
        match self.layout {
            PreviewLayout::Preview => self.preview_inset(cx),
            _ => Item::floating_controls_inset(self.editor.read(cx), cx),
        }
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
        } else if TypeId::of::<html_preview::html_preview_view::HtmlPreviewView>() == type_id {
            // The developer's tools ask the workspace for the page being read,
            // and inside this tab the page is not the item -- this is. Without
            // this the tools find nothing and show three empty tabs.
            self.preview
                .clone()
                .downcast::<html_preview::html_preview_view::HtmlPreviewView>()
                .ok()
                .map(Into::into)
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
