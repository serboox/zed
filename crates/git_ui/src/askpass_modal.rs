use askpass::EncryptedPassword;
use editor::Editor;
use futures::channel::oneshot;
use gpui::{AppContext, DismissEvent, Entity, EventEmitter, Focusable, Styled};
use ui::{
    AnyElement, App, Button, ButtonCommon, Clickable, Color, Context, Icon, IconButton, IconName,
    IconSize, InteractiveElement, IntoElement, Label, LabelCommon, LabelSize, ParentElement,
    Render, SharedString, Tooltip, Window, cyberpunk, div, h_flex, v_flex,
};
use util::maybe;
use workspace::ModalView;
use zeroize::Zeroize;

pub(crate) struct AskPassModal {
    operation: SharedString,
    prompt: SharedString,
    editor: Entity<Editor>,
    tx: Option<oneshot::Sender<EncryptedPassword>>,
}

impl EventEmitter<DismissEvent> for AskPassModal {}
impl ModalView for AskPassModal {}
impl Focusable for AskPassModal {
    fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl AskPassModal {
    pub fn new(
        operation: SharedString,
        prompt: SharedString,
        tx: oneshot::Sender<EncryptedPassword>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            if prompt.contains("yes/no") || prompt.contains("Username") {
                editor.set_masked(false, cx);
            } else {
                editor.set_masked(true, cx);
            }
            editor
        });
        Self {
            operation,
            prompt,
            editor,
            tx: Some(tx),
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        maybe!({
            let tx = self.tx.take()?;
            let mut text = self.editor.update(cx, |this, cx| {
                let text = this.text(cx);
                this.clear(window, cx);
                text
            });
            let pw = askpass::EncryptedPassword::try_from(text.as_ref()).ok()?;
            text.zeroize();
            tx.send(pw).ok();
            Some(())
        });

        cx.emit(DismissEvent);
    }

    /// Lives on the footer's left rather than in a band of its own: it is
    /// incidental to the one thing this window is waiting for.
    fn render_hint(&self) -> Option<AnyElement> {
        if (self.prompt.contains("Password") || self.prompt.contains("Username"))
            && self.prompt.contains("github.com")
        {
            return Some(
                h_flex()
                    .gap_2()
                    .min_w_0()
                    .overflow_hidden()
                    .child(Icon::new(IconName::Github).size(IconSize::Small))
                    .child(
                        Label::new("You may need to configure git for GitHub.")
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate(),
                    )
                    .child(
                        Button::new("learn-more", "Learn more")
                            .label_size(LabelSize::Small)
                            .style(cyberpunk::Rank::Quiet.style())
                            .on_click(|_, _, cx| {
                                cx.open_url(
                                    "https://docs.github.com/en/get-started/git-basics/set-up-git\
                                     #authenticating-with-github-from-git",
                                )
                            }),
                    )
                    .into_any_element(),
            );
        }
        None
    }
}

impl Render for AskPassModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let hint = self.render_hint();
        cyberpunk::dialog_shell(cx)
            .key_context("PasswordPrompt")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .child(
                cyberpunk::dialog_header(self.operation.clone(), cx).child(
                    div().debug_selector(|| "DIALOG-CLOSE".to_string()).child(
                        IconButton::new("askpass-close", IconName::Close)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Close"))
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    ),
                ),
            )
            .child(
                cyberpunk::dialog_body().child(
                    v_flex()
                        .w_full()
                        .px_3()
                        .pb_3()
                        .gap_2()
                        .child(
                            Label::new(self.prompt.clone())
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .child(cyberpunk::dialog_field(
                            "Answer",
                            false,
                            cx,
                            self.editor.clone(),
                        )),
                ),
            )
            .child(
                cyberpunk::dialog_footer()
                    .child(cyberpunk::dialog_footer_left().children(hint))
                    .child(cyberpunk::dialog_footer_spacer())
                    .child(
                        Button::new("askpass-cancel", "Cancel")
                            .label_size(LabelSize::Small)
                            .style(cyberpunk::Rank::Neutral.style())
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        Button::new("askpass-confirm", "Continue")
                            .label_size(LabelSize::Small)
                            .style(cyberpunk::Rank::Accent.style())
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.confirm(&menu::Confirm, window, cx);
                            })),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
    }

    fn draw_the_prompt(cx: &mut TestAppContext) -> &mut gpui::VisualTestContext {
        init_test(cx);
        let (sender, _receiver) = oneshot::channel();
        let (_modal, cx) = cx.add_window_view(|window, cx| {
            AskPassModal::new(
                "git fetch".into(),
                "Password for 'https://github.com':".into(),
                sender,
                window,
                cx,
            )
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        cx.run_until_parked();
        cx
    }

    #[gpui::test]
    async fn the_password_prompt_ends_its_answer_in_the_bottom_right_corner(
        cx: &mut TestAppContext,
    ) {
        let cx = draw_the_prompt(cx);

        let footer = cx
            .debug_bounds("DIALOG-FOOTER")
            .expect("the footer is painted");
        let confirm = cx
            .debug_bounds("BUTTON-Continue")
            .expect("the confirming action is painted");
        let cancel = cx
            .debug_bounds("BUTTON-Cancel")
            .expect("the dismissing action is painted");

        assert!(
            confirm.right() > footer.left() + footer.size.width * 0.85,
            "the confirming action ends at {:?} in a bar spanning {:?}..{:?}, so it is not in \
             the corner",
            confirm.right(),
            footer.left(),
            footer.right()
        );
        assert!(
            cancel.right() <= confirm.left(),
            "the confirming action comes last, so Cancel at {:?}..{:?} sits left of Continue at \
             {:?}..{:?}",
            cancel.left(),
            cancel.right(),
            confirm.left(),
            confirm.right()
        );
        assert!(
            confirm.bottom() <= footer.bottom() + gpui::px(0.5),
            "the confirming action ends at {:?} below the bar ending at {:?}",
            confirm.bottom(),
            footer.bottom()
        );
    }

    #[gpui::test]
    async fn the_password_prompt_keeps_its_way_out_in_the_top_right_corner(
        cx: &mut TestAppContext,
    ) {
        let cx = draw_the_prompt(cx);

        let header = cx
            .debug_bounds("DIALOG-HEADER")
            .expect("the header is painted");
        let close = cx
            .debug_bounds("DIALOG-CLOSE")
            .expect("the way out is painted");

        assert!(
            close.right() > header.left() + header.size.width * 0.85,
            "the way out ends at {:?} in a header spanning {:?}..{:?}, so it is not in the corner",
            close.right(),
            header.left(),
            header.right()
        );
        assert!(
            close.top() >= header.top() - gpui::px(0.5)
                && close.bottom() <= header.bottom() + gpui::px(0.5),
            "the way out is painted {:?}..{:?} outside a header spanning {:?}..{:?}",
            close.top(),
            close.bottom(),
            header.top(),
            header.bottom()
        );
    }
}
