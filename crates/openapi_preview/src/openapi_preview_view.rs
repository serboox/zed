use std::collections::{HashMap, HashSet};
use std::time::Duration;

use api_client::FolderId;
use api_client_ui::ApiClientStore;
use editor::{Editor, EditorEvent};
use gpui::{
    AnyElement, App, Entity, FocusHandle, Focusable, Hsla, ScrollHandle, SharedString,
    Subscription, Task, Window,
};
use ui::{Tooltip, WithScrollbar, prelude::*};
use workspace::{Toast, Workspace, notifications::NotificationId};

use crate::api_collection::{ImportedCollection, OperationSelection, collection_from_document};
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
    expanded_schemas: HashSet<SharedString>,
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
                expanded_schemas: HashSet::default(),
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

    fn toggle_schema(&mut self, schema: SharedString, cx: &mut Context<Self>) {
        if !self.expanded_schemas.remove(&schema) {
            self.expanded_schemas.insert(schema);
        }
        cx.notify();
    }

    fn save_document_to_api_client(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.save_operations_to_api_client(OperationSelection::AllOperations, window, cx);
    }

    fn save_operation_to_api_client(
        &mut self,
        operation_key: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.save_operations_to_api_client(
            OperationSelection::SingleOperation(operation_key),
            window,
            cx,
        );
    }

    fn save_operations_to_api_client(
        &mut self,
        selection: OperationSelection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(document) = self.document.clone() else {
            return;
        };
        let Some(workspace) = Workspace::for_window(window, cx) else {
            return;
        };

        let Some(store) = ApiClientStore::global(cx) else {
            workspace.update(cx, |workspace, cx| {
                workspace.show_toast(
                    Toast::new(
                        NotificationId::named("openapi-save-to-api-client-unavailable".into()),
                        "The API Client isn't available, so this couldn't be saved.",
                    ),
                    cx,
                );
            });
            return;
        };

        let imported = collection_from_document(&document, selection);
        let requested = imported.requests.len();
        let collection_name = imported.collection.name.clone();
        let added = apply_import_to_store(&store, imported, cx);

        workspace.update(cx, |workspace, cx| {
            let message = if requested == 0 {
                format!("Nothing to save to \"{collection_name}\".")
            } else if added == 0 {
                format!("\"{collection_name}\" already has every selected request saved.")
            } else if added == requested {
                let suffix = if added == 1 { "" } else { "s" };
                format!("Saved {added} request{suffix} to \"{collection_name}\".")
            } else {
                let suffix = if added == 1 { "" } else { "s" };
                format!(
                    "Saved {added} new request{suffix} to \"{collection_name}\" ({} already saved).",
                    requested - added
                )
            };
            workspace.show_toast(
                Toast::new(NotificationId::named("openapi-save-to-api-client".into()), message),
                cx,
            );
        });
    }

    fn render_header(
        &self,
        document: &OpenApiDocument,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        v_flex()
            .debug_selector(|| "openapi-header".to_string())
            .gap_1p5()
            .p_3()
            .rounded_lg()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().surface_background)
            .child(
                h_flex()
                    .flex_wrap()
                    .gap_2()
                    .items_center()
                    .child(
                        Label::new(document.title.clone())
                            .size(LabelSize::Custom(rems(1.5)))
                            .weight(gpui::FontWeight::BOLD),
                    )
                    .when_some(document.version.clone(), |header, version| {
                        header.child(pill(
                            version,
                            cx.theme().colors().element_background,
                            Color::Default,
                            cx,
                        ))
                    })
                    .child(pill(
                        document.spec_label.clone(),
                        cx.theme().colors().border_variant,
                        Color::Muted,
                        cx,
                    ))
                    .child(
                        Button::new("openapi-save-document", "Save to API Client")
                            .start_icon(Icon::new(IconName::Bookmark))
                            .style(ButtonStyle::Subtle)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.save_document_to_api_client(window, cx)
                            })),
                    ),
            )
            .when(!document.base_urls.is_empty(), |header| {
                header.child(
                    v_flex()
                        .gap_0p5()
                        .children(document.base_urls.iter().map(|url| {
                            Label::new(url.clone())
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .buffer_font(cx)
                        })),
                )
            })
            .when_some(document.description.clone(), |header, description| {
                header.child(
                    Label::new(description)
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
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
            .debug_selector(|| format!("openapi-tag-card-{}", group.name))
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().colors().border)
            .overflow_hidden()
            .child(
                h_flex()
                    .id(SharedString::from(format!("openapi-group-{group_name}")))
                    .w_full()
                    .gap_2()
                    .items_center()
                    .px_3()
                    .py_2()
                    .cursor_pointer()
                    .bg(cx.theme().colors().surface_background)
                    .hover(|style| style.bg(cx.theme().colors().element_hover))
                    .on_click(
                        cx.listener(move |this, _, _, cx| {
                            this.toggle_group(group_name.clone(), cx)
                        }),
                    )
                    .when(!collapsed, |header| {
                        header.border_b_1().border_color(cx.theme().colors().border)
                    })
                    .child(
                        Label::new(group.name.clone())
                            .size(LabelSize::Custom(rems(1.125)))
                            .weight(gpui::FontWeight::SEMIBOLD)
                            .flex_none(),
                    )
                    .child(div().flex_1().min_w_0().when_some(
                        group.description.clone(),
                        |container, description| {
                            container.child(
                                Label::new(description)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted)
                                    .truncate(),
                            )
                        },
                    ))
                    .child(
                        Label::new(format!("{operation_count}"))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        Icon::new(if collapsed {
                            IconName::ChevronRight
                        } else {
                            IconName::ChevronDown
                        })
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                    ),
            )
            .when(!collapsed, |card| {
                card.child(
                    v_flex().gap_2().p_2().children(
                        group
                            .operations
                            .iter()
                            .map(|operation| self.render_operation(operation, cx)),
                    ),
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
        let accent = method_accent_color(method_accent(operation.method), cx);

        v_flex()
            .debug_selector(|| format!("openapi-operation-row-{key}"))
            .w_full()
            .rounded_md()
            .border_1()
            .border_color(accent)
            .bg(accent.opacity(0.08))
            .child(
                h_flex()
                    .id(SharedString::from(format!("openapi-operation-{key}")))
                    .w_full()
                    .gap_2()
                    .items_center()
                    .px_2()
                    .py_1p5()
                    .cursor_pointer()
                    .hover(|style| style.bg(accent.opacity(0.16)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_operation(toggle_key.clone(), cx)
                    }))
                    .child(
                        h_flex()
                            .flex_none()
                            .w(rems(4.5))
                            .justify_center()
                            .py_0p5()
                            .rounded_sm()
                            .bg(accent)
                            .child(
                                Label::new(operation.method.label())
                                    .size(LabelSize::XSmall)
                                    .weight(gpui::FontWeight::BOLD)
                                    .color(Color::Custom(badge_foreground(accent))),
                            ),
                    )
                    .child(
                        div().flex_shrink(1.).min_w_0().child(
                            Label::new(operation.path.clone())
                                .buffer_font(cx)
                                .weight(gpui::FontWeight::BOLD)
                                .truncate()
                                .when(operation.deprecated, |label| label.strikethrough()),
                        ),
                    )
                    .child(div().flex_1().min_w_0().when_some(
                        operation.summary.clone(),
                        |container, summary| {
                            container.child(
                                Label::new(summary)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted)
                                    .truncate(),
                            )
                        },
                    ))
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
                    })
                    .child(
                        IconButton::new(
                            SharedString::from(format!("openapi-save-operation-{key}")),
                            IconName::Bookmark,
                        )
                        .icon_size(IconSize::XSmall)
                        .tooltip(Tooltip::text("Save to API Client"))
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.save_operation_to_api_client(key.clone(), window, cx)
                            },
                        )),
                    ),
            )
            .when(expanded, |card| {
                card.child(
                    div()
                        .px_2()
                        .pb_2()
                        .child(self.render_operation_details(operation, cx)),
                )
            })
    }

    fn render_operation_details(
        &self,
        operation: &Operation,
        cx: &App,
    ) -> impl IntoElement + use<> {
        let key = operation.key();

        v_flex()
            .gap_2()
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
                details
                    .child(section_title("Parameters"))
                    .child(render_parameters_table(&operation.parameters, &key, cx))
            })
            .when_some(operation.request_body.clone(), |details, body| {
                details
                    .child(section_title("Request body"))
                    .child(render_request_body(&body, cx))
            })
            .when(!operation.responses.is_empty(), |details| {
                details
                    .child(section_title("Responses"))
                    .child(render_responses_table(&operation.responses, &key, cx))
            })
    }

    fn render_schemas(
        &self,
        schemas: &[SchemaSummary],
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        v_flex()
            .debug_selector(|| "openapi-schemas".to_string())
            .gap_2()
            .p_3()
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().surface_background)
            .child(
                Label::new("Schemas")
                    .size(LabelSize::Custom(rems(1.125)))
                    .weight(gpui::FontWeight::SEMIBOLD),
            )
            .children(schemas.iter().map(|schema| self.render_schema(schema, cx)))
    }

    fn render_schema(
        &self,
        schema: &SchemaSummary,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let expanded = self.expanded_schemas.contains(&schema.name);
        let schema_name = schema.name.clone();
        let toggle_name = schema.name.clone();

        v_flex()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                h_flex()
                    .id(SharedString::from(format!("openapi-schema-{schema_name}")))
                    .w_full()
                    .gap_2()
                    .items_center()
                    .px_2()
                    .py_1()
                    .cursor_pointer()
                    .hover(|style| style.bg(cx.theme().colors().element_hover))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_schema(toggle_name.clone(), cx)
                    }))
                    .child(
                        Icon::new(if expanded {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                    )
                    .child(
                        Label::new(schema.name.clone())
                            .buffer_font(cx)
                            .weight(gpui::FontWeight::BOLD),
                    )
                    .child(
                        Label::new(schema.type_label.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .when(expanded, |card| {
                card.child(
                    v_flex()
                        .gap_0p5()
                        .px_2()
                        .pb_2()
                        .children(schema.properties.iter().map(|(property, type_label)| {
                            h_flex()
                                .gap_1()
                                .child(Label::new(format!("{property}:")).buffer_font(cx))
                                .child(
                                    Label::new(type_label.clone())
                                        .buffer_font(cx)
                                        .color(Color::Muted),
                                )
                        })),
                )
            })
    }
}

/// Adds `imported` to the API client store: a brand-new collection when none
/// by this name exists yet, otherwise its folders/requests are merged into
/// the existing one (matched by name) so saving the same document twice
/// never creates a second collection. A request already present under the
/// same collection, folder, method, and URL is left alone rather than
/// duplicated. Returns how many requests were newly added.
fn apply_import_to_store(
    store: &Entity<ApiClientStore>,
    imported: ImportedCollection,
    cx: &mut Context<OpenApiPreviewView>,
) -> usize {
    store.update(cx, |store, cx| {
        let existing_collection_id = store
            .collections
            .iter()
            .find(|collection| collection.name == imported.collection.name)
            .map(|collection| collection.id);

        let Some(collection_id) = existing_collection_id else {
            let added = imported.requests.len();
            store.import_collection(imported.collection, imported.folders, imported.requests, cx);
            return added;
        };

        store.update_collection(collection_id, cx, |collection| {
            for variable in &imported.collection.variables {
                if !collection
                    .variables
                    .iter()
                    .any(|existing| existing.key == variable.key)
                {
                    collection.variables.push(variable.clone());
                }
            }
        });

        let mut folder_id_map: HashMap<FolderId, FolderId> = HashMap::new();
        for folder in &imported.folders {
            let existing_folder_id = store
                .folders
                .iter()
                .find(|existing| {
                    existing.collection_id == collection_id
                        && existing.parent_id.is_none()
                        && existing.name == folder.name
                })
                .map(|existing| existing.id);
            let target_id = match existing_folder_id {
                Some(id) => Some(id),
                None => store.create_folder(collection_id, folder.name.clone(), None, cx),
            };
            if let Some(target_id) = target_id {
                folder_id_map.insert(folder.id, target_id);
            }
        }

        let mut added = 0;
        for request in imported.requests {
            let folder_id = request
                .folder_id
                .and_then(|source_folder_id| folder_id_map.get(&source_folder_id).copied());
            let already_saved = store.requests.iter().any(|existing| {
                existing.collection_id == collection_id
                    && existing.folder_id == folder_id
                    && existing.method == request.method
                    && existing.url == request.url
            });
            if already_saved {
                continue;
            }
            let request_id = store.create_request(collection_id, request.name, folder_id, cx);
            store.update_request(request_id, cx, |stored| {
                stored.description = request.description;
                stored.method = request.method;
                stored.url = request.url;
                stored.params = request.params;
                stored.headers = request.headers;
                stored.body = request.body;
            });
            added += 1;
        }
        added
    })
}

fn render_request_body(body: &RequestBody, cx: &App) -> impl IntoElement + use<> {
    v_flex()
        .gap_1()
        .p_2()
        .rounded_md()
        .border_1()
        .border_color(cx.theme().colors().border_variant)
        .child(
            h_flex()
                .flex_wrap()
                .gap_2()
                .items_center()
                .children(body.content_types.iter().map(|content_type| {
                    Label::new(content_type.clone())
                        .size(LabelSize::Small)
                        .buffer_font(cx)
                }))
                .when(body.required, |row| {
                    row.child(
                        Label::new("required")
                            .size(LabelSize::XSmall)
                            .color(Color::Error),
                    )
                })
                .when_some(body.type_label.clone(), |row, type_label| {
                    row.child(
                        Label::new(type_label)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .buffer_font(cx),
                    )
                }),
        )
        .when_some(body.description.clone(), |details, description| {
            details.child(
                Label::new(description)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
        })
}

fn render_parameters_table(
    parameters: &[Parameter],
    operation_key: &SharedString,
    cx: &App,
) -> impl IntoElement + use<> {
    v_flex()
        .debug_selector(|| format!("openapi-parameters-table-{operation_key}"))
        .rounded_md()
        .border_1()
        .border_color(cx.theme().colors().border_variant)
        .overflow_hidden()
        .child(parameters_table_header(cx))
        .children(parameters.iter().enumerate().map(|(index, parameter)| {
            h_flex()
                .gap_3()
                .items_start()
                .px_2()
                .py_1p5()
                .when(index > 0, |row| {
                    row.border_t_1()
                        .border_color(cx.theme().colors().border_variant)
                })
                .child(
                    v_flex()
                        .w_1_3()
                        .flex_none()
                        .gap_0p5()
                        .child(
                            h_flex()
                                .gap_0p5()
                                .child(
                                    Label::new(parameter.name.clone())
                                        .buffer_font(cx)
                                        .weight(gpui::FontWeight::BOLD),
                                )
                                .when(parameter.required, |row| {
                                    row.child(
                                        Label::new("*")
                                            .weight(gpui::FontWeight::BOLD)
                                            .color(Color::Error),
                                    )
                                }),
                        )
                        .child(
                            h_flex()
                                .gap_1()
                                .child(
                                    Label::new(parameter.type_label.clone())
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted)
                                        .italic(),
                                )
                                .child(
                                    Label::new(parameter.location.clone())
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                ),
                        ),
                )
                .child(div().flex_1().min_w_0().when_some(
                    parameter.description.clone(),
                    |container, description| {
                        container.child(
                            Label::new(description)
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    },
                ))
        }))
}

fn parameters_table_header(cx: &App) -> impl IntoElement {
    h_flex()
        .gap_3()
        .px_2()
        .py_1()
        .border_b_1()
        .border_color(cx.theme().colors().border_variant)
        .bg(cx.theme().colors().surface_background)
        .child(
            div().w_1_3().flex_none().child(
                Label::new("Name")
                    .size(LabelSize::XSmall)
                    .weight(gpui::FontWeight::SEMIBOLD)
                    .color(Color::Muted),
            ),
        )
        .child(
            div().flex_1().min_w_0().child(
                Label::new("Description")
                    .size(LabelSize::XSmall)
                    .weight(gpui::FontWeight::SEMIBOLD)
                    .color(Color::Muted),
            ),
        )
}

fn render_responses_table(
    responses: &[Response],
    operation_key: &SharedString,
    cx: &App,
) -> impl IntoElement + use<> {
    v_flex()
        .debug_selector(|| format!("openapi-responses-table-{operation_key}"))
        .rounded_md()
        .border_1()
        .border_color(cx.theme().colors().border_variant)
        .overflow_hidden()
        .child(responses_table_header(cx))
        .children(responses.iter().enumerate().map(|(index, response)| {
            h_flex()
                .gap_3()
                .items_start()
                .px_2()
                .py_1p5()
                .when(index > 0, |row| {
                    row.border_t_1()
                        .border_color(cx.theme().colors().border_variant)
                })
                .child(
                    div().w_1_5().flex_none().child(
                        Label::new(response.status.clone())
                            .buffer_font(cx)
                            .weight(gpui::FontWeight::BOLD)
                            .color(status_color(&response.status)),
                    ),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_0p5()
                        .when_some(response.description.clone(), |details, description| {
                            details.child(Label::new(description).size(LabelSize::Small))
                        })
                        .when_some(response.type_label.clone(), |details, type_label| {
                            details.child(
                                Label::new(type_label)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .buffer_font(cx),
                            )
                        })
                        .children(response.content_types.iter().map(|content_type| {
                            Label::new(content_type.clone())
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                        })),
                )
        }))
}

fn responses_table_header(cx: &App) -> impl IntoElement {
    h_flex()
        .gap_3()
        .px_2()
        .py_1()
        .border_b_1()
        .border_color(cx.theme().colors().border_variant)
        .bg(cx.theme().colors().surface_background)
        .child(
            div().w_1_5().flex_none().child(
                Label::new("Code")
                    .size(LabelSize::XSmall)
                    .weight(gpui::FontWeight::SEMIBOLD)
                    .color(Color::Muted),
            ),
        )
        .child(
            div().flex_1().min_w_0().child(
                Label::new("Description")
                    .size(LabelSize::XSmall)
                    .weight(gpui::FontWeight::SEMIBOLD)
                    .color(Color::Muted),
            ),
        )
}

fn section_title(title: &'static str) -> impl IntoElement {
    Label::new(title.to_uppercase())
        .size(LabelSize::XSmall)
        .color(Color::Muted)
        .weight(gpui::FontWeight::SEMIBOLD)
}

fn pill(text: SharedString, background: Hsla, text_color: Color, _cx: &App) -> impl IntoElement {
    div()
        .flex_none()
        .px_1p5()
        .py_0p5()
        .rounded_full()
        .bg(background)
        .child(Label::new(text).size(LabelSize::Small).color(text_color))
}

/// Success in green, redirects and client errors in the same amber (the theme's
/// status palette has no distinct orange), server errors in red.
fn status_color(status: &str) -> Color {
    match status.chars().next() {
        Some('2') => Color::Success,
        Some('3') | Some('4') => Color::Warning,
        Some('5') => Color::Error,
        _ => Color::Muted,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MethodAccent {
    Info,
    Created,
    Warning,
    Error,
}

/// Idempotent reads in blue, creation in green, mutation in amber, deletion in
/// red -- the colouring a reader of a contract reference page expects.
fn method_accent(method: HttpMethod) -> MethodAccent {
    match method {
        HttpMethod::Get | HttpMethod::Head | HttpMethod::Options | HttpMethod::Trace => {
            MethodAccent::Info
        }
        HttpMethod::Post => MethodAccent::Created,
        HttpMethod::Put | HttpMethod::Patch => MethodAccent::Warning,
        HttpMethod::Delete => MethodAccent::Error,
    }
}

/// Text laid over a solid accent fill. The status palette a theme picks is free
/// to be light -- amber in a light theme is -- so the foreground follows the
/// fill instead of always being white.
fn badge_foreground(fill: Hsla) -> Hsla {
    if fill.l > 0.6 {
        gpui::hsla(0., 0., 0.12, 1.)
    } else {
        gpui::white()
    }
}

fn method_accent_color(accent: MethodAccent, cx: &App) -> Hsla {
    let status = cx.theme().status();
    match accent {
        MethodAccent::Info => status.info,
        MethodAccent::Created => status.created,
        MethodAccent::Warning => status.warning,
        MethodAccent::Error => status.error,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_color_follows_the_status_family() {
        assert_eq!(status_color("200"), Color::Success);
        assert_eq!(status_color("201"), Color::Success);
        assert_eq!(status_color("301"), Color::Warning);
        assert_eq!(status_color("404"), Color::Warning);
        assert_eq!(status_color("500"), Color::Error);
        assert_eq!(status_color("default"), Color::Muted);
    }

    #[test]
    fn method_accent_groups_verbs_by_what_they_do() {
        assert_eq!(method_accent(HttpMethod::Get), MethodAccent::Info);
        assert_eq!(method_accent(HttpMethod::Head), MethodAccent::Info);
        assert_eq!(method_accent(HttpMethod::Options), MethodAccent::Info);
        assert_eq!(method_accent(HttpMethod::Trace), MethodAccent::Info);
        assert_eq!(method_accent(HttpMethod::Post), MethodAccent::Created);
        assert_eq!(method_accent(HttpMethod::Put), MethodAccent::Warning);
        assert_eq!(method_accent(HttpMethod::Patch), MethodAccent::Warning);
        assert_eq!(method_accent(HttpMethod::Delete), MethodAccent::Error);
    }

    #[test]
    fn badge_text_stays_legible_on_light_and_dark_fills() {
        let on_light = badge_foreground(gpui::hsla(0.12, 0.9, 0.72, 1.));
        let on_dark = badge_foreground(gpui::hsla(0.6, 0.8, 0.35, 1.));
        assert!(
            on_light.l < 0.3,
            "a light fill needs dark text, got lightness {}",
            on_light.l
        );
        assert!(
            on_dark.l > 0.7,
            "a dark fill needs light text, got lightness {}",
            on_dark.l
        );
    }
}
