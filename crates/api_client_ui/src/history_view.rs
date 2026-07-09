use crate::store::{ApiClientStore, ApiClientStoreEvent};
use api_client::HistoryEntry;
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, Render, ScrollHandle, Subscription,
    WeakEntity, Window,
};
use ui::{
    Icon, IconName, IconSize, Label, LabelSize, ScrollAxes, Scrollbars, WithScrollbar, prelude::*,
};
use workspace::{Item, Workspace, item::ItemEvent};

pub struct HistoryView {
    focus_handle: FocusHandle,
    store: Entity<ApiClientStore>,
    workspace: WeakEntity<Workspace>,
    scroll_handle: ScrollHandle,
    _subscription: Subscription,
}

impl HistoryView {
    pub fn new(
        store: Entity<ApiClientStore>,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.subscribe(&store, |_, _, event: &ApiClientStoreEvent, cx| {
            if matches!(event, ApiClientStoreEvent::HistoryChanged) {
                cx.notify();
            }
        });
        Self {
            focus_handle: cx.focus_handle(),
            store,
            workspace,
            scroll_handle: ScrollHandle::new(),
            _subscription: subscription,
        }
    }

    fn reopen(&mut self, entry_index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(request_id) = self
            .store
            .read(cx)
            .history
            .get(entry_index)
            .and_then(|entry| entry.request_id)
        else {
            return;
        };
        let Some(request) = self
            .store
            .read(cx)
            .requests
            .iter()
            .find(|r| r.id == request_id)
            .cloned()
        else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let store = self.store.clone();
        let workspace_handle = self.workspace.clone();
        workspace.update(cx, |workspace, cx| {
            let view = cx.new(|cx| {
                crate::request_view::RequestView::new(&request, store, workspace_handle, window, cx)
            });
            workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
        });
    }

    fn clear(&mut self, cx: &mut Context<Self>) {
        self.store.update(cx, |store, cx| store.clear_history(cx));
    }
}

impl EventEmitter<()> for HistoryView {}

impl Focusable for HistoryView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for HistoryView {
    type Event = ();

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "History".into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::HistoryRerun))
    }

    fn to_item_events(_event: &Self::Event, _f: &mut dyn FnMut(ItemEvent)) {}
}

impl Render for HistoryView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let editor_background = cx.theme().colors().editor_background;
        let history = self.store.read(cx).history.clone();

        let mut list = v_flex()
            .id("api-client-history-list")
            .flex_1()
            .min_h_0()
            .overflow_scroll()
            .track_scroll(&self.scroll_handle)
            .gap_1();
        if history.is_empty() {
            list = list.child(
                Label::new("No requests sent yet.")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            );
        }
        for (index, entry) in history.iter().enumerate() {
            list = list.child(render_history_row(index, entry, cx));
        }
        let list = list.custom_scrollbars(
            Scrollbars::always_visible(ScrollAxes::Vertical)
                .tracked_scroll_handle(&self.scroll_handle),
            window,
            cx,
        );

        v_flex()
            .key_context("ApiClientHistoryView")
            .track_focus(&self.focus_handle)
            .size_full()
            .p_4()
            .gap_3()
            .bg(editor_background)
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .child(Label::new("History").size(LabelSize::Large))
                    .child(
                        div()
                            .id("history-clear-hitbox")
                            .debug_selector(|| "history-clear".to_string())
                            .child(
                                Button::new("history-clear", "Clear")
                                    .style(ButtonStyle::Subtle)
                                    .on_click(cx.listener(|this, _, _window, cx| this.clear(cx))),
                            ),
                    ),
            )
            .child(list)
    }
}

fn render_history_row(
    index: usize,
    entry: &HistoryEntry,
    cx: &mut Context<HistoryView>,
) -> impl IntoElement {
    let colors = cx.theme().colors();
    let status_label = match entry.status {
        Some(status) => status.to_string(),
        None => "Failed".to_string(),
    };
    let (status_icon, status_color) = match entry.status {
        Some(status) if (200..300).contains(&status) => (IconName::Check, Color::Success),
        Some(_) => (IconName::Warning, Color::Warning),
        None => (IconName::XCircle, Color::Error),
    };
    h_flex()
        .id(SharedString::from(format!("history-row-{index}")))
        .debug_selector(move || format!("history-row-{index}"))
        .w_full()
        .gap_3()
        .px_2()
        .py_1()
        .rounded_md()
        .cursor_pointer()
        .hover(|el| el.bg(colors.element_hover))
        .child(
            Label::new(entry.method.clone())
                .size(LabelSize::Small)
                .color(Color::Accent),
        )
        .child(
            h_flex()
                .gap_1()
                .child(
                    Icon::new(status_icon)
                        .size(IconSize::XSmall)
                        .color(status_color),
                )
                .child(
                    Label::new(status_label)
                        .size(LabelSize::Small)
                        .color(status_color),
                ),
        )
        .child(Label::new(entry.url.clone()).size(LabelSize::Small))
        .on_click(cx.listener(move |this, _, window, cx| this.reopen(index, window, cx)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};
    use project::Project;
    use uuid::Uuid;
    use workspace::Workspace;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
    }

    async fn build_history_view(
        cx: &mut TestAppContext,
    ) -> (
        Entity<ApiClientStore>,
        Entity<HistoryView>,
        VisualTestContext,
    ) {
        init_test(cx);
        let store = cx.new(|cx| ApiClientStore::new(cx));
        let fs = project::FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let workspace_window =
            cx.add_window(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let workspace_handle = workspace_window
            .read_with(cx, |workspace, _| workspace.weak_handle())
            .unwrap();

        let store_for_view = store.clone();
        let window =
            cx.add_window(|_window, cx| HistoryView::new(store_for_view, workspace_handle, cx));
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        let view = window.root(&mut cx).unwrap();
        (store, view, cx)
    }

    fn debug_center(cx: &mut VisualTestContext, selector: &'static str) -> gpui::Point<Pixels> {
        cx.debug_bounds(selector)
            .unwrap_or_else(|| panic!("expected debug bounds for {selector}"))
            .center()
    }

    fn draw(cx: &mut VisualTestContext) {
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn clicking_clear_empties_the_history_list(cx: &mut TestAppContext) {
        let (store, _view, mut cx) = build_history_view(cx).await;
        store.update(&mut cx, |store, cx| {
            store.record_history_entry(
                HistoryEntry::new(
                    Uuid::new_v4(),
                    "GET".into(),
                    "https://api.example.com".into(),
                    Some(200),
                    0,
                ),
                cx,
            );
        });
        draw(&mut cx);

        let clear_button = debug_center(&mut cx, "history-clear");
        cx.simulate_click(clear_button, gpui::Modifiers::none());
        cx.run_until_parked();

        store.read_with(&cx, |store, _| assert!(store.history.is_empty()));
    }

    #[gpui::test]
    async fn clicking_a_history_row_reopens_the_still_existing_request(cx: &mut TestAppContext) {
        let (store, view, mut cx) = build_history_view(cx).await;
        let collection_id =
            store.update(&mut cx, |store, cx| store.create_collection("A".into(), cx));
        let request_id = store.update(&mut cx, |store, cx| {
            store.create_request(collection_id, "Ping".into(), None, cx)
        });
        store.update(&mut cx, |store, cx| {
            store.record_history_entry(
                HistoryEntry::new(
                    request_id,
                    "GET".into(),
                    "https://api.example.com/ping".into(),
                    Some(200),
                    0,
                ),
                cx,
            );
        });
        draw(&mut cx);

        let row = debug_center(&mut cx, "history-row-0");
        cx.simulate_click(row, gpui::Modifiers::none());
        cx.run_until_parked();

        let workspace = view.read_with(&cx, |view, _| view.workspace.clone());
        let opened_a_request_tab = workspace
            .upgrade()
            .unwrap()
            .read_with(&cx, |workspace, cx| {
                workspace
                    .active_pane()
                    .read(cx)
                    .items()
                    .any(|item| item.tab_content_text(0, cx) == "Ping")
            });
        assert!(opened_a_request_tab);
    }
}
