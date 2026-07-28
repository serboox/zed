use std::collections::HashSet;
use std::time::Duration;

use editor::{Editor, EditorEvent};
use gpui::{
    AnyElement, App, Entity, FocusHandle, Focusable, Hsla, ScrollHandle, SharedString,
    Subscription, Task, Window,
};
use ui::{Divider, WithScrollbar, prelude::*};

use crate::openapi_document::{
    HttpMethod, OpenApiDocument, Operation, OperationGroup, Parameter, RequestBody, Response,
    SchemaSummary, parse,
};

const REPARSE_DEBOUNCE: Duration = Duration::from_millis(200);

/// Above this size the contract is parsed on a background thread. Small files
/// parse in well under a frame, and going through the background executor for
/// them would only make every keystroke's preview update arrive a frame later.
const BACKGROUND_PARSE_THRESHOLD: usize = 128 * 1024;

pub struct OpenApiPreviewView {
    editor: Entity<Editor>,
    focus_handle: FocusHandle,
    scroll_handle: ScrollHandle,
    document: Option<OpenApiDocument>,
    parse_error: Option<SharedString>,
    collapsed_groups: HashSet<SharedString>,
    expanded_operations: HashSet<SharedString>,
    pending_parse: Option<Task<()>>,
    /// Set when an edit arrives while a parse is already running. Without it the
    /// preview would keep showing the text that parse started from, because the
    /// running task captured the buffer before that edit existed.
    reparse_after_pending: bool,
    _editor_subscription: Subscription,
}

impl OpenApiPreviewView {
    pub fn new(editor: Entity<Editor>, window: &mut Window, cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            let subscription = cx.subscribe_in(
                &editor,
                window,
                |this: &mut Self, _, event: &EditorEvent, _, cx| match event {
                    EditorEvent::Edited { .. }
                    | EditorEvent::BufferEdited
                    | EditorEvent::BuffersEdited { .. }
                    | EditorEvent::Reparsed(_) => this.schedule_parse(true, cx),
                    EditorEvent::FileHandleChanged | EditorEvent::Saved => {
                        this.schedule_parse(false, cx)
                    }
                    _ => {}
                },
            );

            let mut this = Self {
                editor,
                focus_handle: cx.focus_handle(),
                scroll_handle: ScrollHandle::new(),
                document: None,
                parse_error: None,
                collapsed_groups: HashSet::default(),
                expanded_operations: HashSet::default(),
                pending_parse: None,
                reparse_after_pending: false,
                _editor_subscription: subscription,
            };
            this.schedule_parse(false, cx);
            this
        })
    }

    pub fn editor(&self) -> &Entity<Editor> {
        &self.editor
    }

    fn schedule_parse(&mut self, debounce: bool, cx: &mut Context<Self>) {
        if debounce && self.pending_parse.is_some() {
            self.reparse_after_pending = true;
            return;
        }
        self.reparse_after_pending = false;
        let text = self.editor.read(cx).text(cx);
        self.pending_parse = Some(cx.spawn(async move |this, cx| {
            if debounce {
                cx.background_executor().timer(REPARSE_DEBOUNCE).await;
            }
            let parsed = if text.len() >= BACKGROUND_PARSE_THRESHOLD {
                cx.background_spawn(async move { parse(&text) }).await
            } else {
                parse(&text)
            };
            this.update(cx, |this, cx| {
                match parsed {
                    Ok(document) => {
                        this.document = Some(document);
                        this.parse_error = None;
                    }
                    // The last good render is kept on screen: a contract is
                    // syntactically broken for most of the time it is being
                    // edited, and blanking the preview on every keystroke
                    // would make it useless while typing.
                    Err(error) => this.parse_error = Some(format!("{error}").into()),
                }
                this.pending_parse = None;
                cx.notify();
                if this.reparse_after_pending {
                    this.schedule_parse(true, cx);
                }
            })
            .ok();
        }));
    }

    fn toggle_group(&mut self, group: SharedString, cx: &mut Context<Self>) {
        if !self.collapsed_groups.remove(&group) {
            self.collapsed_groups.insert(group);
        }
        cx.notify();
    }

    fn toggle_operation(&mut self, operation: SharedString, cx: &mut Context<Self>) {
        if !self.expanded_operations.remove(&operation) {
            self.expanded_operations.insert(operation);
        }
        cx.notify();
    }

    fn render_header(&self, document: &OpenApiDocument, cx: &App) -> impl IntoElement {
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .flex_wrap()
                    .gap_2()
                    .items_center()
                    .child(Headline::new(document.title.clone()).size(HeadlineSize::Large))
                    .when_some(document.version.clone(), |header, version| {
                        header.child(pill(version, cx.theme().colors().element_background, cx))
                    })
                    .child(
                        Label::new(document.spec_label.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .children(document.base_urls.iter().map(|url| {
                h_flex()
                    .gap_1p5()
                    .child(
                        Label::new("Base URL")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(url.clone())
                            .size(LabelSize::Small)
                            .buffer_font(cx),
                    )
            }))
            .when_some(document.description.clone(), |header, description| {
                header.child(Label::new(description).color(Color::Muted))
            })
    }

    fn render_notes(&self, notes: &[SharedString], cx: &App) -> Option<AnyElement> {
        if notes.is_empty() {
            return None;
        }
        Some(
            v_flex()
                .gap_1()
                .p_2()
                .rounded_sm()
                .border_1()
                .border_dashed()
                .border_color(cx.theme().colors().border_variant)
                .children(notes.iter().map(|note| {
                    h_flex()
                        .gap_1p5()
                        .items_start()
                        .child(
                            Icon::new(IconName::Info)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(
                            Label::new(note.clone())
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                }))
                .into_any_element(),
        )
    }

    fn render_group(
        &self,
        group: &OperationGroup,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let collapsed = self.collapsed_groups.contains(&group.name);
        let group_name = group.name.clone();
        let operation_count = group.operations.len();

        v_flex()
            .gap_1()
            .child(
                h_flex()
                    .id(SharedString::from(format!("openapi-group-{group_name}")))
                    .gap_1p5()
                    .py_1()
                    .items_center()
                    .cursor_pointer()
                    .on_click(
                        cx.listener(move |this, _, _, cx| {
                            this.toggle_group(group_name.clone(), cx)
                        }),
                    )
                    .child(
                        Icon::new(if collapsed {
                            IconName::ChevronRight
                        } else {
                            IconName::ChevronDown
                        })
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                    )
                    .child(Headline::new(group.name.clone()).size(HeadlineSize::XSmall))
                    .child(
                        Label::new(format!("{operation_count}"))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .when_some(group.description.clone(), |header, description| {
                        header.child(
                            Label::new(description)
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .truncate(),
                        )
                    }),
            )
            .child(Divider::horizontal())
            .when(!collapsed, |section| {
                section.children(
                    group
                        .operations
                        .iter()
                        .map(|operation| self.render_operation(operation, cx)),
                )
            })
    }

    fn render_operation(
        &self,
        operation: &Operation,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let key = operation.key();
        let expanded = self.expanded_operations.contains(&key);
        let toggle_key = key.clone();
        let (method_background, method_foreground) = method_colors(operation.method, cx);

        v_flex()
            .gap_1()
            .child(
                h_flex()
                    .id(SharedString::from(format!("openapi-operation-{key}")))
                    .gap_2()
                    .py_1()
                    .px_1()
                    .rounded_sm()
                    .items_center()
                    .cursor_pointer()
                    .hover(|style| style.bg(cx.theme().colors().element_hover))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_operation(toggle_key.clone(), cx)
                    }))
                    .child(
                        div()
                            .px_1p5()
                            .py_0p5()
                            .rounded_sm()
                            .bg(method_background)
                            .child(
                                Label::new(operation.method.label())
                                    .size(LabelSize::XSmall)
                                    .weight(gpui::FontWeight::BOLD)
                                    .color(method_foreground),
                            ),
                    )
                    .child(
                        Label::new(operation.path.clone())
                            .buffer_font(cx)
                            .when(operation.deprecated, |label| label.strikethrough()),
                    )
                    .when_some(operation.summary.clone(), |row, summary| {
                        row.child(
                            Label::new(summary)
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .truncate(),
                        )
                    })
                    .when(operation.deprecated, |row| {
                        row.child(
                            Label::new("deprecated")
                                .size(LabelSize::XSmall)
                                .color(Color::Warning),
                        )
                    })
                    .when(operation.secured, |row| {
                        row.child(
                            Icon::new(IconName::Lock)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                    }),
            )
            .when(expanded, |row| {
                row.child(self.render_operation_details(operation, cx))
            })
    }

    fn render_operation_details(
        &self,
        operation: &Operation,
        cx: &App,
    ) -> impl IntoElement + use<> {
        v_flex()
            .gap_2()
            .ml_5()
            .mb_2()
            .pl_2()
            .border_l_1()
            .border_color(cx.theme().colors().border_variant)
            .when_some(operation.description.clone(), |details, description| {
                details.child(Label::new(description).size(LabelSize::Small))
            })
            .when_some(operation.operation_id.clone(), |details, operation_id| {
                details.child(
                    h_flex()
                        .gap_2()
                        .child(
                            Label::new("operationId")
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(
                            Label::new(operation_id)
                                .size(LabelSize::Small)
                                .buffer_font(cx),
                        ),
                )
            })
            .when(!operation.parameters.is_empty(), |details| {
                details.child(section_title("Parameters")).child(
                    v_flex().gap_0p5().children(
                        operation
                            .parameters
                            .iter()
                            .map(|parameter| render_parameter(parameter, cx)),
                    ),
                )
            })
            .when_some(operation.request_body.clone(), |details, body| {
                details
                    .child(section_title("Request body"))
                    .child(render_request_body(&body, cx))
            })
            .when(!operation.responses.is_empty(), |details| {
                details.child(section_title("Responses")).child(
                    v_flex().gap_0p5().children(
                        operation
                            .responses
                            .iter()
                            .map(|response| render_response(response, cx)),
                    ),
                )
            })
    }

    fn render_schemas(&self, schemas: &[SchemaSummary], cx: &App) -> impl IntoElement + use<> {
        v_flex()
            .gap_1()
            .child(Headline::new("Schemas").size(HeadlineSize::XSmall))
            .child(Divider::horizontal())
            .children(schemas.iter().map(|schema| {
                v_flex()
                    .gap_0p5()
                    .py_1()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Label::new(schema.name.clone()).buffer_font(cx))
                            .child(
                                Label::new(schema.type_label.clone())
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                    )
                    .children(schema.properties.iter().map(|(property, type_label)| {
                        h_flex()
                            .gap_2()
                            .ml_4()
                            .child(
                                Label::new(property.clone())
                                    .size(LabelSize::Small)
                                    .buffer_font(cx),
                            )
                            .child(
                                Label::new(type_label.clone())
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                    }))
            }))
    }
}

fn render_parameter(parameter: &Parameter, cx: &App) -> impl IntoElement + use<> {
    h_flex()
        .gap_2()
        .items_start()
        .child(
            Label::new(parameter.name.clone())
                .size(LabelSize::Small)
                .buffer_font(cx),
        )
        .child(
            Label::new(format!("in {}", parameter.location))
                .size(LabelSize::XSmall)
                .color(Color::Muted),
        )
        .child(
            Label::new(parameter.type_label.clone())
                .size(LabelSize::XSmall)
                .color(Color::Muted),
        )
        .when(parameter.required, |row| {
            row.child(
                Label::new("required")
                    .size(LabelSize::XSmall)
                    .color(Color::Error),
            )
        })
        .when_some(parameter.description.clone(), |row, description| {
            row.child(
                Label::new(description)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .truncate(),
            )
        })
}

fn render_request_body(body: &RequestBody, cx: &App) -> impl IntoElement + use<> {
    v_flex()
        .gap_0p5()
        .child(
            h_flex()
                .gap_2()
                .when_some(body.type_label.clone(), |row, type_label| {
                    row.child(
                        Label::new(type_label)
                            .size(LabelSize::Small)
                            .buffer_font(cx),
                    )
                })
                .when(body.required, |row| {
                    row.child(
                        Label::new("required")
                            .size(LabelSize::XSmall)
                            .color(Color::Error),
                    )
                })
                .children(body.content_types.iter().map(|content_type| {
                    Label::new(content_type.clone())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                })),
        )
        .when_some(body.description.clone(), |details, description| {
            details.child(
                Label::new(description)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
        })
}

fn render_response(response: &Response, cx: &App) -> impl IntoElement + use<> {
    h_flex()
        .gap_2()
        .items_start()
        .child(
            Label::new(response.status.clone())
                .size(LabelSize::Small)
                .buffer_font(cx)
                .color(status_color(&response.status)),
        )
        .when_some(response.description.clone(), |row, description| {
            row.child(Label::new(description).size(LabelSize::Small))
        })
        .when_some(response.type_label.clone(), |row, type_label| {
            row.child(
                Label::new(type_label)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
        })
        .children(response.content_types.iter().map(|content_type| {
            Label::new(content_type.clone())
                .size(LabelSize::XSmall)
                .color(Color::Muted)
        }))
}

fn section_title(title: &'static str) -> impl IntoElement {
    Label::new(title)
        .size(LabelSize::XSmall)
        .color(Color::Muted)
        .weight(gpui::FontWeight::SEMIBOLD)
}

fn pill(text: SharedString, background: Hsla, _cx: &App) -> impl IntoElement {
    div()
        .px_1p5()
        .py_0p5()
        .rounded_sm()
        .bg(background)
        .child(Label::new(text).size(LabelSize::Small))
}

fn status_color(status: &str) -> Color {
    match status.chars().next() {
        Some('2') => Color::Success,
        Some('3') => Color::Info,
        Some('4') => Color::Warning,
        Some('5') => Color::Error,
        _ => Color::Muted,
    }
}

fn method_colors(method: HttpMethod, cx: &App) -> (Hsla, Color) {
    let status = cx.theme().status();
    let background = match method {
        HttpMethod::Get | HttpMethod::Head | HttpMethod::Options | HttpMethod::Trace => {
            status.info_background
        }
        HttpMethod::Post => status.created_background,
        HttpMethod::Put | HttpMethod::Patch => status.warning_background,
        HttpMethod::Delete => status.error_background,
    };
    (background, Color::Default)
}

impl Focusable for OpenApiPreviewView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for OpenApiPreviewView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let contents = match self.document.clone() {
            Some(document) => v_flex()
                .gap_4()
                .child(self.render_header(&document, cx))
                .children(self.render_notes(&document.notes, cx))
                .children(
                    document
                        .groups
                        .iter()
                        .map(|group| self.render_group(group, cx).into_any_element()),
                )
                .when(!document.schemas.is_empty(), |body| {
                    body.child(self.render_schemas(&document.schemas, cx))
                })
                .into_any_element(),
            None => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_1()
                .child(
                    Icon::new(IconName::FileCode)
                        .size(IconSize::Medium)
                        .color(Color::Muted),
                )
                .child(
                    Label::new("This file has no readable OpenAPI contract yet")
                        .color(Color::Muted),
                )
                .into_any_element(),
        };

        v_flex()
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .when_some(self.parse_error.clone(), |body, error| {
                body.child(
                    h_flex()
                        .w_full()
                        .gap_1p5()
                        .p_2()
                        .items_start()
                        .bg(cx.theme().status().warning_background)
                        .child(
                            Icon::new(IconName::Warning)
                                .size(IconSize::XSmall)
                                .color(Color::Warning),
                        )
                        .child(
                            v_flex()
                                .gap_0p5()
                                .child(
                                    Label::new("The contract does not parse")
                                        .size(LabelSize::Small),
                                )
                                .child(
                                    Label::new(error)
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted)
                                        .buffer_font(cx),
                                )
                                .child(
                                    Label::new("Showing the last version that parsed.")
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                ),
                        ),
                )
            })
            .child(
                div()
                    .id("openapi-preview-scroll")
                    .track_focus(&self.focus_handle)
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll_handle)
                    .p_4()
                    .child(contents)
                    .vertical_scrollbar_for(&self.scroll_handle, window, cx),
            )
    }
}
