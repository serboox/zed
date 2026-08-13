use gpui::{
    App, Decorations, Entity, EventEmitter, FocusHandle, Focusable, PromptButton, PromptHandle,
    PromptLevel, PromptResponse, RenderablePromptHandle, SharedString, TextStyleRefinement, Window,
    div, prelude::*,
};
use markdown::{Markdown, MarkdownElement, MarkdownStyle};
use settings::{Settings, SettingsStore};
use theme::ClientDecorationsExt;
use theme_settings::ThemeSettings;
use ui::{ButtonLike, ButtonSize, ElevationIndex, FluentBuilder, cyberpunk, prelude::*};
use workspace::WorkspaceSettings;

pub fn init(cx: &mut App) {
    process_settings(cx);

    cx.observe_global::<SettingsStore>(process_settings)
        .detach();
}

fn process_settings(cx: &mut App) {
    let settings = WorkspaceSettings::get_global(cx);
    if settings.use_system_prompts && cfg!(not(any(target_os = "linux", target_os = "freebsd"))) {
        cx.reset_prompt_builder();
    } else {
        cx.set_prompt_builder(zed_prompt_renderer);
    }
}

/// Use this function in conjunction with [App::set_prompt_builder] to force
/// GPUI to use the internal prompt system.
fn zed_prompt_renderer(
    level: PromptLevel,
    message: &str,
    detail: Option<&str>,
    actions: &[PromptButton],
    handle: PromptHandle,
    window: &mut Window,
    cx: &mut App,
) -> RenderablePromptHandle {
    let renderer = cx.new({
        |cx| ZedPromptRenderer {
            level,
            message: cx.new(|cx| Markdown::new(SharedString::new(message), None, None, cx)),
            actions: actions.iter().map(|a| a.label().to_string()).collect(),
            focus: cx.focus_handle(),
            active_action_id: 0,
            detail: detail
                .filter(|text| !text.is_empty())
                .map(|text| cx.new(|cx| Markdown::new(SharedString::new(text), None, None, cx))),
        }
    });

    handle.with_view(renderer, window, cx)
}

pub struct ZedPromptRenderer {
    level: PromptLevel,
    message: Entity<Markdown>,
    actions: Vec<String>,
    focus: FocusHandle,
    active_action_id: usize,
    detail: Option<Entity<Markdown>>,
}

impl ZedPromptRenderer {
    fn confirm(&mut self, _: &menu::Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(PromptResponse(self.active_action_id));
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.actions.iter().position(|a| a == "Cancel") {
            cx.emit(PromptResponse(ix));
        }
    }

    fn select_first(
        &mut self,
        _: &menu::SelectFirst,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.active_action_id = self.actions.len().saturating_sub(1);
        cx.notify();
    }

    fn select_last(&mut self, _: &menu::SelectLast, _window: &mut Window, cx: &mut Context<Self>) {
        self.active_action_id = 0;
        cx.notify();
    }

    fn select_next(&mut self, _: &menu::SelectNext, _window: &mut Window, cx: &mut Context<Self>) {
        self.active_action_id = (self.active_action_id + 1) % self.actions.len();
        cx.notify();
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.active_action_id > 0 {
            self.active_action_id -= 1;
        } else {
            self.active_action_id = self.actions.len().saturating_sub(1);
        }
        cx.notify();
    }
}

impl Render for ZedPromptRenderer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let accent = cyberpunk::accent_for_prompt_level(self.level);

        let dialog = v_flex()
            .key_context("Prompt")
            .cursor_default()
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .w(px(400.))
            .cyberpunk_surface()
            .shadow(ElevationIndex::ModalSurface.shadow(cx))
            .overflow_hidden()
            .cyberpunk_monospace(cx)
            // The bar across the top says what kind of answer is being asked for
            // before the sentence is read: red for anything with consequences,
            // cyan for a routine notice.
            .child(div().w_full().h(px(2.)).bg(accent.border()))
            .child(
                v_flex()
                    .p(cyberpunk::SPACE_18)
                    .gap(cyberpunk::SPACE_14)
                    .child(div().w_full().child(MarkdownElement::new(
                        self.message.clone(),
                        markdown_style(true, window, cx),
                    )))
                    .children(self.detail.clone().map(|detail| {
                        div().w_full().text_xs().child(MarkdownElement::new(
                            detail,
                            markdown_style(false, window, cx),
                        ))
                    })),
            )
            .child(
                v_flex()
                    .p(cyberpunk::SPACE_18)
                    .pt_0()
                    .gap(cyberpunk::SPACE_4)
                    .children(self.actions.iter().enumerate().map(|(ix, action)| {
                        let selected = ix == self.active_action_id;
                        // A `ButtonLike` rather than hand-drawn rows: the button
                        // role, keyboard activation and pressed state all come
                        // with it. Square and outlined, like the rest of this
                        // chrome, and sized up because the default outline reads
                        // as cramped against an 18-point rhythm.
                        ButtonLike::new(ix)
                            .style(if selected {
                                ButtonStyle::OutlinedCustom(accent.border())
                            } else {
                                ButtonStyle::OutlinedCustom(cyberpunk::border_raised())
                            })
                            .square()
                            .size(ButtonSize::Large)
                            .full_width()
                            .tab_index(ix as isize)
                            .child(
                                h_flex()
                                    .w_full()
                                    .justify_center()
                                    .cyberpunk_monospace(cx)
                                    .child(Label::new(action.to_uppercase()).color(Color::Custom(
                                        if selected {
                                            accent.bright()
                                        } else {
                                            cyberpunk::text_secondary()
                                        },
                                    ))),
                            )
                            .on_click(cx.listener(move |_, _, _window, cx| {
                                cx.emit(PromptResponse(ix));
                            }))
                    })),
            );

        let decorations = window.window_decorations();
        let inset = window.client_inset().unwrap_or(Pixels::ZERO);

        div().size_full().child(
            v_flex()
                .occlude()
                .absolute()
                .inset_0()
                .bg(gpui::black().opacity(0.2))
                .map(|this| match decorations {
                    Decorations::Server => this,
                    Decorations::Client { tiling } => this
                        .when(!tiling.top, |this| this.top(inset))
                        .when(!tiling.bottom, |this| this.bottom(inset))
                        .when(!tiling.left, |this| this.left(inset))
                        .when(!tiling.right, |this| this.right(inset))
                        .rounded_client_corners(tiling),
                })
                .items_center()
                .justify_center()
                .child(dialog),
        )
    }
}

fn markdown_style(main_message: bool, window: &Window, cx: &App) -> MarkdownStyle {
    let mut base_text_style = window.text_style();
    let settings = ThemeSettings::get_global(cx);
    let font_size = settings.ui_font_size(cx).into();

    let color = if main_message {
        cyberpunk::text_primary()
    } else {
        cyberpunk::text_secondary()
    };

    base_text_style.refine(&TextStyleRefinement {
        // The buffer font rather than the interface one: the message is the only
        // text in this dialog, and proportional type here is what made it read as
        // a system alert dropped into the editor.
        font_family: Some(theme::theme_settings(cx).buffer_font(cx).family.clone()),
        font_size: Some(font_size),
        color: Some(color),
        ..Default::default()
    });

    MarkdownStyle {
        base_text_style,
        selection_background_color: cx.theme().colors().element_selection_background,
        ..Default::default()
    }
}

impl EventEmitter<PromptResponse> for ZedPromptRenderer {}

impl Focusable for ZedPromptRenderer {
    fn focus_handle(&self, _: &crate::App) -> FocusHandle {
        self.focus.clone()
    }
}
