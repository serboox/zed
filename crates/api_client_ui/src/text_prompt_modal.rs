use editor::Editor;
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render, Window,
};
use std::sync::Arc;
use ui::{ElevationIndex, Tooltip, cyberpunk, prelude::*};
use workspace::ModalView;

/// A single-field text-input modal shared by every "give this a name" flow in
/// the API Client panel (New Collection / New Folder / New Request / Rename).
/// One generic modal instead of four near-identical ones, mirroring how
/// `db_client_ui`'s `RenameTableView` is shaped but without anything specific
/// to a single call site baked in.
pub struct TextPromptModal {
    focus_handle: FocusHandle,
    title: SharedString,
    confirm_label: SharedString,
    pub(crate) editor: Entity<Editor>,
    /// A multi-line editor has no height of its own, so the box around it has
    /// to be given one or the reader gets a sliver to paste a whole document
    /// into.
    multiline: bool,
    on_confirm: Arc<dyn Fn(String, &mut Window, &mut App)>,
}

/// Room for a pasted command or document without the dialog outgrowing the
/// window; the editor scrolls inside it.
const MULTILINE_HEIGHT: f32 = 200.;

impl TextPromptModal {
    pub fn new(
        title: impl Into<SharedString>,
        confirm_label: impl Into<SharedString>,
        placeholder: &'static str,
        initial_value: &str,
        on_confirm: Arc<dyn Fn(String, &mut Window, &mut App)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_impl(
            title,
            confirm_label,
            placeholder,
            initial_value,
            false,
            on_confirm,
            window,
            cx,
        )
    }

    /// Same modal, but with a multi-line editor -- for pasting a whole
    /// `curl` command or a Postman collection JSON document rather than
    /// typing a short name.
    pub fn new_multiline(
        title: impl Into<SharedString>,
        confirm_label: impl Into<SharedString>,
        placeholder: &'static str,
        on_confirm: Arc<dyn Fn(String, &mut Window, &mut App)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_impl(
            title,
            confirm_label,
            placeholder,
            "",
            true,
            on_confirm,
            window,
            cx,
        )
    }

    fn new_impl(
        title: impl Into<SharedString>,
        confirm_label: impl Into<SharedString>,
        placeholder: &'static str,
        initial_value: &str,
        multiline: bool,
        on_confirm: Arc<dyn Fn(String, &mut Window, &mut App)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let initial_value = initial_value.to_string();
        let editor = cx.new(|cx| {
            let mut editor = if multiline {
                Editor::multi_line(window, cx)
            } else {
                Editor::single_line(window, cx)
            };
            editor.set_placeholder_text(placeholder, window, cx);
            if !initial_value.is_empty() {
                editor.set_text(initial_value, window, cx);
                editor.select_all(&Default::default(), window, cx);
            }
            editor
        });
        window.focus(&editor.focus_handle(cx), cx);
        Self {
            focus_handle: cx.focus_handle(),
            title: title.into(),
            confirm_label: confirm_label.into(),
            editor,
            multiline,
            on_confirm,
        }
    }

    pub(crate) fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let value = self.editor.read(cx).text(cx).trim().to_string();
        if value.is_empty() {
            cx.emit(DismissEvent);
            return;
        }
        (self.on_confirm)(value, window, cx);
        cx.emit(DismissEvent);
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for TextPromptModal {}

impl ModalView for TextPromptModal {}

impl Focusable for TextPromptModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TextPromptModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("TextPromptModal")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &menu::Cancel, _window, cx| this.cancel(cx)))
            .w(px(if self.multiline { 640. } else { 420. }))
            .p_3()
            .gap_3()
            .cyberpunk_surface()
            .shadow(ElevationIndex::ModalSurface.shadow(cx))
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .child(cyberpunk::dialog_title(self.title.clone(), cx))
                    .child(div().flex_1())
                    .child(
                        IconButton::new("text-prompt-dismiss", IconName::Close)
                            .icon_size(IconSize::Small)
                            .style(cyberpunk::Rank::Quiet.style())
                            .tooltip(Tooltip::text("Close"))
                            .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                    ),
            )
            .child(
                div()
                    .p_2()
                    .when(self.multiline, |this| this.h(px(MULTILINE_HEIGHT)))
                    .debug_selector(|| "text-prompt-editor-box".to_string())
                    .rounded_none()
                    .border_1()
                    .border_color(cyberpunk::border_dim())
                    .bg(cyberpunk::surface())
                    .child(self.editor.clone()),
            )
            .child(
                h_flex().justify_end().gap_2().child(
                    Button::new("text-prompt-confirm", self.confirm_label.clone())
                        .style(ButtonStyle::OutlinedCustom(
                            cyberpunk::Accent::Cyan.border(),
                        ))
                        .on_click(cx.listener(|this, _, window, cx| this.confirm(window, cx))),
                ),
            )
    }
}
