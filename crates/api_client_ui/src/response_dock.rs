use crate::response_view::{ResponseData, ResponseTab, format_size};
use api_client::TestResult;
use editor::Editor;
use gpui::{
    AnyElement, App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, ParentElement, Pixels, Render, ScrollHandle, SharedString, Styled, WeakEntity,
    Window, div,
};
use ui::{IconName, Label, LabelSize, ScrollAxes, Scrollbars, WithScrollbar, prelude::*};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};
use zed_actions::api_client_panel::ToggleResponseDockFocus;

const API_RESPONSE_DOCK_PANEL_KEY: &str = "ApiResponseDockPanel";

/// Everything the dock needs to render a completed response. The body
/// editors are the very same `Entity<Editor>` the originating `RequestView`
/// populated in `apply_response` -- the dock displays them rather than
/// rebuilding pretty/raw/preview text itself, so there is exactly one place
/// that turns a response body into pretty-printed JSON or stripped-HTML
/// preview text.
pub struct DockResponseEntry {
    pub request_title: SharedString,
    pub response: ResponseData,
    pub response_is_html: bool,
    pub pretty_body_editor: Entity<Editor>,
    pub raw_body_editor: Entity<Editor>,
    pub preview_body_editor: Entity<Editor>,
    pub test_results: Vec<TestResult>,
    pub visualize_data: Option<serde_json::Value>,
}

enum ResponseDockDisplay {
    Idle,
    Sending {
        request_title: SharedString,
    },
    Error {
        request_title: SharedString,
        message: String,
    },
    Success(DockResponseEntry),
}

/// Whether `current` still makes sense to keep selected once a response with
/// this shape (html body, ran tests, has visualize data) becomes the dock's
/// new response -- e.g. `TestResults` was showing, but the new response
/// never ran a test script. Falls back to `Pretty` otherwise, the one tab
/// every response can always show. Takes plain fields rather than a
/// `DockResponseEntry` so the reset rule is testable without a `Context`
/// (building a real `DockResponseEntry` needs `Entity<Editor>`s, which need
/// a window).
fn next_response_tab(
    current: ResponseTab,
    response_is_html: bool,
    has_test_results: bool,
    has_visualize_data: bool,
) -> ResponseTab {
    let tab_still_applies = match current {
        ResponseTab::Preview => response_is_html,
        ResponseTab::TestResults => has_test_results,
        ResponseTab::Visualize => has_visualize_data,
        ResponseTab::Diff => false,
        ResponseTab::Pretty | ResponseTab::Raw | ResponseTab::Headers | ResponseTab::Cookies => {
            true
        }
    };
    if tab_still_applies {
        current
    } else {
        ResponseTab::Pretty
    }
}

/// A single bottom-dock surface shared by every request, always showing the
/// most recent send regardless of which request produced it -- a second send
/// simply replaces `display`, matching the "one shared response view" this
/// panel exists to provide instead of each request tab keeping its own.
/// Identifies one send, so updates that arrive out of order can be told apart.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct SendGeneration(u64);

pub struct ResponseDockPanel {
    focus_handle: FocusHandle,
    display: ResponseDockDisplay,
    generation: SendGeneration,
    response_tab: ResponseTab,
    scroll_handle: ScrollHandle,
}

impl EventEmitter<PanelEvent> for ResponseDockPanel {}

impl Focusable for ResponseDockPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ResponseDockPanel {
    pub(crate) fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            display: ResponseDockDisplay::Idle,
            generation: SendGeneration::default(),
            response_tab: ResponseTab::Pretty,
            scroll_handle: ScrollHandle::new(),
        }
    }

    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        workspace.update(&mut cx, |_workspace, cx| cx.new(|cx| Self::new(cx)))
    }

    /// Claims the dock for a send that is starting. Requests finish in whatever
    /// order the network answers, so every later update carries this number and
    /// an older one is ignored -- otherwise a slow first request would land on
    /// top of a newer one's reply.
    pub fn begin_send(
        &mut self,
        request_title: SharedString,
        cx: &mut Context<Self>,
    ) -> SendGeneration {
        self.generation = SendGeneration(self.generation.0 + 1);
        self.display = ResponseDockDisplay::Sending { request_title };
        cx.notify();
        self.generation
    }

    /// Which send the dock is showing, so a request view can tell whether the
    /// response on screen is still its own.
    pub fn showing(&self) -> SendGeneration {
        self.generation
    }

    fn is_current(&self, generation: SendGeneration) -> bool {
        generation == self.generation
    }

    pub fn show_error(
        &mut self,
        generation: SendGeneration,
        request_title: SharedString,
        message: String,
        cx: &mut Context<Self>,
    ) {
        if !self.is_current(generation) {
            return;
        }
        self.display = ResponseDockDisplay::Error {
            request_title,
            message,
        };
        cx.notify();
    }

    /// Replaces whatever the dock is currently showing. Falls back to the
    /// `Pretty` tab when the tab currently selected no longer applies to
    /// this entry (e.g. `Test Results` was selected but this response has no
    /// test script), the same reset `RequestView::apply_response` used to do
    /// for its own per-request tab state.
    pub fn show_response(
        &mut self,
        generation: SendGeneration,
        entry: DockResponseEntry,
        cx: &mut Context<Self>,
    ) {
        if !self.is_current(generation) {
            return;
        }
        self.response_tab = next_response_tab(
            self.response_tab,
            entry.response_is_html,
            !entry.test_results.is_empty(),
            entry.visualize_data.is_some(),
        );
        self.display = ResponseDockDisplay::Success(entry);
        cx.notify();
    }

    fn render_tab_chip(
        label: &'static str,
        is_selected: bool,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Self, &gpui::ClickEvent, &mut Window, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        let colors = cx.theme().colors();
        div()
            .id(SharedString::from(format!("api-response-dock-tab-{label}")))
            .debug_selector(move || format!("api-response-dock-tab-{label}"))
            .px_2()
            .py_0p5()
            .rounded_md()
            .cursor_pointer()
            .when(is_selected, |el| el.bg(colors.element_selected))
            .when(!is_selected, |el| {
                el.hover(|el| el.bg(colors.element_hover))
            })
            .child(
                Label::new(label)
                    .size(LabelSize::Small)
                    .color(if is_selected {
                        Color::Default
                    } else {
                        Color::Muted
                    }),
            )
            .on_click(cx.listener(on_click))
    }
}

impl Render for ResponseDockPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let border = cx.theme().colors().border;
        let background = cx.theme().colors().background;
        let line_height = window.line_height();
        let response_tab = self.response_tab;

        let content: AnyElement = match &self.display {
            ResponseDockDisplay::Idle => {
                Label::new("No response yet -- send a request to see it here.")
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .into_any_element()
            }
            ResponseDockDisplay::Sending { request_title } => h_flex()
                .gap_2()
                .child(
                    Label::new(format!("Sending {request_title}..."))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element(),
            ResponseDockDisplay::Error {
                request_title,
                message,
            } => v_flex()
                .gap_1()
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Label::new("Request failed")
                                .size(LabelSize::Small)
                                .color(Color::Error),
                        )
                        .child(
                            Label::new(request_title.clone())
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                )
                .child(
                    Label::new(message.clone())
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element(),
            ResponseDockDisplay::Success(entry) => {
                let status = entry.response.status;
                let status_color = if (200..300).contains(&status) {
                    Color::Success
                } else if (400..600).contains(&status) {
                    Color::Error
                } else {
                    Color::Warning
                };

                let summary_row = h_flex()
                    .justify_between()
                    .child(
                        h_flex()
                            .gap_3()
                            .items_center()
                            .child(crate::request_view::RequestView::render_method_badge(
                                format!("{} {}", entry.response.status, entry.response.status_text)
                                    .into(),
                                status_color,
                                cx,
                            ))
                            .child(
                                Label::new(format!("{} ms", entry.response.elapsed_ms))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(
                                Label::new(format_size(entry.response.size_bytes))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                    )
                    .child(
                        Label::new(entry.request_title.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    );

                let mut tab_strip = h_flex().gap_2().child(Self::render_tab_chip(
                    "Pretty",
                    response_tab == ResponseTab::Pretty,
                    cx,
                    |this, _, _, cx| {
                        this.response_tab = ResponseTab::Pretty;
                        cx.notify();
                    },
                ));
                if entry.response_is_html {
                    tab_strip = tab_strip.child(Self::render_tab_chip(
                        "Preview",
                        response_tab == ResponseTab::Preview,
                        cx,
                        |this, _, _, cx| {
                            this.response_tab = ResponseTab::Preview;
                            cx.notify();
                        },
                    ));
                }
                tab_strip = tab_strip
                    .child(Self::render_tab_chip(
                        "Raw",
                        response_tab == ResponseTab::Raw,
                        cx,
                        |this, _, _, cx| {
                            this.response_tab = ResponseTab::Raw;
                            cx.notify();
                        },
                    ))
                    .child(Self::render_tab_chip(
                        "Headers",
                        response_tab == ResponseTab::Headers,
                        cx,
                        |this, _, _, cx| {
                            this.response_tab = ResponseTab::Headers;
                            cx.notify();
                        },
                    ))
                    .child(Self::render_tab_chip(
                        "Cookies",
                        response_tab == ResponseTab::Cookies,
                        cx,
                        |this, _, _, cx| {
                            this.response_tab = ResponseTab::Cookies;
                            cx.notify();
                        },
                    ));
                if !entry.test_results.is_empty() {
                    tab_strip = tab_strip.child(Self::render_tab_chip(
                        "Test Results",
                        response_tab == ResponseTab::TestResults,
                        cx,
                        |this, _, _, cx| {
                            this.response_tab = ResponseTab::TestResults;
                            cx.notify();
                        },
                    ));
                }
                if entry.visualize_data.is_some() {
                    tab_strip = tab_strip.child(Self::render_tab_chip(
                        "Visualize",
                        response_tab == ResponseTab::Visualize,
                        cx,
                        |this, _, _, cx| {
                            this.response_tab = ResponseTab::Visualize;
                            cx.notify();
                        },
                    ));
                }

                let body: AnyElement = match response_tab {
                    ResponseTab::TestResults => {
                        let mut list = v_flex().gap_1();
                        for test in &entry.test_results {
                            let (color, status_text) = if test.passed {
                                (Color::Success, "PASS")
                            } else {
                                (Color::Error, "FAIL")
                            };
                            let mut row = v_flex().gap_0p5().child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        Label::new(status_text).size(LabelSize::Small).color(color),
                                    )
                                    .child(Label::new(test.name.clone()).size(LabelSize::Small)),
                            );
                            if let Some(error) = &test.error {
                                row = row.child(
                                    Label::new(error.clone())
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                );
                            }
                            list = list.child(row);
                        }
                        list.into_any_element()
                    }
                    ResponseTab::Visualize => {
                        let text = entry
                            .visualize_data
                            .as_ref()
                            .and_then(|data| serde_json::to_string_pretty(data).ok())
                            .unwrap_or_default();
                        v_flex()
                            .gap_1()
                            .child(
                                Label::new("Rendered as formatted JSON -- GPUI has no sandboxed HTML renderer, so pm.visualize() data is shown as data, not an HTML template.")
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_h(px(200.))
                                    .px_2()
                                    .py_1p5()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(border)
                                    .bg(background)
                                    .child(Label::new(text).size(LabelSize::Small)),
                            )
                            .into_any_element()
                    }
                    ResponseTab::Pretty | ResponseTab::Preview | ResponseTab::Raw => {
                        let editor = match response_tab {
                            ResponseTab::Pretty => entry.pretty_body_editor.clone(),
                            ResponseTab::Preview => entry.preview_body_editor.clone(),
                            _ => entry.raw_body_editor.clone(),
                        };
                        let mut column = v_flex().gap_1().flex_1();
                        if response_tab == ResponseTab::Preview {
                            column = column.child(
                                Label::new("Rendered as plain text -- GPUI has no sandboxed HTML renderer, so scripts and styles are stripped rather than executed.")
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            );
                        }
                        let line_count = editor.read(cx).text(cx).lines().count().max(1);
                        let content_height = line_height * line_count as f32 + px(24.);
                        column
                            .child(
                                div()
                                    .flex_initial()
                                    .h(content_height.max(px(120.)))
                                    .px_2()
                                    .py_1p5()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(border)
                                    .bg(background)
                                    .child(editor),
                            )
                            .into_any_element()
                    }
                    ResponseTab::Headers => {
                        let mut list = v_flex().gap_1();
                        for (key, value) in &entry.response.headers {
                            list = list.child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        Label::new(key.clone())
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    )
                                    .child(Label::new(value.clone()).size(LabelSize::Small)),
                            );
                        }
                        list.into_any_element()
                    }
                    ResponseTab::Cookies => {
                        if entry.response.cookies.is_empty() {
                            Label::new("No cookies in this response.")
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .into_any_element()
                        } else {
                            let mut list = v_flex().gap_1();
                            for cookie in &entry.response.cookies {
                                list = list.child(
                                    h_flex()
                                        .gap_2()
                                        .child(
                                            Label::new(cookie.name.clone())
                                                .size(LabelSize::Small)
                                                .color(Color::Accent),
                                        )
                                        .child(
                                            Label::new(cookie.value.clone()).size(LabelSize::Small),
                                        )
                                        .child(
                                            Label::new(cookie.attributes.clone())
                                                .size(LabelSize::Small)
                                                .color(Color::Muted),
                                        ),
                                );
                            }
                            list.into_any_element()
                        }
                    }
                    ResponseTab::Diff => Label::new(
                        "Diff is only available from the request that produced the response.",
                    )
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .into_any_element(),
                };

                v_flex()
                    .gap_2()
                    .child(summary_row)
                    .child(tab_strip)
                    .child(body)
                    .into_any_element()
            }
        };

        v_flex()
            .id("api-response-dock")
            .debug_selector(|| "api-response-dock".to_string())
            .track_focus(&self.focus_handle)
            .size_full()
            .p_2()
            .gap_2()
            .overflow_scroll()
            .track_scroll(&self.scroll_handle)
            .child(content)
            .custom_scrollbars(
                Scrollbars::always_visible(ScrollAxes::Vertical)
                    .tracked_scroll_handle(&self.scroll_handle),
                window,
                cx,
            )
            .into_any_element()
    }
}

impl Panel for ResponseDockPanel {
    fn persistent_name() -> &'static str {
        API_RESPONSE_DOCK_PANEL_KEY
    }

    fn panel_key() -> &'static str {
        API_RESPONSE_DOCK_PANEL_KEY
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Bottom
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        position == DockPosition::Bottom
    }

    fn set_position(
        &mut self,
        _position: DockPosition,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(280.)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<ui::IconName> {
        Some(IconName::ReplyArrowRight)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("API Response")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleResponseDockFocus)
    }

    fn activation_priority(&self) -> u32 {
        10
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[test]
    fn a_tab_that_still_applies_to_the_new_response_is_kept() {
        assert_eq!(
            next_response_tab(ResponseTab::Headers, false, false, false),
            ResponseTab::Headers
        );
        assert_eq!(
            next_response_tab(ResponseTab::Preview, true, false, false),
            ResponseTab::Preview
        );
        assert_eq!(
            next_response_tab(ResponseTab::TestResults, false, true, false),
            ResponseTab::TestResults
        );
        assert_eq!(
            next_response_tab(ResponseTab::Visualize, false, false, true),
            ResponseTab::Visualize
        );
    }

    #[test]
    fn a_tab_the_new_response_no_longer_supports_resets_to_pretty() {
        assert_eq!(
            next_response_tab(ResponseTab::Preview, false, false, false),
            ResponseTab::Pretty
        );
        assert_eq!(
            next_response_tab(ResponseTab::TestResults, false, false, false),
            ResponseTab::Pretty
        );
        assert_eq!(
            next_response_tab(ResponseTab::Visualize, false, false, false),
            ResponseTab::Pretty
        );
    }

    #[test]
    fn the_diff_tab_never_carries_over_since_the_dock_never_offers_it() {
        assert_eq!(
            next_response_tab(ResponseTab::Diff, true, true, true),
            ResponseTab::Pretty
        );
    }

    fn sample_response(status: u16) -> ResponseData {
        ResponseData {
            status,
            status_text: "status".into(),
            elapsed_ms: 1,
            size_bytes: 2,
            headers: Vec::new(),
            body: b"{}".to_vec(),
            cookies: Vec::new(),
        }
    }

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
    }

    /// The dock is one shared surface: sending a second response must
    /// replace the first entirely, regardless of which request produced
    /// either one -- there is no per-request slot to fall back to.
    #[gpui::test]
    fn showing_a_second_response_replaces_the_first(cx: &mut TestAppContext) {
        init_test(cx);
        let window = cx.add_window(|_, cx| ResponseDockPanel::new(cx));
        let dock = window.root(cx).unwrap();

        let first_editor = window
            .update(cx, |_, window, cx| {
                cx.new(|cx| Editor::multi_line(window, cx))
            })
            .unwrap();
        let second_editor = window
            .update(cx, |_, window, cx| {
                cx.new(|cx| Editor::multi_line(window, cx))
            })
            .unwrap();

        dock.update(cx, |dock, cx| {
            let first = dock.begin_send("First".into(), cx);
            dock.show_response(
                first,
                DockResponseEntry {
                    request_title: "First".into(),
                    response: sample_response(200),
                    response_is_html: false,
                    pretty_body_editor: first_editor.clone(),
                    raw_body_editor: first_editor.clone(),
                    preview_body_editor: first_editor.clone(),
                    test_results: Vec::new(),
                    visualize_data: None,
                },
                cx,
            );
        });
        dock.update(cx, |dock, cx| {
            let second = dock.begin_send("Second".into(), cx);
            dock.show_response(
                second,
                DockResponseEntry {
                    request_title: "Second".into(),
                    response: sample_response(500),
                    response_is_html: false,
                    pretty_body_editor: second_editor.clone(),
                    raw_body_editor: second_editor.clone(),
                    preview_body_editor: second_editor.clone(),
                    test_results: Vec::new(),
                    visualize_data: None,
                },
                cx,
            );
        });

        dock.read_with(cx, |dock, _| match &dock.display {
            ResponseDockDisplay::Success(entry) => {
                assert_eq!(entry.request_title.as_ref(), "Second");
                assert_eq!(entry.response.status, 500);
            }
            _ => panic!("expected the dock to be showing the most recently sent response"),
        });
    }

    /// Requests finish in whatever order the network answers, and the dock shows
    /// one response for the whole workspace, so a slow earlier send must not land
    /// on top of a newer one's reply.
    #[gpui::test]
    fn a_late_reply_does_not_replace_a_newer_one(cx: &mut TestAppContext) {
        init_test(cx);
        let window = cx.add_window(|_, cx| ResponseDockPanel::new(cx));
        let dock = window.root(cx).unwrap();
        let editor = window
            .update(cx, |_, window, cx| {
                cx.new(|cx| Editor::multi_line(window, cx))
            })
            .unwrap();
        let entry = |title: &str, status: u16| DockResponseEntry {
            request_title: title.to_string().into(),
            response: sample_response(status),
            response_is_html: false,
            pretty_body_editor: editor.clone(),
            raw_body_editor: editor.clone(),
            preview_body_editor: editor.clone(),
            test_results: Vec::new(),
            visualize_data: None,
        };

        dock.update(cx, |dock, cx| {
            let first = dock.begin_send("First".into(), cx);
            let second = dock.begin_send("Second".into(), cx);

            dock.show_response(second, entry("Second", 201), cx);
            dock.show_response(first, entry("First", 500), cx);
            match &dock.display {
                ResponseDockDisplay::Success(shown) => assert_eq!(
                    shown.response.status, 201,
                    "the newest send owns the dock, whoever answers last"
                ),
                _ => panic!("expected the newer response to be on screen"),
            }

            dock.show_error(first, "First".into(), "boom".to_string(), cx);
            assert!(
                matches!(&dock.display, ResponseDockDisplay::Success(shown)
                    if shown.response.status == 201),
                "a stale failure must not wipe the newer reply either"
            );
        });
    }

    /// `show_sending`/`show_error` must also fully replace whatever the dock
    /// was showing, not merge with it -- otherwise a request that errors
    /// after a previous request's successful response would leave stale
    /// success data alongside the new error.
    #[gpui::test]
    fn sending_and_error_states_replace_a_previous_success(cx: &mut TestAppContext) {
        init_test(cx);
        let window = cx.add_window(|_, cx| ResponseDockPanel::new(cx));
        let dock = window.root(cx).unwrap();
        let editor = window
            .update(cx, |_, window, cx| {
                cx.new(|cx| Editor::multi_line(window, cx))
            })
            .unwrap();

        dock.update(cx, |dock, cx| {
            let first = dock.begin_send("First".into(), cx);
            dock.show_response(
                first,
                DockResponseEntry {
                    request_title: "First".into(),
                    response: sample_response(200),
                    response_is_html: false,
                    pretty_body_editor: editor.clone(),
                    raw_body_editor: editor.clone(),
                    preview_body_editor: editor.clone(),
                    test_results: Vec::new(),
                    visualize_data: None,
                },
                cx,
            );
        });

        dock.update(cx, |dock, cx| {
            let second = dock.begin_send("Second".into(), cx);
            dock.show_error(second, "Second".into(), "network error".into(), cx);
        });

        dock.read_with(cx, |dock, _| match &dock.display {
            ResponseDockDisplay::Error {
                request_title,
                message,
            } => {
                assert_eq!(request_title.as_ref(), "Second");
                assert_eq!(message, "network error");
            }
            _ => panic!("expected the dock to be showing the error, not the stale success"),
        });
    }
}
