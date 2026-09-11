use crate::commit_view::CommitView;
use git::Oid;
use gpui::{Action, ClipboardItem, Entity, FocusHandle, SharedString, WeakEntity, Window, actions};
use project::{GIT_COMMAND_TASK_TAG, git_store::Repository};

use git::repository::ResetMode;
use std::rc::Rc;
use task::{TaskContext, TaskVariables, VariableName};
use ui::{App, Color, ContextMenu, ContextMenuEntry, IconName, IconPosition, prelude::*};
use workspace::Workspace;

actions!(
    git_graph,
    [
        /// Copies the SHA of the selected commit to the clipboard.
        CopyCommitSha,
        /// Copies a tag from the selected commit to the clipboard.
        CopyCommitTag,
        /// Opens the commit view for the selected commit.
        OpenCommitView,
    ]
);

const COMMIT_TAG_LIST_WIDTH_IN_REMS: Rems = rems(10.);
const CUSTOM_GIT_COMMANDS_DOCS_SLUG: &str = "tasks#custom-git-commands";

pub(crate) struct CommitContextMenuData {
    pub(crate) sha: Oid,
    pub(crate) tag_names: Vec<SharedString>,
    /// Every commit the reader has picked, oldest last, when the menu was
    /// opened over more than one.
    pub(crate) selected: Vec<Oid>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommitContextMenuSource {
    GitGraph,
    GitPanel,
}

pub(crate) fn commit_context_menu(
    commit: CommitContextMenuData,
    source: CommitContextMenuSource,
    ref_name: Option<SharedString>,
    focus_handle: FocusHandle,
    repository: Option<WeakEntity<Repository>>,
    workspace: WeakEntity<Workspace>,
    // What "Solo" does, where the view offers it. The panel has no filter of
    // its own, so it passes nothing and the entry does not appear.
    solo: Option<Rc<dyn Fn(SharedString, &mut App)>>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ContextMenu> {
    let sha = commit.sha;
    let selected = commit.selected.clone();
    let sha_short = sha.display_short();
    let git_tasks = git_context_menu_tasks(
        git_task_context(&repository, sha, ref_name.as_deref(), cx),
        &workspace,
        cx,
    );
    let header = match &ref_name {
        Some(ref_name) => format!("Ref {ref_name}"),
        None => format!("Commit {sha_short}"),
    };

    ContextMenu::build(window, cx, move |context_menu, _, _| {
        context_menu
            .context(focus_handle)
            .header(header)
            .entry("View Commit", Some(OpenCommitView.boxed_clone()), {
                let repository = repository.clone();
                let workspace = workspace.clone();
                move |window, cx| {
                    let Some(repository) = repository.clone() else {
                        return;
                    };
                    CommitView::open(
                        sha.to_string(),
                        repository,
                        workspace.clone(),
                        None,
                        None,
                        window,
                        cx,
                    );
                }
            })
            .entry(
                "Copy SHA",
                Some(CopyCommitSha.boxed_clone()),
                move |_window, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(sha.to_string()));
                },
            )
            .when_some(ref_name.clone(), |menu, ref_name| {
                menu.entry("Copy Ref Name", None, move |_window, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(ref_name.to_string()));
                })
            })
            .when(ref_name.is_none(), |menu| {
                menu.map(|menu| {
                    let tag_names = commit.tag_names.clone();
                    let copy_tag_label = "Copy Tag";

                    match tag_names.as_slice() {
                        [] => menu.item(
                            ContextMenuEntry::new(copy_tag_label)
                                .action(CopyCommitTag.boxed_clone())
                                .disabled(true),
                        ),
                        [tag_name] => {
                            let tag_name = tag_name.clone();
                            let label = format!("{copy_tag_label}: {tag_name}");
                            menu.entry(
                                label,
                                Some(CopyCommitTag.boxed_clone()),
                                move |_window, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        tag_name.to_string(),
                                    ));
                                },
                            )
                        }
                        _ => menu.submenu(copy_tag_label, move |menu, _window, _cx| {
                            let mut menu = menu.fixed_width(COMMIT_TAG_LIST_WIDTH_IN_REMS.into());

                            for tag_name in tag_names.clone() {
                                let tag_name_to_copy = tag_name.clone();
                                menu = menu.entry(tag_name, None, move |_window, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        tag_name_to_copy.to_string(),
                                    ));
                                });
                            }
                            menu
                        }),
                    }
                })
            })
            .map(|menu| {
                git_actions_menu(
                    menu,
                    sha,
                    &selected,
                    ref_name.clone(),
                    repository.clone(),
                    workspace.clone(),
                    solo.clone(),
                )
            })
            .when(source == CommitContextMenuSource::GitPanel, |menu| {
                menu.entry("Show in Git Graph", None, move |window, cx| {
                    window.dispatch_action(
                        Box::new(crate::git_graph::OpenAtCommit {
                            sha: sha.to_string(),
                        }),
                        cx,
                    );
                })
            })
            .map(|mut menu| {
                menu = menu.separator().header("Custom Commands");

                if git_tasks.is_empty() {
                    return menu.item(
                        ContextMenuEntry::new("Learn More")
                            .icon(IconName::ArrowUpRight)
                            .icon_color(Color::Muted)
                            .icon_position(IconPosition::End)
                            .handler(|_window, cx| {
                                let docs_url =
                                    release_channel::docs_url(CUSTOM_GIT_COMMANDS_DOCS_SLUG, cx);
                                cx.open_url(&docs_url);
                            }),
                    );
                }

                for (task_source_kind, resolved_task) in git_tasks {
                    let label = resolved_task.display_label().to_string();
                    let workspace = workspace.clone();
                    menu = menu.entry(label, None, move |window, cx| {
                        workspace
                            .update(cx, |workspace, cx| {
                                workspace.schedule_resolved_task(
                                    task_source_kind.clone(),
                                    resolved_task.clone(),
                                    false,
                                    window,
                                    cx,
                                );
                            })
                            .ok();
                    });
                }

                menu
            })
    })
}

fn git_task_context(
    repository: &Option<WeakEntity<Repository>>,
    commit_sha: git::Oid,
    ref_name: Option<&str>,
    cx: &App,
) -> Option<TaskContext> {
    let repository_path = repository
        .as_ref()?
        .upgrade()?
        .read(cx)
        .work_directory_abs_path
        .to_path_buf();
    let repository_name = repository_path
        .file_name()
        .and_then(|name| name.to_str())
        .map(ToString::to_string);
    let mut task_variables = TaskVariables::from_iter([
        (VariableName::GitSha, commit_sha.to_string()),
        (VariableName::GitShaShort, commit_sha.display_short()),
        (
            VariableName::GitRepositoryPath,
            repository_path.to_string_lossy().into_owned(),
        ),
    ]);

    if let Some(repository_name) = repository_name {
        task_variables.insert(VariableName::GitRepositoryName, repository_name);
    }
    if let Some(ref_name) = ref_name {
        task_variables.insert(VariableName::GitRef, ref_name.to_string());
    }

    Some(TaskContext {
        cwd: Some(repository_path),
        task_variables,
        ..TaskContext::default()
    })
}

fn git_context_menu_tasks(
    task_context: Option<TaskContext>,
    workspace: &WeakEntity<Workspace>,
    cx: &App,
) -> Vec<(project::TaskSourceKind, task::ResolvedTask)> {
    let Some(task_context) = task_context else {
        return Vec::new();
    };
    let Some(workspace) = workspace.upgrade() else {
        return Vec::new();
    };
    let project = workspace.read(cx).project().clone();
    let task_inventory = project.read_with(cx, |project, cx| {
        project.task_store().read(cx).task_inventory().cloned()
    });
    let Some(task_inventory) = task_inventory else {
        return Vec::new();
    };

    task_inventory
        .read(cx)
        .resolve_global_tasks_with_tag(GIT_COMMAND_TASK_TAG, &task_context)
}

/// The commands a history offers over a commit or over the ref on it.
///
/// Everything here goes through the same `Repository` calls the panel uses, so
/// an error reads the same wherever it came from.
#[allow(clippy::too_many_arguments)]
fn git_actions_menu(
    menu: ContextMenu,
    sha: Oid,
    selected: &[Oid],
    ref_name: Option<SharedString>,
    repository: Option<WeakEntity<Repository>>,
    workspace: WeakEntity<Workspace>,
    solo: Option<Rc<dyn Fn(SharedString, &mut App)>>,
) -> ContextMenu {
    let Some(repository) = repository else {
        return menu;
    };

    // A menu opened over a selection of commits acts on all of them; over one,
    // on the one.
    let commits: Vec<String> = match selected.len() > 1 {
        true => selected.iter().map(|sha| sha.to_string()).collect(),
        false => vec![sha.to_string()],
    };
    let many = commits.len() > 1;
    let of_them = match many {
        true => format!(" ({} commits)", commits.len()),
        false => String::new(),
    };

    let menu = match ref_name {
        None => menu,
        Some(name) => {
            let name_for_solo = name.clone();
            let checkout = (name.clone(), repository.clone());
            let branch_from = (name.clone(), repository.clone());
            let rename = (name.clone(), repository.clone());
            let delete = (name, repository.clone());
            let push = repository.clone();
            let pull = repository.clone();
            let workspace_for_branch = workspace.clone();
            let workspace_for_rename = workspace.clone();

            menu.separator()
                .header("Branch")
                .entry("Check Out", None, move |_window, cx| {
                    let (name, repository) = checkout.clone();
                    run_on_repository(&repository, cx, move |repository| {
                        repository.change_branch(name.to_string())
                    });
                })
                .entry("New Branch From Here…", None, move |window, cx| {
                    let (from, repository) = branch_from.clone();
                    ask_for_a_name(
                        &workspace_for_branch,
                        "New branch",
                        "Branch name",
                        "",
                        window,
                        cx,
                        move |name, _window, cx| {
                            let repository = repository.clone();
                            let from = from.clone();
                            run_on_repository(&repository, cx, move |repository| {
                                repository.create_branch(name.to_string(), Some(from.to_string()))
                            });
                        },
                    );
                })
                .entry("Rename…", None, move |window, cx| {
                    let (name, repository) = rename.clone();
                    ask_for_a_name(
                        &workspace_for_rename,
                        "Rename branch",
                        "New name",
                        name.clone(),
                        window,
                        cx,
                        move |new_name, _window, cx| {
                            let repository = repository.clone();
                            let name = name.clone();
                            run_on_repository(&repository, cx, move |repository| {
                                repository.rename_branch(name.to_string(), new_name.to_string())
                            });
                        },
                    );
                })
                .entry("Delete", None, move |_window, cx| {
                    let (name, repository) = delete.clone();
                    run_on_repository(&repository, cx, move |repository| {
                        // Local, and not forced: a branch that has not been
                        // merged is worth refusing rather than losing.
                        repository.delete_branch(false, name.to_string(), false)
                    });
                })
                .when_some(solo, |menu, solo| {
                    let name = name_for_solo.clone();
                    menu.entry("Solo", None, move |_window, cx| {
                        solo(name.clone(), cx);
                    })
                })
                .entry("Push", None, move |window, cx| {
                    dispatch_git(&push, window, cx, git::Push.boxed_clone());
                })
                .entry("Pull", None, move |window, cx| {
                    dispatch_git(&pull, window, cx, git::Pull.boxed_clone());
                })
        }
    };

    let tag_at = repository.clone();
    let branch_at = repository.clone();
    let cherry_pick = (commits.clone(), repository.clone());
    let revert = (commits, repository.clone());
    let workspace_for_tag = workspace.clone();
    let workspace_for_commit_branch = workspace;

    let menu = menu
        .separator()
        .header("Commit")
        .entry("New Branch Here…", None, move |window, cx| {
            let repository = branch_at.clone();
            let at = sha.to_string();
            ask_for_a_name(
                &workspace_for_commit_branch,
                "New branch",
                "Branch name",
                "",
                window,
                cx,
                move |name, _window, cx| {
                    let repository = repository.clone();
                    let at = at.clone();
                    run_on_repository(&repository, cx, move |repository| {
                        repository.create_branch(name.to_string(), Some(at))
                    });
                },
            );
        })
        .entry("New Tag Here…", None, move |window, cx| {
            let repository = tag_at.clone();
            let at = sha.to_string();
            ask_for_a_name(
                &workspace_for_tag,
                "New tag",
                "Tag name",
                "",
                window,
                cx,
                move |name, _window, cx| {
                    let repository = repository.clone();
                    let at = at.clone();
                    run_on_repository(&repository, cx, move |repository| {
                        repository.create_tag(name.to_string(), at)
                    });
                },
            );
        })
        .entry(format!("Cherry-Pick{of_them}"), None, move |_window, cx| {
            let (commits, repository) = cherry_pick.clone();
            run_on_repository(&repository, cx, move |repository| {
                repository.cherry_pick(commits)
            });
        })
        .entry(format!("Revert{of_them}"), None, move |_window, cx| {
            let (commits, repository) = revert.clone();
            run_on_repository(&repository, cx, move |repository| {
                repository.revert_commits(commits)
            });
        });

    // A reset moves the branch, and a hard one throws the working tree away
    // with it, so that one asks first.
    let resets = [
        ("Reset Here (Soft)", ResetMode::Soft, false),
        ("Reset Here (Mixed)", ResetMode::Mixed, false),
        ("Reset Here (Hard)", ResetMode::Hard, true),
    ];
    let mut menu = menu;
    for (label, mode, ask_first) in resets {
        let repository = repository.clone();
        let at = sha.to_string();
        menu = menu.entry(label, None, move |window, cx| {
            let repository = repository.clone();
            let at = at.clone();
            if !ask_first {
                reset_to(&repository, at, mode, cx);
                return;
            }

            let answer = window.prompt(
                gpui::PromptLevel::Warning,
                "Reset this branch, discarding everything not committed?",
                Some("A hard reset cannot be undone."),
                &["Reset", "Cancel"],
                cx,
            );
            cx.spawn(async move |cx| {
                if answer.await.ok() == Some(0) {
                    cx.update(|cx| reset_to(&repository, at, mode, cx));
                }
            })
            .detach();
        });
    }

    let _ = many;
    menu
}

/// Moves the branch to a commit.
fn reset_to(repository: &WeakEntity<Repository>, at: String, mode: ResetMode, cx: &mut App) {
    let Some(repository) = repository.upgrade() else {
        return;
    };
    let answer = repository.update(cx, |repository, cx| repository.reset(at, mode, cx));
    cx.spawn(async move |_| {
        if let Ok(Err(error)) = answer.await {
            log::error!("git reset failed: {error:#}");
        }
    })
    .detach();
}

/// Runs one repository command and reports what it says, wherever it failed.
fn run_on_repository<F, R>(repository: &WeakEntity<Repository>, cx: &mut App, command: F)
where
    F: FnOnce(&mut Repository) -> futures::channel::oneshot::Receiver<anyhow::Result<R>> + 'static,
    R: 'static,
{
    let Some(repository) = repository.upgrade() else {
        return;
    };
    let answer = repository.update(cx, |repository, _| command(repository));
    cx.spawn(async move |_| {
        if let Ok(Err(error)) = answer.await {
            log::error!("git command failed: {error:#}");
        }
    })
    .detach();
}

/// Sends a workspace-level git action, for the commands the panel already owns.
fn dispatch_git(
    repository: &WeakEntity<Repository>,
    window: &mut Window,
    cx: &mut App,
    action: Box<dyn Action>,
) {
    let _ = repository;
    window.dispatch_action(action, cx);
}

/// Opens the one-field prompt and runs `then` with what was typed.
fn ask_for_a_name(
    workspace: &WeakEntity<Workspace>,
    title: &'static str,
    field: &'static str,
    starting_with: impl Into<SharedString>,
    window: &mut Window,
    cx: &mut App,
    then: impl Fn(SharedString, &mut Window, &mut App) + 'static,
) {
    let starting_with = starting_with.into();
    workspace
        .update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                crate::name_prompt::NamePrompt::new(
                    title,
                    field,
                    starting_with.clone(),
                    then,
                    window,
                    cx,
                )
            });
        })
        .ok();
}
