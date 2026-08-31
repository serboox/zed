use std::sync::Arc;

use api_client::{EnvironmentId, RequestId};
use gpui::{AnyElement, App, Context, DismissEvent, Entity, Task, WeakEntity, Window, div};
use picker::{Picker, PickerDelegate};
use ui::{
    ElevationIndex, Icon, IconButton, IconName, IconSize, Label, LabelSize, ListItem,
    ListItemSpacing, SharedString, Tooltip, cyberpunk, prelude::*,
};
use workspace::Workspace;

use crate::store::ApiClientStore;

/// What the row that follows the active environment is called, in the list and
/// to a search.
const THE_ACTIVE_ONE: &str = "Use Active Environment";

/// What the row that asks for no comparison is called.
const NO_COMPARISON: &str = "Do Not Compare";

/// What choosing a row in this picker means. The list itself is the same either
/// way -- the same environments, the same pins, the same search -- because there
/// is only one list of environments and one set of pins per request.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum WhatThePickerIsFor {
    /// Where the request is sent.
    WhereItGoes,
    /// Which environment the next send is compared against. Choosing sends
    /// nothing: the comparison waits for Send, like every other send.
    WhatToCompareWith,
}

impl WhatThePickerIsFor {
    /// What the row that chooses nothing is called.
    fn the_row_that_chooses_nothing(self) -> &'static str {
        match self {
            Self::WhereItGoes => THE_ACTIVE_ONE,
            Self::WhatToCompareWith => NO_COMPARISON,
        }
    }
}

/// What the reader sees in the request's environment picker. Choosing an
/// environment and pinning one are two different things, so a row carries both:
/// the row itself sends the request somewhere, and the pin on it decides whether
/// the row stays at the top of the list.
enum Row {
    /// Names the group under it. Not something to land on.
    Header(SharedString),
    /// Back to whichever environment is active store-wide, or to no comparison
    /// at all, depending on what the picker is for.
    ChoosesNothing,
    Environment {
        id: EnvironmentId,
        name: SharedString,
        pinned: bool,
    },
}

pub struct EnvironmentPickerDelegate {
    store: Entity<ApiClientStore>,
    workspace: WeakEntity<Workspace>,
    request_id: RequestId,
    what_for: WhatThePickerIsFor,
    rows: Vec<Row>,
    selected_index: usize,
}

impl EnvironmentPickerDelegate {
    pub fn new(
        store: Entity<ApiClientStore>,
        workspace: WeakEntity<Workspace>,
        request_id: RequestId,
        what_for: WhatThePickerIsFor,
    ) -> Self {
        Self {
            store,
            workspace,
            request_id,
            what_for,
            rows: Vec::new(),
            selected_index: 0,
        }
    }

    /// The environment this picker's tick is on: where the request is sent, or
    /// what it is compared against. Only one that still exists.
    fn chosen(&self, cx: &App) -> Option<EnvironmentId> {
        let store = self.store.read(cx);
        store
            .requests
            .iter()
            .find(|request| request.id == self.request_id)
            .and_then(|request| match self.what_for {
                WhatThePickerIsFor::WhereItGoes => request.chosen_environment(),
                WhatThePickerIsFor::WhatToCompareWith => request.compared_with(),
            })
            .filter(|id| store.environment_by_id(*id).is_some())
    }

    /// Builds the list: the pinned environments under a heading, a line, then
    /// the active-environment row and everything else. Both groups read
    /// alphabetically and blind to case, so a name that starts with a capital
    /// sits among its neighbours rather than above every lower-case one.
    fn rebuild(&mut self, query: &str, cx: &mut App) {
        let query = query.trim().to_lowercase();
        let store = self.store.read(cx);
        let pinned_ids = store
            .requests
            .iter()
            .find(|request| request.id == self.request_id)
            .map(|request| request.pinned_environments())
            .unwrap_or_default();

        let mut pinned: Vec<(EnvironmentId, SharedString)> = Vec::new();
        let mut the_rest: Vec<(EnvironmentId, SharedString)> = Vec::new();
        for environment in &store.environments {
            if !query.is_empty() && !environment.name.to_lowercase().contains(&query) {
                continue;
            }
            let row = (environment.id, SharedString::from(environment.name.clone()));
            match pinned_ids.contains(&environment.id) {
                true => pinned.push(row),
                false => the_rest.push(row),
            }
        }
        let by_name = |rows: &mut Vec<(EnvironmentId, SharedString)>| {
            rows.sort_by(|(_, one), (_, other)| {
                one.to_lowercase()
                    .cmp(&other.to_lowercase())
                    .then_with(|| one.cmp(other))
            });
        };
        by_name(&mut pinned);
        by_name(&mut the_rest);

        let mut rows = Vec::with_capacity(pinned.len() + the_rest.len() + 2);
        if !pinned.is_empty() {
            rows.push(Row::Header("Pinned to this request".into()));
            for (id, name) in pinned {
                rows.push(Row::Environment {
                    id,
                    name,
                    pinned: true,
                });
            }
        }
        // Searchable like any other row rather than hidden by a search: it is
        // the way back to choosing nothing at all, and a reader with something
        // typed must not be locked out of it.
        let chooses_nothing = self.what_for.the_row_that_chooses_nothing();
        if query.is_empty() || chooses_nothing.to_lowercase().contains(&query) {
            rows.push(Row::ChoosesNothing);
        }
        for (id, name) in the_rest {
            rows.push(Row::Environment {
                id,
                name,
                pinned: false,
            });
        }
        self.rows = rows;
        self.selected_index = self.where_the_choice_is(cx);
    }

    fn where_the_choice_is(&self, cx: &App) -> usize {
        let chosen = self.chosen(cx);
        self.rows
            .iter()
            .position(|row| match (row, chosen) {
                (Row::Environment { id, .. }, Some(chosen)) => *id == chosen,
                (Row::ChoosesNothing, None) => true,
                _ => false,
            })
            .unwrap_or_else(|| {
                self.rows
                    .iter()
                    .position(|row| !matches!(row, Row::Header(_)))
                    .unwrap_or(0)
            })
    }

    /// Pins or unpins one row without sending anything anywhere, and puts the
    /// list back together so the row moves to its group straight away.
    fn toggle_pin(&mut self, id: EnvironmentId, query: &str, cx: &mut App) {
        let request_id = self.request_id;
        self.store.update(cx, |store, cx| {
            store.toggle_request_pinned_environment(request_id, id, cx)
        });
        self.rebuild(query, cx);
    }
}

#[cfg(any(test, feature = "test-support"))]
impl EnvironmentPickerDelegate {
    /// What the list holds, in the order it is painted, for a test that has to
    /// say which row a line falls under -- the line itself takes up no space.
    pub fn rows_for_test(&self, _cx: &App) -> Vec<SharedString> {
        self.rows
            .iter()
            .map(|row| match row {
                Row::Header(title) => title.clone(),
                Row::ChoosesNothing => self.what_for.the_row_that_chooses_nothing().into(),
                Row::Environment { name, .. } => name.clone(),
            })
            .collect()
    }
}

impl PickerDelegate for EnvironmentPickerDelegate {
    type ListItem = AnyElement;

    fn name() -> &'static str {
        "api client environments"
    }

    fn match_count(&self) -> usize {
        self.rows.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, ix: usize, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.selected_index = ix.min(self.rows.len().saturating_sub(1));
        cx.notify();
    }

    fn can_select(&self, ix: usize, _window: &mut Window, _cx: &mut Context<Picker<Self>>) -> bool {
        !matches!(self.rows.get(ix), Some(Row::Header(_)) | None)
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Search".into()
    }

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        self.rebuild(&query, cx);
        cx.notify();
        Task::ready(())
    }

    fn confirm(&mut self, _secondary: bool, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let chosen = match self.rows.get(self.selected_index) {
            Some(Row::Environment { id, .. }) => Some(*id),
            Some(Row::ChoosesNothing) => None,
            Some(Row::Header(_)) | None => return,
        };
        let request_id = self.request_id;
        let what_for = self.what_for;
        self.store.update(cx, |store, cx| match what_for {
            WhatThePickerIsFor::WhereItGoes => {
                store.choose_request_environment(request_id, chosen, cx)
            }
            // Nothing is sent here. The comparison is asked for, and Send is
            // what carries it out.
            WhatThePickerIsFor::WhatToCompareWith => {
                store.set_request_comparison_environment(request_id, chosen, cx)
            }
        });
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.defer_in(window, |picker, window, cx| {
            picker.set_query("", window, cx);
        });
    }

    /// A new environment is made where every other one is made, so what the
    /// reader learns here carries over.
    fn searchbar_trailer(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<AnyElement> {
        let store = self.store.clone();
        let workspace = self.workspace.clone();
        Some(
            div()
                .pr_1()
                .debug_selector(|| "environment-picker-new".to_string())
                .child(
                    IconButton::new("environment-picker-new", IconName::Plus)
                        .style(cyberpunk::Rank::Quiet.style())
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text("New Environment"))
                        .on_click(move |_, window, cx| {
                            let store = store.clone();
                            workspace
                                .update(cx, |workspace, cx| {
                                    workspace.toggle_modal(window, cx, |window, cx| {
                                        crate::environment_editor::EnvironmentEditorModal::new_for_environments(
                                            store, window, cx,
                                        )
                                    });
                                })
                                .ok();
                        }),
                )
                .into_any_element(),
        )
    }

    fn separators_after_indices(&self) -> Vec<usize> {
        // Under the last pinned row, so the pinned ones read as a group of their
        // own rather than as the top of one long list -- and only when there is
        // a row under it, or the line hangs off the bottom of the list.
        let last_pinned = self
            .rows
            .iter()
            .rposition(|row| matches!(row, Row::Environment { pinned: true, .. }));
        match last_pinned {
            Some(last_pinned) if last_pinned + 1 < self.rows.len() => vec![last_pinned],
            _ => Vec::new(),
        }
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let chosen = self.chosen(cx);
        match self.rows.get(ix)? {
            Row::Header(title) => Some(
                div()
                    .px_2()
                    .pt_1()
                    .pb_0p5()
                    .debug_selector(|| "environment-picker-pinned-header".to_string())
                    .child(
                        Label::new(title.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .into_any_element(),
            ),
            Row::ChoosesNothing => Some(
                div()
                    .debug_selector(|| "environment-row:nothing".to_string())
                    .child(
                        ListItem::new(("environment-row", ix))
                            .inset(true)
                            .spacing(ListItemSpacing::Sparse)
                            .toggle_state(selected)
                            .start_slot(the_mark(chosen.is_none()))
                            .child(
                                Label::new(self.what_for.the_row_that_chooses_nothing()).truncate(),
                            ),
                    )
                    .into_any_element(),
            ),
            Row::Environment { id, name, pinned } => {
                let id = *id;
                let pinned = *pinned;
                let (icon, tooltip) = match pinned {
                    true => (IconName::Unpin, "Unpin from this request"),
                    false => (IconName::Pin, "Pin to this request"),
                };
                Some(
                    div()
                        .debug_selector({
                            let name = name.clone();
                            move || format!("environment-row:{name}")
                        })
                        .child(
                            ListItem::new(("environment-row", ix))
                                .inset(true)
                                .spacing(ListItemSpacing::Sparse)
                                .toggle_state(selected)
                                .start_slot(the_mark(chosen == Some(id)))
                                .child(Label::new(name.clone()).truncate())
                                // The click on the row itself is the list's own:
                                // it is what tells the picker which row was hit,
                                // and a second handler here would send the
                                // request twice.
                                .end_slot_on_hover(
                                    div()
                                        .pr_1()
                                        .debug_selector({
                                            let name = name.clone();
                                            move || format!("environment-pin:{name}")
                                        })
                                        .child(
                                            IconButton::new(("environment-pin", ix), icon)
                                                .layer(ElevationIndex::ElevatedSurface)
                                                .icon_size(IconSize::Small)
                                                .tooltip(Tooltip::text(tooltip))
                                                .on_click(cx.listener(
                                                    move |picker, _, window, cx| {
                                                        // The pin is its own
                                                        // action: the click must
                                                        // not reach the row and
                                                        // send the request
                                                        // somewhere.
                                                        cx.stop_propagation();
                                                        let query = picker.query(cx);
                                                        picker.delegate.toggle_pin(id, &query, cx);
                                                        window.refresh();
                                                        cx.notify();
                                                    },
                                                )),
                                        ),
                                ),
                        )
                        .into_any_element(),
                )
            }
        }
    }
}

/// The tick that says where this request is sent. Always the same size, taken up
/// or not, so every name in the list starts at the same place.
fn the_mark(marked: bool) -> AnyElement {
    match marked {
        true => Icon::new(IconName::Check)
            .size(IconSize::Small)
            .color(Color::Accent)
            .into_any_element(),
        false => div().size(ui::rems_from_px(14.)).into_any_element(),
    }
}

/// The picker a chip opens: the same list of environments and the same pins
/// whichever chip asked for it.
pub fn environment_picker(
    store: Entity<ApiClientStore>,
    workspace: WeakEntity<Workspace>,
    request_id: RequestId,
    what_for: WhatThePickerIsFor,
    window: &mut Window,
    cx: &mut App,
) -> Entity<Picker<EnvironmentPickerDelegate>> {
    cx.new(|cx| {
        let mut delegate = EnvironmentPickerDelegate::new(store, workspace, request_id, what_for);
        delegate.rebuild("", cx);
        Picker::list(delegate, window, cx).max_height(ui::rems(20.))
    })
}
