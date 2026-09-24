use editor::Editor;
use gpui::{AppContext as _, DismissEvent, Entity, EventEmitter, Focusable, ReadGlobal, Styled};
use ui::{
    App, Color, Context, InteractiveElement, IntoElement, Label, LabelCommon, LabelSize,
    ParentElement, Render, SharedString, Window, cyberpunk, div,
};
use workspace::ModalView;

use super::{OpenListener, RawOpenRequest};

pub struct OpenUrlModal {
    editor: Entity<Editor>,
    last_error: Option<SharedString>,
}

impl EventEmitter<DismissEvent> for OpenUrlModal {}
impl ModalView for OpenUrlModal {}

impl Focusable for OpenUrlModal {
    fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl OpenUrlModal {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("zed://...", window, cx);
            editor
        });

        Self {
            editor,
            last_error: None,
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let url = self.editor.update(cx, |editor, cx| {
            let text = editor.text(cx).trim().to_string();
            editor.clear(window, cx);
            text
        });

        if url.is_empty() {
            cx.emit(DismissEvent);
            return;
        }

        // Handle zed:// URLs internally.
        if url.starts_with("zed://") || url.starts_with("zed-cli://") {
            OpenListener::global(cx).open(RawOpenRequest {
                urls: vec![url],
                ..Default::default()
            });
            cx.emit(DismissEvent);
            return;
        }

        match url::Url::parse(&url) {
            Ok(parsed_url) => {
                cx.open_url(parsed_url.as_str());
                cx.emit(DismissEvent);
            }
            Err(e) => {
                self.last_error = Some(format!("Invalid URL: {}", e).into());
                cx.notify();
            }
        }
    }
}

impl Render for OpenUrlModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let hint = match self.last_error.clone() {
            Some(error) => Label::new(error).size(LabelSize::Small).color(Color::Error),
            None => Label::new("Paste a URL to open.")
                .color(Color::Muted)
                .size(LabelSize::Small),
        };

        cyberpunk::dialog_shell("Open URL", window, cx)
            .key_context("OpenUrlModal")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .child(cyberpunk::dialog_header("Open URL", cx))
            .child(cyberpunk::dialog_body().child(div().w_full().p_2().child(self.editor.clone())))
            .child(cyberpunk::dialog_footer().child(cyberpunk::dialog_footer_left().child(hint)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
    }

    #[gpui::test]
    fn the_open_url_modal_renders_inside_a_framed_dialog_shell(cx: &mut TestAppContext) {
        init_test(cx);
        let window = cx.add_window(|window, cx| OpenUrlModal::new(window, cx));
        let mut cx = VisualTestContext::from_window(window.into(), cx);

        cx.update(|window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        });

        let bounds = cx.debug_bounds("DIALOG-SHELL");
        assert!(
            bounds.is_some(),
            "the open-URL prompt renders inside a framed, draggable dialog shell"
        );
    }
}
