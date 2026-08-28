use crate::request_view::RequestView;
use crate::response_view::{Pair, format_size, pretty_print_body, render_pairs, render_timing};
use crate::store::{
    ApiClientStore, ApiClientStoreEvent, HistoryExchangeDetail, HistoryExchangeOutcome,
};
use api_client::{HistoryEntry, RequestId};
use editor::Editor;
use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, Hsla, PromptLevel,
    Render, ScrollHandle, Subscription, WeakEntity, Window, px,
};
use ui::{
    Icon, IconName, IconSize, Label, LabelSize, ScrollAxes, Scrollbars, WithScrollbar, prelude::*,
};
use uuid::Uuid;
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

    /// Opens the detailed exchange report for a history row. Looked up by
    /// `entry.id`, not by the row's position in `history`: the position was
    /// captured when this row was last painted, and a send completing in the
    /// background between that paint and this click reorders `history` --
    /// resolving by position would then open whatever now sits at that
    /// position, not the entry the reader actually clicked.
    fn open_detail(&mut self, entry_id: Uuid, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self
            .store
            .read(cx)
            .history
            .iter()
            .find(|entry| entry.id == entry_id)
            .cloned()
        else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let store = self.store.clone();
        let workspace_handle = self.workspace.clone();
        let workspace_entity_id = workspace.entity_id();
        // `add_item_to_active_pane` below has the pane scan every existing
        // item -- including this `HistoryView`, when (as in the real app) it
        // is opened as a pane item rather than a standalone window -- to
        // check for a duplicate, reading each one via
        // `ItemHandle::buffer_kind`/`project_entry_ids`. Reading this same
        // entity while its own click handler still holds it leased panics.
        // `cx.defer_in` does not help: it re-wraps this same entity in a
        // fresh lease for the whole deferred callback, and the pane scan
        // would still collide with that. A plain `cx.defer` plus
        // `cx.with_window`, looked up by the *workspace's* own entity id
        // rather than this view's, reaches the target window without
        // leasing this entity at all.
        cx.defer(move |cx| {
            let opened = cx.with_window(workspace_entity_id, |window, cx| {
                workspace.update(cx, |workspace, cx| {
                    let view = cx.new(|cx| {
                        HistoryDetailView::new(entry, store, workspace_handle, window, cx)
                    });
                    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
                });
            });
            if opened.is_none() {
                log::warn!("the window this history belongs to is gone; no report was opened");
            }
        });
    }

    fn clear(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.store.read(cx).history.len();
        if count == 0 {
            return;
        }
        let message = format!(
            "Clear the history of {count} {}? This cannot be undone.",
            if count == 1 { "request" } else { "requests" }
        );
        let answer = window.prompt(
            PromptLevel::Warning,
            &message,
            None,
            &["Cancel", "Clear"],
            cx,
        );
        let store = self.store.clone();
        cx.spawn_in(window, async move |_, cx| {
            // Cancel comes first, so clearing is the second button.
            if answer.await == Ok(1) {
                store.update(cx, |store, cx| store.clear_history(cx));
            }
        })
        .detach();
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
                                    .on_click(
                                        cx.listener(|this, _, window, cx| this.clear(window, cx)),
                                    ),
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
    let entry_id = entry.id;
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
        .child(RequestView::render_method_badge(
            entry.method.clone().into(),
            RequestView::method_color_for_label(&entry.method),
            cx,
        ))
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
        .on_click(cx.listener(move |this, _, window, cx| this.open_detail(entry_id, window, cx)))
}

/// Opens `request_id` in a fresh, editable `RequestView` tab. Used by
/// `HistoryDetailView`'s "Edit & Resend" action -- a bounds-checked lookup by
/// id, so a request deleted after it was sent just leaves the button unable
/// to find anything, rather than opening stale or wrong data.
fn open_request_editor(
    store: &Entity<ApiClientStore>,
    workspace: &WeakEntity<Workspace>,
    request_id: RequestId,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(request) = store
        .read(cx)
        .requests
        .iter()
        .find(|request| request.id == request_id)
        .cloned()
    else {
        return;
    };
    let Some(workspace_entity) = workspace.upgrade() else {
        return;
    };
    let store = store.clone();
    let workspace_handle = workspace.clone();
    workspace_entity.update(cx, |workspace, cx| {
        let view = cx.new(|cx| {
            crate::request_view::RequestView::new(&request, store, workspace_handle, window, cx)
        });
        workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
    });
}

/// Formats a Unix timestamp in milliseconds as `YYYY-MM-DD HH:MM:SS UTC`,
/// without pulling in a date/time crate -- the same public-domain
/// civil-from-days algorithm `api_client` uses for `{{$isoTimestamp}}`, kept
/// as its own copy here since that one is private to `api_client`.
fn format_sent_at(sent_at_unix_ms: u64) -> String {
    const SECONDS_PER_DAY: u64 = 86_400;
    let total_seconds = sent_at_unix_ms / 1000;
    let days_since_epoch = total_seconds / SECONDS_PER_DAY;
    let seconds_of_day = total_seconds % SECONDS_PER_DAY;
    let (hour, minute, second) = (
        seconds_of_day / 3600,
        (seconds_of_day / 60) % 60,
        seconds_of_day % 60,
    );

    let z = days_since_epoch as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };

    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} UTC")
}

fn content_type_of(headers: &[(String, String)]) -> &str {
    headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        .map(|(_, value)| value.as_str())
        .unwrap_or("")
}

/// The best text to show for a body: pretty-printed when the content type is
/// recognized, the raw bytes lossily decoded otherwise. `None` only for a
/// genuinely empty body, so the caller can tell "no body was sent" apart
/// from "a body was sent, and it happens to be empty text".
fn body_preview_text(body: &[u8], content_type: &str) -> Option<String> {
    if body.is_empty() {
        return None;
    }
    Some(match pretty_print_body(body, content_type) {
        Some((pretty, _language)) => pretty,
        None => String::from_utf8_lossy(body).into_owned(),
    })
}

fn header_pairs(headers: &[(String, String)]) -> Vec<Pair> {
    headers
        .iter()
        .map(|(name, value)| Pair {
            name: name.clone().into(),
            value: value.clone().into(),
            also: None,
        })
        .collect()
}

/// A report of one past exchange: the request as it was actually sent, the
/// response (or the error that came back instead), how long it took, and
/// the environment used -- everything `HistoryExchangeDetail` carries.
/// Reusable across the app's history rather than a single shared "current
/// send" surface, since a report describes a specific point in the past and
/// must keep showing it even after later sends replace what is current.
///
/// Built once, at construction, from whatever `history_detail` returns for
/// this entry at that moment -- deliberately a frozen snapshot rather than a
/// live view: a report should keep showing what it opened to, even if the
/// underlying entry is later evicted from `history_details` or the history
/// itself is cleared.
pub struct HistoryDetailView {
    focus_handle: FocusHandle,
    store: Entity<ApiClientStore>,
    workspace: WeakEntity<Workspace>,
    entry: HistoryEntry,
    detail: Option<HistoryExchangeDetail>,
    request_body_editor: Entity<Editor>,
    response_body_editor: Entity<Editor>,
    scroll_handle: ScrollHandle,
    _subscription: Subscription,
}

impl HistoryDetailView {
    pub fn new(
        entry: HistoryEntry,
        store: Entity<ApiClientStore>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let detail = store.read(cx).history_detail(entry.id).cloned();

        let request_body_text =
            detail.as_ref().and_then(|detail| {
                detail.request.body.as_deref().and_then(|body| {
                    body_preview_text(body, content_type_of(&detail.request.headers))
                })
            });
        let response_body_text = detail.as_ref().and_then(|detail| match &detail.outcome {
            HistoryExchangeOutcome::Success(response) => {
                body_preview_text(&response.body, response.content_type())
            }
            HistoryExchangeOutcome::Error(_) => None,
        });

        let request_body_editor = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.set_read_only(true);
            if let Some(text) = request_body_text {
                editor.set_text(text, window, cx);
            }
            editor
        });
        let response_body_editor = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.set_read_only(true);
            if let Some(text) = response_body_text {
                editor.set_text(text, window, cx);
            }
            editor
        });

        // Kept in step with the store's structural changes so "Edit &
        // Resend" disables itself the moment the underlying request is
        // deleted, rather than only on the next unrelated re-render.
        let subscription = cx.subscribe(&store, |_, _, _event: &ApiClientStoreEvent, cx| {
            cx.notify();
        });

        Self {
            focus_handle: cx.focus_handle(),
            store,
            workspace,
            entry,
            detail,
            request_body_editor,
            response_body_editor,
            scroll_handle: ScrollHandle::new(),
            _subscription: subscription,
        }
    }

    fn open_request(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(request_id) = self.entry.request_id else {
            return;
        };
        let Some(workspace_entity) = self.workspace.upgrade() else {
            return;
        };
        let workspace_entity_id = workspace_entity.entity_id();
        let store = self.store.clone();
        let workspace = self.workspace.clone();
        // `open_request_editor` adds a tab to the same pane this
        // `HistoryDetailView` is itself sitting in as the active item, which
        // makes the pane scan every existing item -- this one included -- to
        // check for a duplicate, reading it while this click handler still
        // holds it leased. `cx.defer_in` would not help here: it re-wraps
        // this same entity in a fresh lease for the whole deferred callback,
        // which the pane scan would still collide with -- see
        // `HistoryView::open_detail` for the same hazard and the same fix.
        cx.defer(move |cx| {
            let opened = cx.with_window(workspace_entity_id, |window, cx| {
                open_request_editor(&store, &workspace, request_id, window, cx);
            });
            if opened.is_none() {
                log::warn!("the window this report belongs to is gone; the request was not opened");
            }
        });
    }

    fn can_open_request(&self, cx: &App) -> bool {
        self.entry.request_id.is_some_and(|request_id| {
            self.store
                .read(cx)
                .requests
                .iter()
                .any(|request| request.id == request_id)
        })
    }
}

impl EventEmitter<()> for HistoryDetailView {}

impl Focusable for HistoryDetailView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for HistoryDetailView {
    type Event = ();

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        format!("{} {}", self.entry.method, self.entry.url).into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::HistoryRerun))
    }

    fn to_item_events(_event: &Self::Event, _f: &mut dyn FnMut(ItemEvent)) {}
}

fn labeled_row(label: &'static str, value: impl IntoElement) -> impl IntoElement {
    h_flex()
        .gap_3()
        .child(
            div()
                .flex_none()
                .w(px(96.))
                .child(Label::new(label).size(LabelSize::Small).color(Color::Muted)),
        )
        .child(div().flex_1().min_w_0().child(value))
}

fn body_box(
    text: Option<String>,
    empty_message: &'static str,
    editor: Entity<Editor>,
    border: Hsla,
) -> AnyElement {
    match text {
        None => Label::new(empty_message)
            .size(LabelSize::Small)
            .color(Color::Muted)
            .into_any_element(),
        Some(_) => div()
            .w_full()
            .h(px(160.))
            .rounded_md()
            .border_1()
            .border_color(border)
            .child(editor)
            .into_any_element(),
    }
}

impl Render for HistoryDetailView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let editor_background = cx.theme().colors().editor_background;
        let border = cx.theme().colors().border;
        let entry = self.entry.clone();
        let can_open_request = self.can_open_request(cx);

        let status_label = match entry.status {
            Some(status) => status.to_string(),
            None => "Failed".to_string(),
        };
        let (status_icon, status_color) = match entry.status {
            Some(status) if (200..300).contains(&status) => (IconName::Check, Color::Success),
            Some(_) => (IconName::Warning, Color::Warning),
            None => (IconName::XCircle, Color::Error),
        };

        let mut summary = h_flex()
            .flex_wrap()
            .gap_4()
            .child(labeled_row(
                "Sent",
                Label::new(format_sent_at(entry.sent_at_unix_ms)).size(LabelSize::Small),
            ))
            .child(labeled_row(
                "Environment",
                Label::new(
                    self.detail
                        .as_ref()
                        .and_then(|detail| detail.environment_name.clone())
                        .unwrap_or_else(|| "None".to_string()),
                )
                .size(LabelSize::Small),
            ));
        if let Some(HistoryExchangeOutcome::Success(response)) =
            self.detail.as_ref().map(|detail| &detail.outcome)
        {
            summary = summary
                .child(labeled_row(
                    "Duration",
                    Label::new(format!("{} ms", response.elapsed_ms)).size(LabelSize::Small),
                ))
                .child(labeled_row(
                    "Size",
                    Label::new(format_size(response.size_bytes)).size(LabelSize::Small),
                ));
        }

        let request_section = v_flex()
            .gap_2()
            .child(Label::new("Request").size(LabelSize::Large))
            .child(match &self.detail {
                Some(detail) => v_flex()
                    .gap_2()
                    .child(
                        Label::new(format!("{} {}", detail.request.method, detail.request.url))
                            .size(LabelSize::Small)
                            .buffer_font(cx),
                    )
                    .child(render_pairs(
                        "history-detail-request-headers",
                        "No headers were sent.",
                        header_pairs(&detail.request.headers),
                        cx,
                    ))
                    .child(
                        div()
                            .id("history-detail-request-body-hitbox")
                            .debug_selector(|| "history-detail-request-body".to_string())
                            .child(body_box(
                                detail.request.body.as_deref().and_then(|body| {
                                    body_preview_text(
                                        body,
                                        content_type_of(&detail.request.headers),
                                    )
                                }),
                                "No request body.",
                                self.request_body_editor.clone(),
                                border,
                            )),
                    )
                    .into_any_element(),
                None => Label::new(
                    "Full request detail is only kept for the current session -- this entry \
                     predates it or has aged out.",
                )
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element(),
            });

        let response_section = v_flex()
            .gap_2()
            .child(Label::new("Response").size(LabelSize::Large))
            .child(match self.detail.as_ref().map(|detail| &detail.outcome) {
                Some(HistoryExchangeOutcome::Success(response)) => v_flex()
                    .gap_2()
                    .child(render_pairs(
                        "history-detail-response-headers",
                        "No headers were returned.",
                        header_pairs(&response.headers),
                        cx,
                    ))
                    .child(
                        div()
                            .id("history-detail-response-body-hitbox")
                            .debug_selector(|| "history-detail-response-body".to_string())
                            .child(body_box(
                                body_preview_text(&response.body, response.content_type()),
                                "No response body.",
                                self.response_body_editor.clone(),
                                border,
                            )),
                    )
                    .child(
                        Label::new("Timing")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(render_timing(response.timings, cx))
                    .into_any_element(),
                Some(HistoryExchangeOutcome::Error(message)) => Label::new(message.clone())
                    .size(LabelSize::Small)
                    .color(Color::Error)
                    .into_any_element(),
                None => Label::new("No response detail is available for this entry.")
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .into_any_element(),
            });

        let scrollable = v_flex()
            .id("api-client-history-detail")
            .flex_1()
            .min_h_0()
            .overflow_scroll()
            .track_scroll(&self.scroll_handle)
            .gap_4()
            .child(request_section)
            .child(response_section);
        let scrollable = scrollable.custom_scrollbars(
            Scrollbars::always_visible(ScrollAxes::Vertical)
                .tracked_scroll_handle(&self.scroll_handle),
            window,
            cx,
        );

        v_flex()
            .key_context("ApiClientHistoryDetailView")
            .track_focus(&self.focus_handle)
            .size_full()
            .p_4()
            .gap_3()
            .bg(editor_background)
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(RequestView::render_method_badge(
                                entry.method.clone().into(),
                                RequestView::method_color_for_label(&entry.method),
                                cx,
                            ))
                            .child(
                                Icon::new(status_icon)
                                    .size(IconSize::Small)
                                    .color(status_color),
                            )
                            .child(Label::new(status_label).color(status_color))
                            .child(Label::new(entry.url.clone())),
                    )
                    .child(
                        div()
                            .id("history-detail-edit-hitbox")
                            .debug_selector(|| "history-detail-edit".to_string())
                            .child(
                                Button::new("history-detail-edit", "Edit & Resend")
                                    .style(ButtonStyle::Subtle)
                                    .disabled(!can_open_request)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_request(window, cx)
                                    })),
                            ),
                    ),
            )
            .child(summary)
            .child(scrollable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext, WindowHandle};
    use project::Project;
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
        WindowHandle<Workspace>,
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
        (store, view, workspace_window, cx)
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

    /// Clearing the history is not undoable, so it asks first. The click alone
    /// must no longer be enough.
    #[gpui::test]
    async fn clicking_clear_empties_the_history_list_once_confirmed(cx: &mut TestAppContext) {
        let (store, _view, _workspace_window, mut cx) = build_history_view(cx).await;
        record_one_entry(&store, &mut cx);
        draw(&mut cx);

        let clear_button = debug_center(&mut cx, "history-clear");
        cx.simulate_click(clear_button, gpui::Modifiers::none());
        cx.run_until_parked();
        cx.simulate_prompt_answer("Clear");
        cx.run_until_parked();

        store.read_with(&cx, |store, _| assert!(store.history.is_empty()));
    }

    #[gpui::test]
    async fn cancelling_the_clear_prompt_keeps_the_history(cx: &mut TestAppContext) {
        let (store, _view, _workspace_window, mut cx) = build_history_view(cx).await;
        record_one_entry(&store, &mut cx);
        draw(&mut cx);

        let clear_button = debug_center(&mut cx, "history-clear");
        cx.simulate_click(clear_button, gpui::Modifiers::none());
        cx.run_until_parked();
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();

        store.read_with(&cx, |store, _| {
            assert_eq!(
                store.history.len(),
                1,
                "answering Cancel must leave the history alone"
            )
        });
    }

    fn record_one_entry(store: &Entity<ApiClientStore>, cx: &mut VisualTestContext) {
        store.update(cx, |store, cx| {
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
    }

    fn sample_history_detail(
        response_headers: Vec<(String, String)>,
        response_body: &str,
    ) -> HistoryExchangeDetail {
        HistoryExchangeDetail {
            request: api_client::ResolvedRequest {
                method: "GET".into(),
                url: "https://api.example.com/ping".into(),
                headers: vec![("Accept".into(), "application/json".into())],
                body: None,
            },
            outcome: HistoryExchangeOutcome::Success(crate::response_view::ResponseData {
                status: 200,
                status_text: "OK".into(),
                elapsed_ms: 42,
                size_bytes: response_body.len(),
                headers: response_headers,
                body: response_body.as_bytes().to_vec(),
                cookies: Vec::new(),
                timings: api_client::Timings::default(),
            }),
            environment_name: Some("Staging".into()),
        }
    }

    fn record_entry_with_detail(
        store: &Entity<ApiClientStore>,
        cx: &mut VisualTestContext,
        entry: HistoryEntry,
        detail: HistoryExchangeDetail,
    ) {
        store.update(cx, |store, cx| {
            store.record_history_detail(entry.id, detail);
            store.record_history_entry(entry, cx);
        });
    }

    #[gpui::test]
    async fn clicking_a_history_row_opens_its_detail_report(cx: &mut TestAppContext) {
        let (store, _view, workspace_window, mut cx) = build_history_view(cx).await;
        let collection_id =
            store.update(&mut cx, |store, cx| store.create_collection("A".into(), cx));
        let request_id = store.update(&mut cx, |store, cx| {
            store.create_request(collection_id, "Ping".into(), None, cx)
        });
        let entry = HistoryEntry::new(
            request_id,
            "GET".into(),
            "https://api.example.com/ping".into(),
            Some(200),
            1_700_000_000_000,
        );
        record_entry_with_detail(
            &store,
            &mut cx,
            entry,
            sample_history_detail(Vec::new(), "pong"),
        );
        draw(&mut cx);

        let row = debug_center(&mut cx, "history-row-0");
        cx.simulate_click(row, gpui::Modifiers::none());
        cx.run_until_parked();

        let opened_the_report =
            workspace_window
                .read_with(&cx, |workspace, cx| {
                    workspace.active_pane().read(cx).items().any(|item| {
                        item.tab_content_text(0, cx) == "GET https://api.example.com/ping"
                    })
                })
                .unwrap();
        assert!(
            opened_the_report,
            "clicking a history row must open its exchange report, not the editable request"
        );
    }

    #[gpui::test]
    async fn edit_and_resend_opens_the_still_existing_request_for_editing(cx: &mut TestAppContext) {
        let (store, _view, workspace_window, mut cx) = build_history_view(cx).await;
        let collection_id =
            store.update(&mut cx, |store, cx| store.create_collection("A".into(), cx));
        let request_id = store.update(&mut cx, |store, cx| {
            store.create_request(collection_id, "Ping".into(), None, cx)
        });
        let entry = HistoryEntry::new(
            request_id,
            "GET".into(),
            "https://api.example.com/ping".into(),
            Some(200),
            1_700_000_000_000,
        );
        record_entry_with_detail(
            &store,
            &mut cx,
            entry,
            sample_history_detail(Vec::new(), "pong"),
        );
        draw(&mut cx);

        let row = debug_center(&mut cx, "history-row-0");
        cx.simulate_click(row, gpui::Modifiers::none());
        cx.run_until_parked();

        let mut workspace_cx = VisualTestContext::from_window(workspace_window.into(), &cx);
        draw(&mut workspace_cx);
        let edit_button = debug_center(&mut workspace_cx, "history-detail-edit");
        workspace_cx.simulate_click(edit_button, gpui::Modifiers::none());
        workspace_cx.run_until_parked();

        let opened_a_request_tab = workspace_window
            .read_with(&workspace_cx, |workspace, cx| {
                workspace
                    .active_pane()
                    .read(cx)
                    .items()
                    .any(|item| item.tab_content_text(0, cx) == "Ping")
            })
            .unwrap();
        assert!(
            opened_a_request_tab,
            "Edit & Resend must open the underlying request for editing"
        );
    }

    /// The row a reader is looking at is identified by the row's own id, not
    /// by its position: a send that completes in the background between the
    /// last paint and the click reorders `history`, and resolving by
    /// position would silently open whatever now sits there instead of what
    /// the reader actually clicked. Fails against a position-based lookup.
    #[gpui::test]
    async fn a_row_opens_the_entry_it_was_painted_for_after_the_list_has_shifted(
        cx: &mut TestAppContext,
    ) {
        let (store, view, workspace_window, mut cx) = build_history_view(cx).await;
        let first = HistoryEntry::new(
            Uuid::new_v4(),
            "GET".into(),
            "https://api.example.com/first".into(),
            Some(200),
            1_700_000_000_000,
        );
        let first_id = first.id;
        store.update(&mut cx, |store, cx| {
            store.record_history_entry(first, cx);
        });
        draw(&mut cx);

        // A send that completes after the list was painted lands at position 0
        // and pushes what was painted as row 0 down to row 1.
        store.update(&mut cx, |store, cx| {
            store.record_history_entry(
                HistoryEntry::new(
                    Uuid::new_v4(),
                    "GET".into(),
                    "https://api.example.com/second".into(),
                    Some(200),
                    1_700_000_000_001,
                ),
                cx,
            );
        });
        draw(&mut cx);

        // The action a row carries is asked for directly rather than clicked.
        // A click cannot stage this: gpui dispatches it against the frame that
        // painted the row, and that frame's own handler is the one that runs,
        // so the test would only ever prove that the newest paint agrees with
        // itself. What has to hold is that the identity a row hands on is the
        // entry's, not its place in a list that moves under it -- which is
        // what shifting the list and then resolving the earlier identity
        // shows. `clicking_a_history_row_opens_its_detail_report` covers the
        // press itself.
        view.update_in(&mut cx, |view, window, cx| {
            view.open_detail(first_id, window, cx);
        });
        cx.run_until_parked();

        let opened: Vec<String> = workspace_window
            .read_with(&cx, |workspace, cx| {
                workspace
                    .active_pane()
                    .read(cx)
                    .items()
                    .map(|item| item.tab_content_text(0, cx).to_string())
                    .collect()
            })
            .unwrap();
        assert_eq!(
            opened,
            vec!["GET https://api.example.com/first".to_string()],
            "the entry a row was painted for has to be the one that opens, however \
             far the list has moved since"
        );
    }

    #[gpui::test]
    async fn the_detail_report_paints_the_recorded_response_headers_and_body(
        cx: &mut TestAppContext,
    ) {
        let (store, _view, workspace_window, mut cx) = build_history_view(cx).await;
        let entry = HistoryEntry::new(
            Uuid::new_v4(),
            "GET".into(),
            "https://api.example.com/ping".into(),
            Some(200),
            1_700_000_000_000,
        );
        record_entry_with_detail(
            &store,
            &mut cx,
            entry,
            sample_history_detail(
                vec![("X-Trace-Id".into(), "abc-123".into())],
                r#"{"pong":true}"#,
            ),
        );
        draw(&mut cx);

        let row = debug_center(&mut cx, "history-row-0");
        cx.simulate_click(row, gpui::Modifiers::none());
        cx.run_until_parked();

        let mut workspace_cx = VisualTestContext::from_window(workspace_window.into(), &cx);
        draw(&mut workspace_cx);

        assert!(
            workspace_cx
                .debug_bounds("history-detail-response-headers-row-0")
                .is_some(),
            "the response header the exchange actually returned must be painted"
        );
        assert!(
            workspace_cx
                .debug_bounds("history-detail-response-body")
                .is_some(),
            "the response body box must be painted"
        );

        let detail_view = workspace_window
            .read_with(&workspace_cx, |workspace, cx| {
                workspace
                    .active_pane()
                    .read(cx)
                    .items_of_type::<HistoryDetailView>()
                    .next()
            })
            .unwrap()
            .expect("the detail report must be the active item");
        let response_text = detail_view.read_with(&workspace_cx, |view, cx| {
            view.response_body_editor.read(cx).text(cx)
        });
        assert!(
            response_text.contains("pong"),
            "the body editor must show the actual recorded response body, got {response_text:?}"
        );
    }

    /// A deleted request leaves history alone (see `HistoryEntry`'s own
    /// doc), so the report must still open and render -- it just cannot
    /// offer to reopen a request that is no longer there.
    #[gpui::test]
    async fn a_report_for_a_deleted_request_still_opens_with_edit_and_resend_disabled(
        cx: &mut TestAppContext,
    ) {
        let (store, _view, workspace_window, mut cx) = build_history_view(cx).await;
        let entry = HistoryEntry::new(
            Uuid::new_v4(),
            "GET".into(),
            "https://api.example.com/gone".into(),
            Some(200),
            1_700_000_000_000,
        );
        record_entry_with_detail(
            &store,
            &mut cx,
            entry,
            sample_history_detail(Vec::new(), "pong"),
        );
        draw(&mut cx);

        let row = debug_center(&mut cx, "history-row-0");
        cx.simulate_click(row, gpui::Modifiers::none());
        cx.run_until_parked();

        let mut workspace_cx = VisualTestContext::from_window(workspace_window.into(), &cx);
        draw(&mut workspace_cx);
        let items_before = workspace_window
            .read_with(&workspace_cx, |workspace, cx| {
                workspace.active_pane().read(cx).items().count()
            })
            .unwrap();

        let edit_button = debug_center(&mut workspace_cx, "history-detail-edit");
        workspace_cx.simulate_click(edit_button, gpui::Modifiers::none());
        workspace_cx.run_until_parked();

        let items_after = workspace_window
            .read_with(&workspace_cx, |workspace, cx| {
                workspace.active_pane().read(cx).items().count()
            })
            .unwrap();
        assert_eq!(
            items_before, items_after,
            "Edit & Resend must do nothing for a request that no longer exists, not open a \
             stale or empty tab"
        );
    }

    #[test]
    fn format_sent_at_renders_a_known_unix_millisecond_timestamp() {
        assert_eq!(format_sent_at(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(format_sent_at(1_700_000_000_000), "2023-11-14 22:13:20 UTC");
    }

    #[test]
    fn format_sent_at_truncates_rather_than_rounds_the_millisecond_remainder() {
        assert_eq!(format_sent_at(1_700_000_000_999), "2023-11-14 22:13:20 UTC");
    }
}
