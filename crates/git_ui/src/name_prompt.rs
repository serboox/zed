use editor::Editor;
use gpui::{AppContext as _, DismissEvent, Entity, EventEmitter, Focusable};
use std::rc::Rc;
use ui::{
    App, Button, ButtonCommon, Clickable, Context, IconButton, IconName, IconSize,
    InteractiveElement, IntoElement, LabelSize, ParentElement, Render, SharedString, Styled,
    Tooltip, Window, cyberpunk, div, v_flex,
};
use workspace::ModalView;

/// Asks for one name and hands it back.
///
/// Creating a branch or a tag at a commit, and renaming a branch, are the same
/// question with a different title, and none of them is worth a screen of its
/// own.
pub(crate) struct NamePrompt {
    title: SharedString,
    field: SharedString,
    editor: Entity<Editor>,
    answered: Option<Rc<dyn Fn(SharedString, &mut Window, &mut App)>>,
}

impl EventEmitter<DismissEvent> for NamePrompt {}
impl ModalView for NamePrompt {}
impl Focusable for NamePrompt {
    fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl NamePrompt {
    pub(crate) fn new(
        title: impl Into<SharedString>,
        field: impl Into<SharedString>,
        starting_with: impl Into<SharedString>,
        answered: impl Fn(SharedString, &mut Window, &mut App) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let starting_with = starting_with.into();
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            if !starting_with.is_empty() {
                editor.set_text(starting_with.as_ref(), window, cx);
                editor.select_all(&Default::default(), window, cx);
            }
            editor
        });

        Self {
            title: title.into(),
            field: field.into(),
            editor,
            answered: Some(Rc::new(answered)),
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let name: SharedString = self.editor.read(cx).text(cx).trim().to_string().into();
        // An empty name is the reader changing their mind, not a command.
        if name.is_empty() {
            cx.emit(DismissEvent);
            return;
        }
        if let Some(answered) = self.answered.take() {
            answered(name, window, cx);
        }
        cx.emit(DismissEvent);
    }
}

impl Render for NamePrompt {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        cyberpunk::dialog_shell("Name", window, cx)
            .key_context("NamePrompt")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .child(
                cyberpunk::dialog_header(self.title.clone(), cx).child(
                    div().debug_selector(|| "DIALOG-CLOSE".to_string()).child(
                        IconButton::new("name-prompt-close", IconName::Close)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Close"))
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    ),
                ),
            )
            .child(
                cyberpunk::dialog_body().child(v_flex().w_full().px_3().pb_3().child(
                    cyberpunk::dialog_field(self.field.clone(), false, cx, self.editor.clone()),
                )),
            )
            .child(
                cyberpunk::dialog_footer()
                    .child(cyberpunk::dialog_footer_left())
                    .child(cyberpunk::dialog_footer_spacer())
                    .child(
                        Button::new("name-prompt-cancel", "Cancel")
                            .min_width(cyberpunk::DIALOG_ACTION_MIN_WIDTH)
                            .label_size(LabelSize::Small)
                            .style(cyberpunk::Rank::Neutral.style())
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        Button::new("name-prompt-confirm", "Create")
                            .min_width(cyberpunk::DIALOG_ACTION_MIN_WIDTH)
                            .label_size(LabelSize::Small)
                            .style(cyberpunk::Rank::Accent.style())
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.confirm(&menu::Confirm, window, cx);
                            })),
                    ),
            )
    }
}
