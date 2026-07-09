use editor::Editor;
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render, Window,
};
use std::sync::Arc;
use ui::prelude::*;
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
    on_confirm: Arc<dyn Fn(String, &mut Window, &mut App)>,
}

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
            .w(px(420.))
            .p_3()
            .gap_3()
            .bg(cx.theme().colors().elevated_surface_background)
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().colors().border)
            .child(Label::new(self.title.clone()).size(LabelSize::Large))
            .child(
                div()
                    .p_2()
                    .rounded_md()
                    .border_1()
                    .border_color(cx.theme().colors().border_variant)
                    .bg(cx.theme().colors().editor_background)
                    .child(self.editor.clone()),
            )
            .child(
                h_flex().justify_end().gap_2().child(
                    Button::new("text-prompt-confirm", self.confirm_label.clone())
                        .style(ButtonStyle::Filled)
                        .on_click(cx.listener(|this, _, window, cx| this.confirm(window, cx))),
                ),
            )
    }
}
