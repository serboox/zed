use crate::response_view::{ResponseData, ResponseTab, format_size};
use api_client::TestResult;
use editor::Editor;
use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    ParentElement, Render, ScrollHandle, SharedString, Styled, Window, div,
};
use terminal_view::terminal_panel::TerminalPanel;
use ui::{IconName, Label, LabelSize, ScrollAxes, Scrollbars, WithScrollbar, prelude::*};
use workspace::{Item, ItemHandle as _, Workspace, dock::Panel as _};

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

/// A tab carries no events of its own; the enum exists because `Item` needs one.
pub enum ResponseTabEvent {}

impl EventEmitter<ResponseTabEvent> for ResponseDockPanel {}

impl Item for ResponseDockPanel {
    type Event = ResponseTabEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "API Response".into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<ui::Icon> {
        Some(ui::Icon::new(IconName::ReplyArrowRight))
    }
}

/// The tab every reply lands in, opened beside the terminals if it is not there
/// yet, activated and revealed. One tab for all requests: a later reply replaces
/// what the tab shows rather than stacking another tab beside it.
pub fn reveal_response_tab(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<Entity<ResponseDockPanel>> {
    let pane = workspace
        .panel::<TerminalPanel>(cx)
        .and_then(|panel| panel.read(cx).pane())?;
    let existing = pane.read(cx).items_of_type::<ResponseDockPanel>().next();
    let tab = match existing {
        Some(tab) => tab,
        None => {
            let tab = cx.new(ResponseDockPanel::new);
            pane.update(cx, |pane, cx| {
                pane.add_item(Box::new(tab.clone()), false, false, None, window, cx);
            });
            tab
        }
    };
    let index = pane
        .read(cx)
        .items()
        .position(|item| item.item_id() == tab.item_id());
    if let Some(index) = index {
        pane.update(cx, |pane, cx| {
            // Activated but not focused: a reply must not take the caret out of
            // the request being edited.
            pane.activate_item(index, false, false, window, cx);
        });
    }
    // Revealed rather than merely opened: a zoomed item elsewhere would leave the
    // tab activated but hidden behind it.
    workspace.reveal_panel::<TerminalPanel>(window, cx);
    Some(tab)
}

/// Opens the response tab and puts the caret in it, for the reader who asked for
/// it by its own shortcut rather than by sending something.
pub fn focus_response_tab(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if let Some(tab) = reveal_response_tab(workspace, window, cx) {
        let handle = tab.read(cx).focus_handle.clone();
        window.focus(&handle, cx);
    }
}

/// The response tab if it is already open, without opening one.
pub fn existing_response_tab(
    workspace: &Workspace,
    cx: &App,
) -> Option<Entity<ResponseDockPanel>> {
    workspace
        .panel::<TerminalPanel>(cx)?
        .read(cx)
        .pane()?
        .read(cx)
        .items_of_type::<ResponseDockPanel>()
        .next()
}

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


    /// Switching tabs starts at the top: the offset left over from the tab
    /// before it belongs to content that is no longer on screen, and would hide
    /// the first lines of what is.
    fn show_tab(&mut self, tab: ResponseTab, cx: &mut Context<Self>) {
        self.response_tab = tab;
        self.scroll_handle.set_offset(gpui::Point::default());
        cx.notify();
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
        let response_tab = self.response_tab;
        // A reply's body is shown in an editor, which scrolls itself; every other
        // tab is a plain list that needs this view to scroll it.
        let body_scrolls_itself = matches!(self.display, ResponseDockDisplay::Success(_))
            && matches!(
                response_tab,
                ResponseTab::Pretty | ResponseTab::Preview | ResponseTab::Raw
            );

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
                        this.show_tab(ResponseTab::Pretty, cx);
                    },
                ));
                if entry.response_is_html {
                    tab_strip = tab_strip.child(Self::render_tab_chip(
                        "Preview",
                        response_tab == ResponseTab::Preview,
                        cx,
                        |this, _, _, cx| {
                            this.show_tab(ResponseTab::Preview, cx);
                        },
                    ));
                }
                tab_strip = tab_strip
                    .child(Self::render_tab_chip(
                        "Raw",
                        response_tab == ResponseTab::Raw,
                        cx,
                        |this, _, _, cx| {
                            this.show_tab(ResponseTab::Raw, cx);
                        },
                    ))
                    .child(Self::render_tab_chip(
                        "Headers",
                        response_tab == ResponseTab::Headers,
                        cx,
                        |this, _, _, cx| {
                            this.show_tab(ResponseTab::Headers, cx);
                        },
                    ))
                    .child(Self::render_tab_chip(
                        "Cookies",
                        response_tab == ResponseTab::Cookies,
                        cx,
                        |this, _, _, cx| {
                            this.show_tab(ResponseTab::Cookies, cx);
                        },
                    ));
                if !entry.test_results.is_empty() {
                    tab_strip = tab_strip.child(Self::render_tab_chip(
                        "Test Results",
                        response_tab == ResponseTab::TestResults,
                        cx,
                        |this, _, _, cx| {
                            this.show_tab(ResponseTab::TestResults, cx);
                        },
                    ));
                }
                if entry.visualize_data.is_some() {
                    tab_strip = tab_strip.child(Self::render_tab_chip(
                        "Visualize",
                        response_tab == ResponseTab::Visualize,
                        cx,
                        |this, _, _, cx| {
                            this.show_tab(ResponseTab::Visualize, cx);
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
                        // The editor fills what is left and scrolls itself. Sizing
                        // this box to a guess at the text's height instead would
                        // leave the editor clipped and scrolling inside a box that
                        // barely scrolls, so the visible thumb would crawl while
                        // the body ran past the end.
                        column
                            .min_h_0()
                            .child(
                                div()
                                    .flex_1()
                                    .min_h_0()
                                    // A dock dragged down to almost nothing must
                                    // still show some of the reply rather than
                                    // collapsing it away entirely.
                                    .min_h(px(80.))
                                    .debug_selector(|| "api-response-body".to_string())
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

                let column = v_flex()
                    .gap_2()
                    .child(summary_row)
                    .child(tab_strip)
                    .child(body);
                // Held to the tab's height only when the body inside scrolls
                // itself. A list has no scroller of its own, so it must be free
                // to be taller than the tab -- that overflow is what gives this
                // view something to scroll.
                if body_scrolls_itself {
                    column.flex_1().min_h_0().into_any_element()
                } else {
                    column.into_any_element()
                }
            }
        };

        let shell = v_flex()
            .id("api-response-dock")
            .debug_selector(|| "api-response-dock".to_string())
            .track_focus(&self.focus_handle)
            .size_full()
            .p_2()
            .gap_2();

        // Only one thing scrolls at a time. A body shown in an editor scrolls
        // itself, and its own scrollbar is the one that tracks the reply; the
        // lists (headers, cookies, test results) have no scroller of their own,
        // so this container is theirs.
        let shell = shell
            .min_h_0()
            .overflow_scroll()
            .track_scroll(&self.scroll_handle)
            .child(content);
        if body_scrolls_itself {
            shell.into_any_element()
        } else {
            shell
                .custom_scrollbars(
                    Scrollbars::always_visible(ScrollAxes::Vertical)
                        .tracked_scroll_handle(&self.scroll_handle),
                    window,
                    cx,
                )
                .into_any_element()
        }
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

    /// A long body must be shown in a box that fills the tab, so the editor's own
    /// scrollbar tracks the whole reply. Sizing that box to a guess at the text's
    /// height instead left the editor clipped and scrolling inside a container
    /// that barely scrolled -- reaching the end of the body moved the visible
    /// thumb a few percent.
    #[gpui::test]
    fn a_long_body_fills_the_tab_instead_of_a_guessed_height(cx: &mut TestAppContext) {
        init_test(cx);
        let window = cx.add_window(|_, cx| ResponseDockPanel::new(cx));
        let dock = window.root(cx).unwrap();
        let long_json: String = std::iter::repeat_n("  \"key\": \"value\",\n", 400).collect();
        let editor = window
            .update(cx, |_, window, cx| {
                cx.new(|cx| {
                    let mut editor = Editor::multi_line(window, cx);
                    editor.set_text(long_json, window, cx);
                    editor
                })
            })
            .unwrap();

        dock.update(cx, |dock, cx| {
            let generation = dock.begin_send("Long".into(), cx);
            dock.show_response(
                generation,
                DockResponseEntry {
                    request_title: "Long".into(),
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

        let cx = &mut gpui::VisualTestContext::from_window(window.into(), cx);
        cx.run_until_parked();

        let dock_bounds = cx
            .debug_bounds("api-response-dock")
            .expect("the dock paints");
        let body_bounds = cx
            .debug_bounds("api-response-body")
            .expect("the body box paints");
        assert!(
            body_bounds.size.height <= dock_bounds.size.height,
            "the body must not paint taller than the tab it lives in: {:?} vs {:?}",
            body_bounds.size.height,
            dock_bounds.size.height
        );
        assert!(
            body_bounds.size.height > dock_bounds.size.height * 0.5,
            "the body has to take the room left over, got {:?} of {:?}",
            body_bounds.size.height,
            dock_bounds.size.height
        );
        // Nothing for this view to scroll: the editor owns it, so its own
        // scrollbar is the one that tracks the reply.
        assert_eq!(
            dock.read_with(cx, |dock, _| dock.scroll_handle.max_offset().y),
            gpui::px(0.),
            "the container must not offer a second, near-still scrollbar"
        );
    }

    /// A list has no scroller of its own, so the tab must stay scrollable for it:
    /// holding the content to the tab's height would leave the visible scrollbar
    /// with nothing to travel over and the last headers unreachable.
    #[gpui::test]
    fn a_long_headers_list_keeps_the_tab_scrollable(cx: &mut TestAppContext) {
        init_test(cx);
        let window = cx.add_window(|_, cx| ResponseDockPanel::new(cx));
        let dock = window.root(cx).unwrap();
        let editor = window
            .update(cx, |_, window, cx| {
                cx.new(|cx| Editor::multi_line(window, cx))
            })
            .unwrap();
        let mut response = sample_response(200);
        response.headers = (0..120)
            .map(|index| (format!("x-header-{index}"), format!("value {index}")))
            .collect();

        dock.update(cx, |dock, cx| {
            let generation = dock.begin_send("Headers".into(), cx);
            dock.show_response(
                generation,
                DockResponseEntry {
                    request_title: "Headers".into(),
                    response,
                    response_is_html: false,
                    pretty_body_editor: editor.clone(),
                    raw_body_editor: editor.clone(),
                    preview_body_editor: editor.clone(),
                    test_results: Vec::new(),
                    visualize_data: None,
                },
                cx,
            );
            dock.show_tab(ResponseTab::Headers, cx);
        });

        let cx = &mut gpui::VisualTestContext::from_window(window.into(), cx);
        cx.run_until_parked();

        assert!(
            dock.read_with(cx, |dock, _| dock.scroll_handle.max_offset().y) > gpui::px(0.),
            "a list taller than the tab has to leave this view something to scroll"
        );
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
