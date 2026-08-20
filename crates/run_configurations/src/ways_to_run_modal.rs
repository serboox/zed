use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render, SharedString,
    WeakEntity, Window, px,
};
use task::{DebugScenario, TaskTemplate};
use ui::{ElevationIndex, prelude::*};
use workspace::{ModalView, Workspace};
use zed_actions::run_configurations::EntryPointOffer;

use crate::configurations_file::Kind;
use crate::configurations_store::ConfigurationsStore;

/// One way of running what the reader asked about.
#[derive(Clone)]
pub enum Way {
    /// A configuration the project keeps in one of its files.
    Kept {
        label: SharedString,
        detail: SharedString,
        kind: Kind,
        at: usize,
    },
    /// What the editor itself would run for this line. It is not in any file yet,
    /// which is what makes it worth offering to keep.
    OnTheSpot {
        label: SharedString,
        detail: SharedString,
        task: TaskTemplate,
    },
    /// A way that was run on the spot before and is still remembered. It is not in
    /// any file either, so it carries the way to pin it into one -- named rather
    /// than numbered, since the list of them shifts as things are run and pinned.
    Remembered {
        label: SharedString,
        detail: SharedString,
        task: TaskTemplate,
    },
}

impl Way {
    fn label(&self) -> SharedString {
        match self {
            Way::Kept { label, .. }
            | Way::OnTheSpot { label, .. }
            | Way::Remembered { label, .. } => label.clone(),
        }
    }

    fn detail(&self) -> SharedString {
        match self {
            Way::Kept { detail, .. }
            | Way::OnTheSpot { detail, .. }
            | Way::Remembered { detail, .. } => detail.clone(),
        }
    }

    /// Whether this way is written down anywhere. The ones that are not are the
    /// ones worth offering to keep.
    fn is_in_a_file(&self) -> bool {
        matches!(self, Way::Kept { .. })
    }
}

/// The window the gutter's run button opens: every way of running this project,
/// the one the editor found for this very line first, and the way to write a new
/// one or go and edit them all.
pub struct WaysToRunModal {
    focus: FocusHandle,
    store: Entity<ConfigurationsStore>,
    workspace: WeakEntity<Workspace>,
    offer: EntryPointOffer,
    ways: Vec<Way>,
    chosen: usize,
    trouble: Option<SharedString>,
}

impl EventEmitter<DismissEvent> for WaysToRunModal {}
impl ModalView for WaysToRunModal {}

impl Focusable for WaysToRunModal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl WaysToRunModal {
    /// Every way of running that this project knows, the line's own first.
    pub fn ways_of(
        offer: &EntryPointOffer,
        store: &Entity<ConfigurationsStore>,
        cx: &App,
    ) -> Vec<Way> {
        let mut ways = Vec::new();
        let on_the_spot = crate::new_configuration_modal::task_from(offer);
        if !on_the_spot.command.trim().is_empty() {
            let task = on_the_spot;
            ways.push(Way::OnTheSpot {
                label: SharedString::from(task.label.clone()),
                detail: SharedString::from(match offer.file.as_deref() {
                    Some(file) => format!(
                        "on the spot -- {} line {}",
                        file.file_name()
                            .map(|name| name.to_string_lossy().to_string())
                            .unwrap_or_else(|| file.display().to_string()),
                        offer.line
                    ),
                    None => "on the spot".to_string(),
                }),
                task,
            });
        }
        let store = store.read(cx);
        for task in store.temporary() {
            let already_offered = ways.iter().any(|way| match way {
                Way::OnTheSpot { task: theirs, .. } => {
                    theirs.label == task.label && theirs.command == task.command
                }
                _ => false,
            });
            if already_offered {
                continue;
            }
            ways.push(Way::Remembered {
                label: SharedString::from(task.label.clone()),
                detail: SharedString::from(
                    format!("{} {}", task.command, task.args.join(" "))
                        .trim()
                        .to_string(),
                ),
                task: task.clone(),
            });
        }
        for kind in [Kind::Task, Kind::Debug] {
            for (at, configuration) in store.of_kind(kind).configurations.iter().enumerate() {
                let detail = match (&configuration.task, &configuration.scenario) {
                    (Some(task), _) => format!("{} {}", task.command, task.args.join(" ")),
                    (_, Some(scenario)) => format!("debug with {}", scenario.adapter),
                    _ => "as the file has it".to_string(),
                };
                ways.push(Way::Kept {
                    label: SharedString::from(configuration.shown_label()),
                    detail: SharedString::from(detail.trim().to_string()),
                    kind,
                    at,
                });
            }
        }
        ways
    }

    pub fn new(
        offer: EntryPointOffer,
        store: Entity<ConfigurationsStore>,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        let ways = Self::ways_of(&offer, &store, cx);
        Self {
            focus: cx.focus_handle(),
            store,
            workspace,
            offer,
            ways,
            chosen: 0,
            trouble: None,
        }
    }

    /// What the window lists, and which of them is not in a file yet. For tests and
    /// for the switcher, which shows the same set.
    pub fn shown_ways(&self) -> Vec<(String, bool)> {
        self.ways
            .iter()
            .map(|way| (way.label().to_string(), !way.is_in_a_file()))
            .collect()
    }

    fn move_by(&mut self, by: isize, cx: &mut Context<Self>) {
        if self.ways.is_empty() {
            return;
        }
        let last = self.ways.len() - 1;
        self.chosen = match by {
            by if by < 0 => match self.chosen {
                0 => last,
                at => at - 1,
            },
            _ => match self.chosen == last {
                true => 0,
                false => self.chosen + 1,
            },
        };
        cx.notify();
    }

    fn run(&mut self, at: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(way) = self.ways.get(at).cloned() else {
            return;
        };
        match way {
            Way::OnTheSpot { task, .. } => {
                // Run without being written down: remembered for the next time, and
                // offered for pinning rather than asked about now.
                self.store.update(cx, |store, cx| {
                    store.remember_temporary(task.clone(), cx);
                });
                self.run_a_task(task, window, cx)
            }
            Way::Remembered { task, .. } => {
                self.store.update(cx, |store, cx| {
                    store.remember_temporary(task.clone(), cx);
                });
                self.run_a_task(task, window, cx)
            }
            Way::Kept { kind, at, .. } => {
                let configuration = self.store.read(cx).get(kind, at).cloned();
                let Some(configuration) = configuration else {
                    self.trouble = Some("It is no longer in the file.".into());
                    cx.notify();
                    return;
                };
                match (configuration.task, configuration.scenario) {
                    (Some(task), _) => self.run_a_task(task, window, cx),
                    (_, Some(scenario)) => self.start_debugging(scenario, window, cx),
                    _ => {
                        self.trouble =
                            Some("The file holds something this editor cannot run.".into());
                        cx.notify();
                    }
                }
            }
        }
    }

    fn run_a_task(&mut self, task: TaskTemplate, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |modal, cx| {
            crate::configurations_view::run_a_task(&workspace, task, cx).await;
            modal.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
        })
        .detach();
    }

    fn start_debugging(
        &mut self,
        scenario: DebugScenario,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |modal, cx| {
            crate::configurations_view::start_a_debug_session(&workspace, scenario, cx).await;
            modal.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
        })
        .detach();
    }

    /// Writes a remembered way into the project's file, where everybody else reads
    /// it. The list is rebuilt, since what was temporary is now kept.
    fn pin(&mut self, at: usize, cx: &mut Context<Self>) {
        let Some(Way::Remembered { task, .. }) = self.ways.get(at).cloned() else {
            return;
        };
        // Looked up by what it is, not by where it was: the list shifts as things
        // are run and pinned, and pinning the wrong one writes somebody else's
        // command into the project's file.
        let Some(which) = self
            .store
            .read(cx)
            .temporary()
            .iter()
            .position(|kept| kept.label == task.label && kept.command == task.command)
        else {
            self.trouble = Some("That way is no longer remembered.".into());
            cx.notify();
            return;
        };
        let writing = self
            .store
            .update(cx, |store, cx| store.pin_temporary(which, cx));
        let store = self.store.clone();
        let offer = self.offer.clone();
        cx.spawn(async move |modal, cx| match writing.await {
            Ok(()) => {
                modal
                    .update(cx, |modal, cx| {
                        modal.ways = Self::ways_of(&offer, &store, cx);
                        modal.chosen = 0;
                        cx.notify();
                    })
                    .ok();
            }
            Err(error) => {
                modal
                    .update(cx, |modal, cx| {
                        modal.trouble = Some(format!("{error:#}").into());
                        cx.notify();
                    })
                    .ok();
            }
        })
        .detach();
    }

    /// Opens the window that writes a new configuration, filled in from whatever
    /// the editor found on the line.
    fn write_a_new_one(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let offer = self.offer.clone();
        let store = self.store.clone();
        let workspace = self.workspace.clone();
        cx.emit(DismissEvent);
        let Some(alive) = workspace.upgrade() else {
            return;
        };
        alive.update(cx, |workspace, cx| {
            let handle = workspace.weak_handle();
            workspace.toggle_modal(window, cx, move |window, cx| {
                crate::new_configuration_modal::NewConfigurationModal::new(
                    offer, store, handle, window, cx,
                )
            });
        });
    }

    fn open_them_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
        window.dispatch_action(
            Box::new(zed_actions::run_configurations::OpenRunConfigurations),
            cx,
        );
    }
}

impl Render for WaysToRunModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let chosen = self.chosen;
        let mut list = v_flex().w_full().gap_0p5();
        for (at, way) in self.ways.iter().enumerate() {
            let label = way.label();
            let detail = way.detail();
            let temporary = !way.is_in_a_file();
            let pinnable = matches!(way, Way::Remembered { .. });
            list = list.child(
                h_flex()
                    .id(SharedString::from(format!("way-{at}")))
                    .debug_selector(move || format!("way-{at}"))
                    .w_full()
                    .px_2()
                    .py_1()
                    .gap_2()
                    .items_center()
                    .justify_between()
                    .cursor_pointer()
                    .when(at == chosen, |row| {
                        row.bg(cx.theme().colors().element_selected)
                    })
                    .hover(|row| row.bg(cx.theme().colors().element_hover))
                    .on_click(cx.listener(move |modal, _, window, cx| modal.run(at, window, cx)))
                    .child(
                        v_flex()
                            .gap_0p5()
                            .child(Label::new(label).size(LabelSize::Small))
                            .child(
                                Label::new(detail)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    )
                    .when(temporary, |row| {
                        row.child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(
                                    Label::new("on the spot")
                                        .size(LabelSize::XSmall)
                                        .color(Color::Accent),
                                )
                                .when(pinnable, |row| {
                                    row.child(
                                        Button::new(("pin-way", at), "Keep")
                                            .style(ButtonStyle::Subtle)
                                            .tooltip(ui::Tooltip::text(
                                                "Write it into the project's tasks.json",
                                            ))
                                            .on_click(cx.listener(move |modal, _, _window, cx| {
                                                modal.pin(at, cx)
                                            })),
                                    )
                                }),
                        )
                    }),
            );
        }

        v_flex()
            .key_context("WaysToRun")
            .track_focus(&self.focus)
            .debug_selector(|| "ways-to-run".to_string())
            .w(px(560.))
            .p_3()
            .gap_2()
            .elevation_3(cx)
            .shadow(ElevationIndex::ModalSurface.shadow(cx))
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .on_action(cx.listener(|modal, _: &menu::SelectNext, _, cx| modal.move_by(1, cx)))
            .on_action(cx.listener(|modal, _: &menu::SelectPrevious, _, cx| modal.move_by(-1, cx)))
            .on_action(cx.listener(|modal, _: &menu::Confirm, window, cx| {
                let at = modal.chosen;
                modal.run(at, window, cx)
            }))
            .child(
                Label::new("How to run this")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(list)
            .children(self.trouble.clone().map(|trouble| {
                Label::new(trouble)
                    .size(LabelSize::XSmall)
                    .color(Color::Error)
            }))
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("ways-new", "New configuration...")
                            .style(ButtonStyle::Subtle)
                            .on_click(cx.listener(|modal, _, window, cx| {
                                modal.write_a_new_one(window, cx)
                            })),
                    )
                    .child(
                        Button::new("ways-all", "All configurations")
                            .style(ButtonStyle::Subtle)
                            .on_click(
                                cx.listener(|modal, _, window, cx| modal.open_them_all(window, cx)),
                            ),
                    ),
            )
    }
}
