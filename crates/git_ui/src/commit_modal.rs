use crate::branch_picker::{self, BranchList};
use crate::git_panel::{
    GitPanel, commit_message_editor, commit_title_exceeds_limit, git_commit_editor_style,
};
use crate::git_panel_settings::GitPanelSettings;
use git::repository::CommitOptions;
use git::{Amend, Commit, GenerateCommitMessage, Signoff};
use project::DisableAiSettings;
use settings::Settings;
use ui::{
    ButtonLike, ContextMenu, KeybindingHint, PopoverMenu, PopoverMenuHandle, SplitButton, Tooltip,
    cyberpunk, prelude::*,
};
use zed_actions::{DecreaseBufferFontSize, IncreaseBufferFontSize, ResetBufferFontSize};

use editor::{Editor, EditorElement};
use gpui::*;
use util::ResultExt;
use workspace::{
    ModalView, Workspace,
    dock::{Dock, PanelHandle},
};

/// How tall the message editor stands inside the dialog's body. The window's
/// width and corner radius are the shell's, not this file's, and this has to
/// leave the header, the footer and the over-limit warning room inside the
/// height every dialog here shares.
const COMMIT_EDITOR_HEIGHT: f32 = 260.0;

pub struct CommitModal {
    git_panel: Entity<GitPanel>,
    commit_editor: Entity<Editor>,
    restore_dock: RestoreDock,
    branch_list_handle: PopoverMenuHandle<BranchList>,
    commit_menu_handle: PopoverMenuHandle<ContextMenu>,
}

impl Focusable for CommitModal {
    fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.commit_editor.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for CommitModal {}
impl ModalView for CommitModal {
    fn on_before_dismiss(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> workspace::DismissDecision {
        self.git_panel.update(cx, |git_panel, cx| {
            git_panel.set_modal_open(false, cx);
        });
        self.restore_dock
            .dock
            .update(cx, |dock, cx| {
                if let Some(active_index) = self.restore_dock.active_index {
                    dock.activate_panel(active_index, window, cx)
                }
                dock.set_open(self.restore_dock.is_open, window, cx)
            })
            .log_err();
        workspace::DismissDecision::Dismiss(true)
    }
}

struct RestoreDock {
    dock: WeakEntity<Dock>,
    is_open: bool,
    active_index: Option<usize>,
}

pub enum ForceMode {
    Amend,
    Commit,
}

impl CommitModal {
    pub fn register(workspace: &mut Workspace) {
        workspace.register_action(|workspace, _: &Commit, window, cx| {
            CommitModal::toggle(workspace, Some(ForceMode::Commit), window, cx);
        });
        workspace.register_action(|workspace, _: &Amend, window, cx| {
            CommitModal::toggle(workspace, Some(ForceMode::Amend), window, cx);
        });
    }

    pub fn toggle(
        workspace: &mut Workspace,
        force_mode: Option<ForceMode>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some(git_panel) = workspace.panel::<GitPanel>(cx) else {
            return;
        };

        git_panel.update(cx, |git_panel, cx| {
            if let Some(force_mode) = force_mode {
                match force_mode {
                    ForceMode::Amend => {
                        if git_panel
                            .active_repository
                            .as_ref()
                            .and_then(|repo| repo.read(cx).head_commit.as_ref())
                            .is_some()
                            && !git_panel.amend_pending()
                        {
                            git_panel.set_amend_pending(true, cx);
                            git_panel.load_last_commit_message(cx);
                        }
                    }
                    ForceMode::Commit => {
                        if git_panel.amend_pending() {
                            git_panel.set_amend_pending(false, cx);
                        }
                    }
                }
            }
            git_panel.set_modal_open(true, cx);
            git_panel.load_local_committer(cx);
        });

        let dock = workspace.dock_at_position(git_panel.position(window, cx));
        let is_open = dock.read(cx).is_open();
        let active_index = dock.read(cx).active_panel_index();
        let dock = dock.downgrade();
        let restore_dock_position = RestoreDock {
            dock,
            is_open,
            active_index,
        };

        workspace.open_panel::<GitPanel>(window, cx);
        workspace.toggle_modal(window, cx, move |window, cx| {
            CommitModal::new(git_panel, restore_dock_position, window, cx)
        })
    }

    fn new(
        git_panel: Entity<GitPanel>,
        restore_dock: RestoreDock,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let panel = git_panel.read(cx);
        let suggested_commit_message = panel.suggest_commit_message(cx);

        let commit_editor = git_panel.update(cx, |git_panel, cx| {
            git_panel.set_modal_open(true, cx);
            let buffer = git_panel.commit_message_buffer(cx);
            let panel_editor = git_panel.commit_editor.clone();
            let project = git_panel.project.clone();

            cx.new(|cx| {
                let mut editor =
                    commit_message_editor(buffer, None, project.clone(), false, window, cx);
                editor.sync_selections(panel_editor, cx).detach();

                editor
            })
        });

        let commit_message = commit_editor.read(cx).text(cx);

        if let Some(suggested_commit_message) = suggested_commit_message
            && commit_message.is_empty()
        {
            commit_editor.update(cx, |editor, cx| {
                editor.set_placeholder_text(&suggested_commit_message, window, cx);
            });
        }

        let focus_handle = commit_editor.focus_handle(cx);

        cx.on_focus_out(&focus_handle, window, |this, _, window, cx| {
            if !this.branch_list_handle.is_focused(window, cx)
                && !this.commit_menu_handle.is_focused(window, cx)
            {
                cx.emit(DismissEvent);
            }
        })
        .detach();

        Self {
            git_panel,
            commit_editor,
            restore_dock,
            branch_list_handle: PopoverMenuHandle::default(),
            commit_menu_handle: PopoverMenuHandle::default(),
        }
    }

    fn commit_editor_element(&self, _window: &mut Window, cx: &mut Context<Self>) -> EditorElement {
        let settings = theme_settings::ThemeSettings::get_global(cx);
        let editor_style = git_commit_editor_style(settings.git_commit_buffer_font_size(cx), cx);
        EditorElement::new(&self.commit_editor, editor_style)
    }

    pub fn render_commit_editor(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let padding_t = 3.0;
        let padding_b = 6.0;
        // The editor reports a height that is one and a half lines short of what
        // it goes on to paint, so the box it sits in has to be that much taller
        // or the last line is clipped.
        let extra_space_hack = 1.5 * window.line_height();

        v_flex()
            .h(px(COMMIT_EDITOR_HEIGHT + padding_b + padding_t) + extra_space_hack)
            .w_full()
            .flex_none()
            .overflow_hidden()
            .pt(px(padding_t))
            .pb(px(padding_b))
            .child(
                div()
                    .h(px(COMMIT_EDITOR_HEIGHT))
                    .w_full()
                    .child(self.commit_editor_element(window, cx)),
            )
    }

    fn render_git_commit_menu(
        &self,
        id: impl Into<ElementId>,
        keybinding_target: Option<FocusHandle>,
        disabled: bool,
    ) -> impl IntoElement {
        let menu_open = self.commit_menu_handle.is_deployed();

        PopoverMenu::new(id.into())
            .with_handle(self.commit_menu_handle.clone())
            .trigger(
                crate::render_split_button_chevron_trigger(
                    "modal-commit-split-button-right",
                    menu_open,
                )
                .disabled(disabled),
            )
            .menu({
                let git_panel_entity = self.git_panel.clone();
                move |window, cx| {
                    let git_panel = git_panel_entity.read(cx);
                    let amend_enabled = git_panel.amend_pending();
                    let signoff_enabled = git_panel.signoff_enabled();
                    let has_previous_commit = git_panel.head_commit(cx).is_some();

                    Some(ContextMenu::build(window, cx, |context_menu, _, _| {
                        context_menu
                            .when_some(keybinding_target.clone(), |el, keybinding_target| {
                                el.context(keybinding_target)
                            })
                            .when(has_previous_commit, |this| {
                                this.toggleable_entry(
                                    "Amend",
                                    amend_enabled,
                                    IconPosition::Start,
                                    Some(Box::new(Amend)),
                                    {
                                        let git_panel = git_panel_entity.downgrade();
                                        move |_, cx| {
                                            git_panel
                                                .update(cx, |git_panel, cx| {
                                                    git_panel.toggle_amend_pending(cx);
                                                })
                                                .ok();
                                        }
                                    },
                                )
                            })
                            .toggleable_entry(
                                "Signoff",
                                signoff_enabled,
                                IconPosition::Start,
                                Some(Box::new(Signoff)),
                                {
                                    let git_panel = git_panel_entity.clone();
                                    move |window, cx| {
                                        git_panel.update(cx, |git_panel, cx| {
                                            git_panel.toggle_signoff_enabled(&Signoff, window, cx);
                                        })
                                    }
                                },
                            )
                    }))
                }
            })
            .offset(gpui::Point {
                x: px(0.),
                y: px(2.),
            })
            .anchor(Anchor::TopRight)
    }

    pub fn render_footer(&self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (
            can_commit,
            tooltip,
            commit_label,
            co_authors,
            generate_commit_message,
            active_repo,
            is_amend_pending,
            is_signoff_enabled,
            workspace,
            is_generating,
        ) = self.git_panel.update(cx, |git_panel, cx| {
            let (can_commit, tooltip) = git_panel.configure_commit_button(cx);
            let title = git_panel.commit_button_title();
            let co_authors = git_panel.render_co_authors(cx);
            let generate_commit_message = git_panel.render_generate_commit_message_button(cx);
            let active_repo = git_panel.active_repository.clone();
            let is_amend_pending = git_panel.amend_pending();
            let is_signoff_enabled = git_panel.signoff_enabled();
            let is_generating = git_panel.is_generating_commit_message();
            (
                can_commit,
                tooltip,
                title,
                co_authors,
                generate_commit_message,
                active_repo,
                is_amend_pending,
                is_signoff_enabled,
                git_panel.workspace.clone(),
                is_generating,
            )
        });

        let branch = active_repo
            .as_ref()
            .and_then(|repo| repo.read(cx).branch.as_ref())
            .map(|b| b.name().to_owned())
            .unwrap_or_else(|| "<no branch>".to_owned());

        let branch_picker_button = Button::new("branch_picker_button", branch)
            .label_size(LabelSize::Small)
            .start_icon(
                Icon::new(IconName::GitBranch)
                    .size(IconSize::Small)
                    .color(Color::Placeholder),
            )
            .on_click(cx.listener(|_, _, window, cx| {
                window.dispatch_action(zed_actions::git::Branch.boxed_clone(), cx);
            }));

        let branch_picker = PopoverMenu::new("popover-button")
            .menu(move |window, cx| {
                Some(branch_picker::popover(
                    workspace.clone(),
                    false,
                    active_repo.clone(),
                    window,
                    cx,
                ))
            })
            .with_handle(self.branch_list_handle.clone())
            .trigger_with_tooltip(
                branch_picker_button,
                Tooltip::for_action_title("Switch Branch", &zed_actions::git::Branch),
            )
            .anchor(Anchor::BottomLeft)
            .offset(gpui::Point {
                x: px(0.0),
                y: px(-2.0),
            });

        let focus_handle = self.focus_handle(cx);

        let close_kb_hint = ui::KeyBinding::for_action(&menu::Cancel, cx)
            .map(|close_kb| KeybindingHint::new(close_kb, cyberpunk::canvas()).suffix("Cancel"));

        cyberpunk::dialog_footer()
            .group("commit_editor_footer")
            .child(
                cyberpunk::dialog_footer_left()
                    .gap_1()
                    .child(
                        h_flex()
                            .min_w_0()
                            .flex_shrink_1()
                            .overflow_x_hidden()
                            .child(branch_picker),
                    )
                    .children(generate_commit_message)
                    .children(co_authors),
            )
            .child(cyberpunk::dialog_footer_spacer())
            .child(
                h_flex()
                    .gap_2()
                    .child(close_kb_hint)
                    .child(SplitButton::new(
                        ButtonLike::new_rounded_left(format!("split-button-left-{}", commit_label))
                            .style(cyberpunk::Rank::Accent.style())
                            .size(ButtonSize::Compact)
                            .disabled(!can_commit)
                            .child(Label::new(commit_label).size(LabelSize::Small).mr_0p5())
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                telemetry::event!("Git Committed", source = "Git Modal");
                                this.git_panel.update(cx, |git_panel, cx| {
                                    git_panel.commit_changes(
                                        CommitOptions {
                                            amend: is_amend_pending,
                                            signoff: is_signoff_enabled,
                                            allow_empty: false,
                                        },
                                        window,
                                        cx,
                                    )
                                });
                                cx.emit(DismissEvent);
                            }))
                            .tooltip({
                                let focus_handle = focus_handle.clone();
                                move |_window, cx| {
                                    if can_commit {
                                        Tooltip::with_meta_in(
                                            tooltip,
                                            Some(&git::Commit),
                                            format!(
                                                "git commit{}{}",
                                                if is_amend_pending { " --amend" } else { "" },
                                                if is_signoff_enabled { " --signoff" } else { "" }
                                            ),
                                            &focus_handle.clone(),
                                            cx,
                                        )
                                    } else {
                                        Tooltip::simple(tooltip, cx)
                                    }
                                }
                            }),
                        self.render_git_commit_menu(
                            format!("split-button-right-{}", commit_label),
                            Some(focus_handle),
                            is_generating,
                        )
                        .into_any_element(),
                    )),
            )
    }

    fn dismiss(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        if self.git_panel.read(cx).amend_pending() {
            self.git_panel
                .update(cx, |git_panel, cx| git_panel.set_amend_pending(false, cx));
        } else {
            cx.emit(DismissEvent);
        }
    }

    fn on_commit(&mut self, _: &git::Commit, window: &mut Window, cx: &mut Context<Self>) {
        let is_amend = self.git_panel.read(cx).amend_pending();
        let did_execute = self.git_panel.update(cx, |git_panel, cx| {
            git_panel.commit(&self.commit_editor.focus_handle(cx), window, cx)
        });
        if did_execute {
            if is_amend {
                telemetry::event!("Git Amended", source = "Git Modal");
            } else {
                telemetry::event!("Git Committed", source = "Git Modal");
            }
            cx.emit(DismissEvent);
        }
    }

    fn on_amend(&mut self, _: &git::Amend, window: &mut Window, cx: &mut Context<Self>) {
        if self.git_panel.update(cx, |git_panel, cx| {
            git_panel.amend(&self.commit_editor.focus_handle(cx), window, cx)
        }) {
            telemetry::event!("Git Amended", source = "Git Modal");
            cx.emit(DismissEvent);
        }
    }

    fn toggle_branch_selector(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.branch_list_handle.is_focused(window, cx) {
            self.focus_handle(cx).focus(window, cx)
        } else {
            self.branch_list_handle.toggle(window, cx);
        }
    }

    fn increase_font_size(
        &mut self,
        action: &IncreaseBufferFontSize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.git_panel.update(cx, |git_panel, cx| {
            git_panel.increase_font_size(action, window, cx);
        });
    }

    fn decrease_font_size(
        &mut self,
        action: &DecreaseBufferFontSize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.git_panel.update(cx, |git_panel, cx| {
            git_panel.decrease_font_size(action, window, cx);
        });
    }

    fn reset_font_size(
        &mut self,
        action: &ResetBufferFontSize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.git_panel.update(cx, |git_panel, cx| {
            git_panel.reset_font_size(action, window, cx);
        });
    }
}

impl Render for CommitModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let editor_focus_handle = self.commit_editor.focus_handle(cx);
        let commit_editor = self.render_commit_editor(window, cx).into_any_element();
        let footer = self.render_footer(window, cx).into_any_element();

        let max_title_length = GitPanelSettings::get_global(cx).commit_title_max_length;
        let title_exceeds_limit = if max_title_length > 0 {
            self.commit_editor
                .read(cx)
                .text(cx)
                .lines()
                .next()
                .is_some_and(|title| commit_title_exceeds_limit(title, max_title_length))
        } else {
            false
        };

        cyberpunk::dialog_shell(cx)
            .id("commit-modal")
            .key_context("GitCommit")
            .on_action(cx.listener(Self::dismiss))
            .on_action(cx.listener(Self::on_commit))
            .on_action(cx.listener(Self::on_amend))
            .on_action(cx.listener(Self::increase_font_size))
            .on_action(cx.listener(Self::decrease_font_size))
            .on_action(cx.listener(Self::reset_font_size))
            .when(!DisableAiSettings::get_global(cx).disable_ai, |this| {
                this.on_action(cx.listener(|this, _: &GenerateCommitMessage, _, cx| {
                    this.git_panel.update(cx, |panel, cx| {
                        panel.generate_commit_message(cx);
                    })
                }))
            })
            .on_action(
                cx.listener(|this, _: &zed_actions::git::Branch, window, cx| {
                    this.toggle_branch_selector(window, cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &zed_actions::git::CheckoutBranch, window, cx| {
                    this.toggle_branch_selector(window, cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &zed_actions::git::Switch, window, cx| {
                    this.toggle_branch_selector(window, cx);
                }),
            )
            .child(
                cyberpunk::dialog_header("Commit", cx).child(
                    div().debug_selector(|| "DIALOG-CLOSE".to_string()).child(
                        IconButton::new("commit-modal-close", IconName::Close)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Close"))
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    ),
                ),
            )
            .child(
                cyberpunk::dialog_body().child(
                    v_flex()
                        .id("editor-container")
                        .w_full()
                        .px_3()
                        .pb_3()
                        .gap_1()
                        .cursor_text()
                        .on_click(cx.listener(move |_, _: &ClickEvent, window, cx| {
                            window.focus(&editor_focus_handle, cx);
                        }))
                        .child(cyberpunk::dialog_field("Message", true, cx, commit_editor))
                        .when(title_exceeds_limit, |this| {
                            this.child(
                                h_flex()
                                    .w_full()
                                    .gap_1()
                                    .child(
                                        Icon::new(IconName::Warning)
                                            .size(IconSize::XSmall)
                                            .color(Color::Error),
                                    )
                                    .child(
                                        Label::new(format!(
                                            "Commit message title exceeds \
                                             {max_title_length}-character limit."
                                        ))
                                        .size(LabelSize::Small)
                                        .color(Color::Error),
                                    ),
                            )
                        }),
                ),
            )
            .child(footer)
    }
}
