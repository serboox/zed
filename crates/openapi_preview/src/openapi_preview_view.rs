use std::collections::{HashMap, HashSet};
use std::time::Duration;

use api_client::FolderId;
use api_client_ui::ApiClientStore;
use editor::{Editor, EditorEvent};
use gpui::{
    AnyElement, App, Entity, FocusHandle, Focusable, Hsla, ScrollHandle, SharedString,
    Subscription, Task, Window,
};
use ui::{ContextMenu, DropdownMenu, Tooltip, WithScrollbar, prelude::*};
use workspace::preview_appearance::{
    PreviewAppearance, preview_appearance, set_preview_appearance,
};
use workspace::{Toast, Workspace, notifications::NotificationId};

use crate::api_collection::{ImportedCollection, OperationSelection, collection_from_document};
use crate::openapi_document::{
    HttpMethod, OpenApiDocument, Operation, OperationGroup, Parameter, ProseSpan, RequestBody,
    Response, SchemaSummary, available_values_label, parse, resolve_selected_server,
    split_code_spans,
};
use crate::palette::{Palette, resolve_theme};
use crate::try_it_out;

const REPARSE_DEBOUNCE: Duration = Duration::from_millis(200);

/// How visible the floating reading controls are at rest, matching the
/// Markdown preview's own floating control.
const RESTING_CONTROL_OPACITY: f32 = 0.35;

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
    try_it_out_panels: HashMap<SharedString, TryItOutPanel>,
    /// The server the reader picked from the header's servers dropdown. `None`
    /// means "use the document's default" -- `resolve_selected_server` reads
    /// this the same way whether it holds a pick or not.
    selected_server_url: Option<SharedString>,
    pending_parse: Option<Task<()>>,
    /// Set when an edit arrives while a parse is already running. Without it the
    /// preview would keep showing the text that parse started from, because the
    /// running task captured the buffer before that edit existed.
    reparse_after_pending: bool,
    /// The palette the reader picked to read this contract in, independent of
    /// the editor's own theme.
    reading_appearance: PreviewAppearance,
    _editor_subscription: Subscription,
}

/// What a single parameter field in a "Try it out" panel lets the reader fill
/// in: free text for most parameters, or a dropdown limited to the values an
/// `enum` schema declares.
enum ParameterInput {
    Text(Entity<Editor>),
    Select {
        allowed_values: Vec<SharedString>,
        selected: Option<SharedString>,
    },
}

/// One parameter field in a "Try it out" panel: the reader's typed-in value
/// for a single query/path/header parameter the operation declares (cookie
/// parameters are skipped, matching `api_collection::build_request`).
struct ParameterField {
    name: SharedString,
    location: SharedString,
    required: bool,
    input: ParameterInput,
}

enum TryItOutSendState {
    Idle,
    Sending,
    Success(TryItOutResponseMeta),
    Error(SharedString),
}

struct TryItOutResponseMeta {
    status: u16,
    status_text: SharedString,
    elapsed_ms: u64,
    size_bytes: usize,
    headers: Vec<(String, String)>,
    body_truncated: bool,
}

/// The "Try it out" panel for a single expanded operation: everything the
/// reader can fill in, plus the outcome of the last send.
struct TryItOutPanel {
    server_editor: Entity<Editor>,
    /// Held only in this editor's in-memory buffer for as long as the panel
    /// stays open. Read once per send to fill in the `Authorization` header
    /// of the outgoing request -- never written to a collection, the
    /// workspace database, or a log.
    auth_editor: Entity<Editor>,
    parameter_fields: Vec<ParameterField>,
    body_editor: Option<Entity<Editor>>,
    response_body_editor: Entity<Editor>,
    headers_expanded: bool,
    send_state: TryItOutSendState,
    _send_task: Option<Task<()>>,
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
                try_it_out_panels: HashMap::default(),
                selected_server_url: None,
                pending_parse: None,
                reparse_after_pending: false,
                reading_appearance: preview_appearance(cx),
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
            this.update_in(cx, |this, window, cx| {
                match parsed {
                    Ok(document) => {
                        // A reparse can drop or rename the server a panel is
                        // pointed at; Execute reads that panel's own field, so
                        // it has to follow the document rather than keep
                        // sending to an address the contract no longer names.
                        let server_url = this.effective_server_url(&document);
                        for panel in this.try_it_out_panels.values() {
                            panel.server_editor.update(cx, |editor, cx| {
                                if editor.text(cx) != server_url {
                                    editor.set_text(server_url.clone(), window, cx);
                                }
                            });
                        }
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

    fn cycle_reading_appearance(&mut self, cx: &mut Context<Self>) {
        self.reading_appearance = self.reading_appearance.next();
        // Remembered for every preview, not just this document: a reader who
        // wants light pages wants them for the next contract too.
        set_preview_appearance(self.reading_appearance, cx);
        cx.notify();
    }

    /// Floating reading controls over the document, matching the Markdown
    /// preview's own: kept faint until the pointer reaches them, bottom-right.
    /// This is chrome sitting on top of the document, so -- like the Markdown
    /// preview's own control -- it follows the editor's theme rather than the
    /// palette the document itself is forced to read in.
    fn render_reading_controls(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let appearance = self.reading_appearance;
        h_flex()
            .id("openapi-reading-controls")
            .absolute()
            .bottom_2()
            .right_3()
            .p_0p5()
            .gap_px()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().elevated_surface_background)
            .shadow_sm()
            .opacity(RESTING_CONTROL_OPACITY)
            .hover(|style| style.opacity(1.0))
            .child(
                IconButton::new("openapi-reading-theme", IconName::Screen)
                    .icon_size(IconSize::Small)
                    .toggle_state(appearance.overrides_editor())
                    .tooltip(Tooltip::text(appearance.tooltip()))
                    .on_click(cx.listener(|this, _, _, cx| this.cycle_reading_appearance(cx))),
            )
    }

    /// Opens or closes the "Try it out" panel for one operation. Opening it
    /// builds one editor per declared parameter (plus any path placeholder a
    /// sloppily-written contract left undeclared, see
    /// `try_it_out::undeclared_path_parameter_names`), a Server field seeded
    /// from the document's first server URL, and -- when the operation has
    /// one -- a body editor seeded from the same JSON skeleton
    /// `collection_from_document` builds for "Save to API Client".
    fn toggle_try_it_out(
        &mut self,
        operation_key: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.try_it_out_panels.remove(&operation_key).is_some() {
            cx.notify();
            return;
        }
        let Some(document) = self.document.clone() else {
            return;
        };
        let Some(operation) = document
            .groups
            .iter()
            .flat_map(|group| &group.operations)
            .find(|operation| operation.key() == operation_key)
            .cloned()
        else {
            return;
        };

        let base_url = self.effective_server_url(&document);
        let server_editor =
            new_try_it_out_field_editor("https://api.example.com", &base_url, window, cx);
        let auth_editor = new_try_it_out_field_editor("Bearer <token>", "", window, cx);

        let mut parameter_fields: Vec<ParameterField> = Vec::new();
        for parameter in &operation.parameters {
            if !try_it_out::parameter_is_fillable(
                parameter.location.as_ref(),
                parameter.name.as_ref(),
                &operation.path,
            ) {
                continue;
            }
            let input = new_parameter_input(parameter, window, cx);
            parameter_fields.push(ParameterField {
                name: parameter.name.clone(),
                location: parameter.location.clone(),
                required: parameter.required,
                input,
            });
        }
        for name in
            try_it_out::undeclared_path_parameter_names(&operation.path, &operation.parameters)
        {
            let editor = new_try_it_out_field_editor("Value", "", window, cx);
            parameter_fields.push(ParameterField {
                name: name.into(),
                location: "path".into(),
                required: true,
                input: ParameterInput::Text(editor),
            });
        }

        let body_editor = if operation.request_body.is_some() {
            let imported = collection_from_document(
                &document,
                OperationSelection::SingleOperation(operation_key.clone()),
            );
            let initial_text = imported
                .requests
                .first()
                .and_then(|request| match &request.body {
                    api_client::RequestBody::Raw { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            Some(new_try_it_out_body_editor(&initial_text, window, cx))
        } else {
            None
        };

        let response_body_editor = new_try_it_out_response_editor(window, cx);

        self.try_it_out_panels.insert(
            operation_key,
            TryItOutPanel {
                server_editor,
                auth_editor,
                parameter_fields,
                body_editor,
                response_body_editor,
                headers_expanded: false,
                send_state: TryItOutSendState::Idle,
                _send_task: None,
            },
        );
        cx.notify();
    }

    /// The server a "Try it out" panel's Server field is seeded with: the
    /// reader's pick from the header's servers dropdown, or the document's
    /// default when nothing has been picked (or the pick no longer exists).
    fn effective_server_url(&self, document: &OpenApiDocument) -> String {
        resolve_selected_server(&document.servers, self.selected_server_url.as_ref())
            .map(|server| server.url.to_string())
            .unwrap_or_default()
    }

    /// Applies a pick from the header's servers dropdown: remembered for the
    /// next panel that opens, and pushed into every panel already open so an
    /// in-progress "Try it out" session follows the new selection too.
    fn select_server(&mut self, url: SharedString, window: &mut Window, cx: &mut Context<Self>) {
        self.selected_server_url = Some(url.clone());
        for panel in self.try_it_out_panels.values() {
            panel.server_editor.update(cx, |editor, cx| {
                editor.set_text(url.to_string(), window, cx);
            });
        }
        cx.notify();
    }

    /// Records the reader's pick from an enum parameter's dropdown, or clears
    /// it back to unset when they chose the blank entry.
    fn set_parameter_value(
        &mut self,
        operation_key: SharedString,
        parameter_name: SharedString,
        parameter_location: SharedString,
        value: Option<SharedString>,
        cx: &mut Context<Self>,
    ) {
        if let Some(panel) = self.try_it_out_panels.get_mut(&operation_key)
            && let Some(field) = panel
                .parameter_fields
                .iter_mut()
                .find(|field| field.name == parameter_name && field.location == parameter_location)
            && let ParameterInput::Select { selected, .. } = &mut field.input
        {
            *selected = value;
        }
        cx.notify();
    }

    fn toggle_try_it_out_headers(&mut self, operation_key: SharedString, cx: &mut Context<Self>) {
        if let Some(panel) = self.try_it_out_panels.get_mut(&operation_key) {
            panel.headers_expanded = !panel.headers_expanded;
        }
        cx.notify();
    }

    fn clear_try_it_out_response(
        &mut self,
        operation_key: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(panel) = self.try_it_out_panels.get_mut(&operation_key) else {
            return;
        };
        panel._send_task = None;
        panel.send_state = TryItOutSendState::Idle;
        panel.response_body_editor.update(cx, |editor, cx| {
            editor.set_read_only(false);
            editor.set_text("", window, cx);
            editor.set_read_only(true);
        });
        cx.notify();
    }

    /// Builds the single-operation request `api_collection` already knows how
    /// to construct, overlays every value the reader typed in via
    /// `try_it_out::apply_overrides`, resolves it, and sends it through the
    /// same `api_client::execute` the API Client panel itself uses.
    fn execute_try_it_out(
        &mut self,
        operation_key: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(panel) = self.try_it_out_panels.get(&operation_key) else {
            return;
        };
        if matches!(panel.send_state, TryItOutSendState::Sending) {
            return;
        }
        let Some(document) = self.document.clone() else {
            return;
        };

        let Some(store) = ApiClientStore::global(cx) else {
            if let Some(panel) = self.try_it_out_panels.get_mut(&operation_key) {
                panel.send_state = TryItOutSendState::Error(
                    "The API Client isn't available, so this request can't be sent.".into(),
                );
            }
            cx.notify();
            return;
        };
        let client = store.read(cx).http_client.clone();

        let Some(panel) = self.try_it_out_panels.get(&operation_key) else {
            return;
        };
        let server_url = panel.server_editor.read(cx).text(cx);
        let auth_header_value = panel.auth_editor.read(cx).text(cx);
        let body_text = panel
            .body_editor
            .as_ref()
            .map(|editor| editor.read(cx).text(cx));
        let parameters: Vec<try_it_out::ParameterOverride> = panel
            .parameter_fields
            .iter()
            .map(|field| {
                let value = match &field.input {
                    ParameterInput::Text(editor) => editor.read(cx).text(cx),
                    ParameterInput::Select { selected, .. } => selected
                        .as_ref()
                        .map(SharedString::to_string)
                        .unwrap_or_default(),
                };
                try_it_out::ParameterOverride {
                    name: field.name.to_string(),
                    location: field.location.to_string(),
                    required: field.required,
                    value,
                }
            })
            .collect();

        if let Some(panel) = self.try_it_out_panels.get_mut(&operation_key) {
            panel.send_state = TryItOutSendState::Sending;
        }
        cx.notify();

        let overrides = try_it_out::TryItOutOverrides {
            server_url,
            auth_header_value,
            body_text,
            parameters,
        };

        let panel_key = operation_key.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let imported = collection_from_document(
                &document,
                OperationSelection::SingleOperation(operation_key.clone()),
            );
            let Some(base_request) = imported.requests.into_iter().next() else {
                this.update(cx, |this, cx| {
                    if let Some(panel) = this.try_it_out_panels.get_mut(&operation_key) {
                        panel.send_state = TryItOutSendState::Error(
                            "This operation no longer has a request to send.".into(),
                        );
                    }
                    cx.notify();
                })
                .ok();
                return;
            };

            let (request, collection) =
                try_it_out::apply_overrides(base_request, imported.collection, &overrides);
            let resolved = try_it_out::resolve_and_build(&request, &collection);

            match api_client::execute(&client, &resolved).await {
                Ok(summary) => {
                    let content_type = summary
                        .headers
                        .iter()
                        .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
                        .map(|(_, value)| value.as_str())
                        .unwrap_or("")
                        .to_string();
                    let (body_text, truncated) =
                        try_it_out::cap_and_render_body(&summary.body, &content_type);
                    let meta = TryItOutResponseMeta {
                        status: summary.status,
                        status_text: summary.status_text.into(),
                        elapsed_ms: summary.elapsed_ms,
                        size_bytes: summary.body.len(),
                        headers: summary.headers,
                        body_truncated: truncated,
                    };
                    this.update_in(cx, |this, window, cx| {
                        if let Some(panel) = this.try_it_out_panels.get_mut(&operation_key) {
                            panel.response_body_editor.update(cx, |editor, cx| {
                                editor.set_read_only(false);
                                editor.set_text(body_text, window, cx);
                                editor.set_read_only(true);
                            });
                            panel.send_state = TryItOutSendState::Success(meta);
                        }
                        cx.notify();
                    })
                    .ok();
                }
                Err(error) => {
                    let message: SharedString = error.to_string().into();
                    this.update(cx, |this, cx| {
                        if let Some(panel) = this.try_it_out_panels.get_mut(&operation_key) {
                            panel.send_state = TryItOutSendState::Error(message);
                        }
                        cx.notify();
                    })
                    .ok();
                }
            }
        });

        if let Some(panel) = self.try_it_out_panels.get_mut(&panel_key) {
            panel._send_task = Some(task);
        }
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
        palette: &Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        v_flex()
            .debug_selector(|| "openapi-header".to_string())
            .gap_1p5()
            .p_3()
            .rounded_lg()
            .border_b_1()
            .border_color(palette.border)
            .bg(palette.surface_background)
            .child(
                h_flex()
                    .flex_wrap()
                    .gap_2()
                    .items_center()
                    .child(
                        Label::new(document.title.clone())
                            .size(LabelSize::Custom(rems(1.5)))
                            .weight(gpui::FontWeight::BOLD)
                            .color(palette.resolve(Color::Default)),
                    )
                    .when_some(document.version.clone(), |header, version| {
                        header.child(pill(
                            version,
                            palette.element_background,
                            palette.resolve(Color::Default),
                        ))
                    })
                    .child(pill(
                        document.spec_label.clone(),
                        palette.border_variant,
                        palette.resolve(Color::Muted),
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
            .children(self.render_servers_dropdown(document, palette, window, cx))
            .when_some(document.description.clone(), |header, description| {
                header.child(render_prose(
                    &description,
                    LabelSize::Small,
                    palette.resolve(Color::Muted),
                    palette,
                    cx,
                ))
            })
    }

    /// The document's `servers:` list as a single dropdown, defaulting to the
    /// first entry: picking one changes what a "Try it out" send goes to,
    /// pre-filling (and, for panels already open, updating) every panel's
    /// Server field. `None` when the document declares no servers at all.
    fn render_servers_dropdown(
        &self,
        document: &OpenApiDocument,
        palette: &Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if document.servers.is_empty() {
            return None;
        }
        let servers = document.servers.clone();
        let selected_url = resolve_selected_server(&servers, self.selected_server_url.as_ref())
            .map(|server| server.url.clone())
            .unwrap_or_default();
        let weak = cx.weak_entity();

        let menu = ContextMenu::build(window, cx, move |mut menu, _, _| {
            for server in &servers {
                let weak = weak.clone();
                let url = server.url.clone();
                let label: SharedString = match &server.description {
                    Some(description) => format!("{} -- {description}", server.url).into(),
                    None => server.url.clone(),
                };
                menu = menu.entry(label, None, move |_, cx| {
                    let url = url.clone();
                    weak.update_in(cx, |view, window, cx| {
                        view.select_server(url, window, cx);
                    })
                    .ok();
                });
            }
            menu
        });

        Some(
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    Label::new("Server")
                        .size(LabelSize::Small)
                        .color(palette.resolve(Color::Muted)),
                )
                .child(DropdownMenu::new(
                    "openapi-servers-dropdown",
                    selected_url,
                    menu,
                ))
                .into_any_element(),
        )
    }

    fn render_notes(&self, notes: &[SharedString], palette: &Palette) -> Option<AnyElement> {
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
                .border_color(palette.border_variant)
                .children(notes.iter().map(|note| {
                    h_flex()
                        .gap_1p5()
                        .items_start()
                        .child(
                            Icon::new(IconName::Info)
                                .size(IconSize::XSmall)
                                .color(palette.resolve(Color::Muted)),
                        )
                        .child(
                            Label::new(note.clone())
                                .size(LabelSize::Small)
                                .color(palette.resolve(Color::Muted)),
                        )
                }))
                .into_any_element(),
        )
    }

    fn render_group(
        &self,
        group: &OperationGroup,
        palette: &Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let collapsed = self.collapsed_groups.contains(&group.name);
        let group_name = group.name.clone();
        let operation_count = group.operations.len();

        v_flex()
            .debug_selector(|| format!("openapi-tag-card-{}", group.name))
            .rounded_lg()
            .border_1()
            .border_color(palette.border)
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
                    .bg(palette.surface_background)
                    .hover(|style| style.bg(palette.element_hover))
                    .on_click(
                        cx.listener(move |this, _, _, cx| {
                            this.toggle_group(group_name.clone(), cx)
                        }),
                    )
                    .when(!collapsed, |header| {
                        header.border_b_1().border_color(palette.border)
                    })
                    .child(
                        Label::new(group.name.clone())
                            .size(LabelSize::Custom(rems(1.125)))
                            .weight(gpui::FontWeight::SEMIBOLD)
                            .flex_none()
                            .color(palette.resolve(Color::Default)),
                    )
                    .child(div().flex_1().min_w_0().when_some(
                        group.description.clone(),
                        |container, description| {
                            container.child(
                                Label::new(description)
                                    .size(LabelSize::Small)
                                    .color(palette.resolve(Color::Muted))
                                    .truncate(),
                            )
                        },
                    ))
                    .child(
                        Label::new(format!("{operation_count}"))
                            .size(LabelSize::XSmall)
                            .color(palette.resolve(Color::Muted)),
                    )
                    .child(
                        Icon::new(if collapsed {
                            IconName::ChevronRight
                        } else {
                            IconName::ChevronDown
                        })
                        .size(IconSize::XSmall)
                        .color(palette.resolve(Color::Muted)),
                    ),
            )
            .when(!collapsed, |card| {
                let mut operations = v_flex().gap_2().p_2();
                for operation in &group.operations {
                    operations =
                        operations.child(self.render_operation(operation, palette, window, cx));
                }
                card.child(operations)
            })
    }

    fn render_operation(
        &self,
        operation: &Operation,
        palette: &Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let key = operation.key();
        let expanded = self.expanded_operations.contains(&key);
        let toggle_key = key.clone();
        let accent = method_accent_color(method_accent(operation.method), palette);

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
                                .color(palette.resolve(Color::Default))
                                .when(operation.deprecated, |label| label.strikethrough()),
                        ),
                    )
                    .child(div().flex_1().min_w_0().when_some(
                        operation.summary.clone(),
                        |container, summary| {
                            container.child(
                                Label::new(summary)
                                    .size(LabelSize::Small)
                                    .color(palette.resolve(Color::Muted))
                                    .truncate(),
                            )
                        },
                    ))
                    .when(operation.deprecated, |row| {
                        row.child(
                            Label::new("deprecated")
                                .size(LabelSize::XSmall)
                                .color(palette.resolve(Color::Warning)),
                        )
                    })
                    .when(operation.secured, |row| {
                        row.child(
                            Icon::new(IconName::Lock)
                                .size(IconSize::XSmall)
                                .color(palette.resolve(Color::Muted)),
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
                        .child(self.render_operation_details(operation, palette, window, cx)),
                )
            })
    }

    /// Lays an expanded operation out the way a reference page does: the
    /// Parameters table first (each row grows its "Try it out" input in place
    /// once the toggle is on), then Request body, then the Server/Auth/
    /// Execute controls, then Responses, and finally -- once a request has
    /// actually been sent -- the response, below everything else.
    fn render_operation_details(
        &self,
        operation: &Operation,
        palette: &Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let key = operation.key();
        let panel = self.try_it_out_panels.get(&key);

        let mut details = v_flex().gap_2();

        if let Some(description) = operation.description.clone() {
            details = details.child(render_prose(
                &description,
                LabelSize::Small,
                palette.resolve(Color::Default),
                palette,
                cx,
            ));
        }
        if let Some(operation_id) = operation.operation_id.clone() {
            details = details.child(
                h_flex()
                    .gap_2()
                    .child(
                        Label::new("operationId")
                            .size(LabelSize::XSmall)
                            .color(palette.resolve(Color::Muted)),
                    )
                    .child(
                        Label::new(operation_id)
                            .size(LabelSize::Small)
                            .buffer_font(cx)
                            .color(palette.resolve(Color::Default)),
                    ),
            );
        }

        details = details.child(
            h_flex()
                .justify_between()
                .items_center()
                .child(section_title("Parameters", palette))
                .child(self.render_try_it_out_toggle(operation, cx)),
        );
        details = if operation.parameters.is_empty() {
            details.child(
                Label::new("No parameters")
                    .size(LabelSize::Small)
                    .color(palette.resolve(Color::Muted)),
            )
        } else {
            details.child(render_parameters_table(
                &operation.parameters,
                &key,
                panel,
                palette,
                window,
                cx,
            ))
        };

        if let Some(body) = operation.request_body.clone() {
            details = details
                .child(section_title("Request body", palette))
                .child(render_request_body(&body, palette, cx));
            if let Some(panel) = panel
                && let Some(body_editor) = &panel.body_editor
            {
                details =
                    details.child(render_try_it_out_body_editor(body_editor.clone(), palette));
            }
        }

        if let Some(panel) = panel {
            details = details.child(self.render_try_it_out_controls(&key, panel, palette, cx));
        }

        if !operation.responses.is_empty() {
            details = details.child(section_title("Responses", palette));
            if let Some(document) = self.document.as_ref() {
                details = details.child(render_responses_table(
                    document,
                    &operation.responses,
                    &key,
                    palette,
                    cx,
                ));
            }
        }

        if let Some(panel) = panel {
            details = details.child(self.render_try_it_out_result(&key, panel, palette, cx));
        }

        details
    }

    fn render_try_it_out_toggle(
        &self,
        operation: &Operation,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let key = operation.key();
        let active = self.try_it_out_panels.contains_key(&key);
        let toggle_key = key.clone();

        Button::new(
            SharedString::from(format!("openapi-try-it-out-toggle-{key}")),
            if active {
                "Close Try it out"
            } else {
                "Try it out"
            },
        )
        .style(ButtonStyle::Subtle)
        .start_icon(Icon::new(if active {
            IconName::ChevronDown
        } else {
            IconName::PlayOutlined
        }))
        .on_click(cx.listener(move |this, _, window, cx| {
            this.toggle_try_it_out(toggle_key.clone(), window, cx);
        }))
    }

    /// The Server/Authorization fields plus the Execute/Clear buttons -- the
    /// part of "Try it out" that is not tied to a single parameter row, shown
    /// directly below the Parameters table (and Request body, when there is
    /// one), before Responses.
    fn render_try_it_out_controls(
        &self,
        operation_key: &SharedString,
        panel: &TryItOutPanel,
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let fields = v_flex()
            .gap_2()
            .child(render_try_it_out_field(
                "Server".into(),
                None,
                false,
                panel.server_editor.clone(),
                palette,
            ))
            .child(render_try_it_out_field(
                "Authorization".into(),
                None,
                false,
                panel.auth_editor.clone(),
                palette,
            ));

        let is_sending = matches!(panel.send_state, TryItOutSendState::Sending);
        let is_idle = matches!(panel.send_state, TryItOutSendState::Idle);
        let execute_key = operation_key.clone();
        let clear_key = operation_key.clone();

        v_flex()
            .gap_3()
            .p_2()
            .rounded_md()
            .border_1()
            .border_color(palette.border_variant)
            .child(fields)
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new(
                            SharedString::from(format!(
                                "openapi-try-it-out-execute-{operation_key}"
                            )),
                            if is_sending { "Sending…" } else { "Execute" },
                        )
                        .start_icon(Icon::new(IconName::Send))
                        .loading(is_sending)
                        .disabled(is_sending)
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.execute_try_it_out(execute_key.clone(), window, cx);
                            },
                        )),
                    )
                    .child(
                        Button::new(
                            SharedString::from(format!("openapi-try-it-out-clear-{operation_key}")),
                            "Clear",
                        )
                        .style(ButtonStyle::Subtle)
                        // Clearing drops the task that carries the request, so it
                        // stays out of reach until the reply is in: a cleared panel
                        // mid-flight would look like nothing had been asked for.
                        .disabled(is_idle || is_sending)
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.clear_try_it_out_response(clear_key.clone(), window, cx);
                            },
                        )),
                    ),
            )
    }

    fn render_try_it_out_result(
        &self,
        operation_key: &SharedString,
        panel: &TryItOutPanel,
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match &panel.send_state {
            TryItOutSendState::Idle => div().into_any_element(),
            TryItOutSendState::Sending => h_flex()
                .pt_2()
                .child(
                    Label::new("Sending…")
                        .size(LabelSize::Small)
                        .color(palette.resolve(Color::Muted)),
                )
                .into_any_element(),
            TryItOutSendState::Error(message) => v_flex()
                .gap_1()
                .pt_2()
                .child(
                    Label::new("Request failed")
                        .size(LabelSize::Small)
                        .color(palette.resolve(Color::Error)),
                )
                .child(
                    Label::new(message.clone())
                        .size(LabelSize::Small)
                        .color(palette.resolve(Color::Muted))
                        .buffer_font(cx),
                )
                .into_any_element(),
            TryItOutSendState::Success(meta) => {
                let status_label = format!("{} {}", meta.status, meta.status_text);
                let color = palette.resolve(status_color(&meta.status.to_string()));
                let size_label = try_it_out::format_response_size(meta.size_bytes);
                let headers_expanded = panel.headers_expanded;
                let headers = meta.headers.clone();
                let toggle_headers_key = operation_key.clone();

                let mut result = v_flex()
                    .gap_2()
                    .pt_2()
                    .border_t_1()
                    .border_color(palette.border_variant)
                    .child(
                        h_flex()
                            .gap_3()
                            .items_center()
                            .child(
                                Label::new(status_label)
                                    .weight(gpui::FontWeight::BOLD)
                                    .color(color),
                            )
                            .child(
                                Label::new(format!("{} ms", meta.elapsed_ms))
                                    .size(LabelSize::Small)
                                    .color(palette.resolve(Color::Muted)),
                            )
                            .child(
                                Label::new(size_label)
                                    .size(LabelSize::Small)
                                    .color(palette.resolve(Color::Muted)),
                            ),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .child(
                                h_flex()
                                    .id(SharedString::from(format!(
                                        "openapi-try-it-out-headers-toggle-{operation_key}"
                                    )))
                                    .gap_1()
                                    .items_center()
                                    .cursor_pointer()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.toggle_try_it_out_headers(
                                            toggle_headers_key.clone(),
                                            cx,
                                        );
                                    }))
                                    .child(
                                        Icon::new(if headers_expanded {
                                            IconName::ChevronDown
                                        } else {
                                            IconName::ChevronRight
                                        })
                                        .size(IconSize::XSmall)
                                        .color(palette.resolve(Color::Muted)),
                                    )
                                    .child(
                                        Label::new(format!("Headers ({})", headers.len()))
                                            .size(LabelSize::Small)
                                            .color(palette.resolve(Color::Muted)),
                                    ),
                            )
                            .when(headers_expanded, |section| {
                                section.child(v_flex().gap_0p5().pl_4().children(
                                    headers.iter().map(|(key, value)| {
                                        h_flex()
                                            .gap_2()
                                            .child(
                                                Label::new(key.clone())
                                                    .size(LabelSize::XSmall)
                                                    .color(palette.resolve(Color::Muted)),
                                            )
                                            .child(
                                                Label::new(value.clone())
                                                    .size(LabelSize::XSmall)
                                                    .color(palette.resolve(Color::Default)),
                                            )
                                    }),
                                ))
                            }),
                    );

                if meta.body_truncated {
                    result = result.child(
                        Label::new(format!(
                            "Response body truncated -- only the first {} KB is shown.",
                            try_it_out::MAX_RESPONSE_BODY_BYTES / 1024
                        ))
                        .size(LabelSize::XSmall)
                        .color(palette.resolve(Color::Warning)),
                    );
                }

                result = result.child(
                    div()
                        .id(SharedString::from(format!(
                            "openapi-try-it-out-response-{operation_key}"
                        )))
                        .max_h(px(320.))
                        .overflow_y_scroll()
                        .rounded_md()
                        .border_1()
                        .border_color(palette.border_variant)
                        .px_2()
                        .py_1p5()
                        .child(panel.response_body_editor.clone()),
                );

                result.into_any_element()
            }
        }
    }

    fn render_schemas(
        &self,
        schemas: &[SchemaSummary],
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        v_flex()
            .debug_selector(|| "openapi-schemas".to_string())
            .gap_2()
            .p_3()
            .rounded_lg()
            .border_1()
            .border_color(palette.border)
            .bg(palette.surface_background)
            .child(
                Label::new("Schemas")
                    .size(LabelSize::Custom(rems(1.125)))
                    .weight(gpui::FontWeight::SEMIBOLD)
                    .color(palette.resolve(Color::Default)),
            )
            .children(
                schemas
                    .iter()
                    .map(|schema| self.render_schema(schema, palette, cx)),
            )
    }

    fn render_schema(
        &self,
        schema: &SchemaSummary,
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let expanded = self.expanded_schemas.contains(&schema.name);
        let schema_name = schema.name.clone();
        let toggle_name = schema.name.clone();

        v_flex()
            .rounded_md()
            .border_1()
            .border_color(palette.border_variant)
            .child(
                h_flex()
                    .id(SharedString::from(format!("openapi-schema-{schema_name}")))
                    .w_full()
                    .gap_2()
                    .items_center()
                    .px_2()
                    .py_1()
                    .cursor_pointer()
                    .hover(|style| style.bg(palette.element_hover))
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
                        .color(palette.resolve(Color::Muted)),
                    )
                    .child(
                        Label::new(schema.name.clone())
                            .buffer_font(cx)
                            .weight(gpui::FontWeight::BOLD)
                            .color(palette.resolve(Color::Default)),
                    )
                    .child(
                        Label::new(schema.type_label.clone())
                            .size(LabelSize::XSmall)
                            .color(palette.resolve(Color::Muted)),
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
                                .child(
                                    Label::new(format!("{property}:"))
                                        .buffer_font(cx)
                                        .color(palette.resolve(Color::Default)),
                                )
                                .child(
                                    Label::new(type_label.clone())
                                        .buffer_font(cx)
                                        .color(palette.resolve(Color::Muted)),
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

fn render_request_body(
    body: &RequestBody,
    palette: &Palette,
    cx: &App,
) -> impl IntoElement + use<> {
    v_flex()
        .gap_1()
        .p_2()
        .rounded_md()
        .border_1()
        .border_color(palette.border_variant)
        .child(
            h_flex()
                .flex_wrap()
                .gap_2()
                .items_center()
                .children(body.content_types.iter().map(|content_type| {
                    Label::new(content_type.clone())
                        .size(LabelSize::Small)
                        .buffer_font(cx)
                        .color(palette.resolve(Color::Default))
                }))
                .when(body.required, |row| {
                    row.child(
                        Label::new("required")
                            .size(LabelSize::XSmall)
                            .color(palette.resolve(Color::Error)),
                    )
                })
                .when_some(body.type_label.clone(), |row, type_label| {
                    row.child(
                        Label::new(type_label)
                            .size(LabelSize::XSmall)
                            .color(palette.resolve(Color::Muted))
                            .buffer_font(cx),
                    )
                }),
        )
        .when_some(body.description.clone(), |details, description| {
            details.child(render_prose(
                &description,
                LabelSize::XSmall,
                palette.resolve(Color::Muted),
                palette,
                cx,
            ))
        })
}

/// The Request body section's editable box, shown once "Try it out" is on --
/// seeded from the same JSON skeleton `collection_from_document` builds, and
/// sent as-is (or as edited) when Execute is pressed.
fn render_try_it_out_body_editor(
    body_editor: Entity<Editor>,
    palette: &Palette,
) -> impl IntoElement + use<> {
    div()
        .rounded_md()
        .border_1()
        .border_color(palette.border_variant)
        .px_2()
        .py_1p5()
        .child(body_editor)
}

/// The Parameters table. Each row always shows the parameter's name, type,
/// and description; once "Try it out" is on (`panel` is `Some`), a row for a
/// parameter with allowed values also grows an "Available values" line, and a
/// fillable parameter grows its input (a dropdown for an enum, otherwise
/// plain text) directly beneath the description -- matching where the
/// reference view grows its own inputs.
fn render_parameters_table(
    parameters: &[Parameter],
    operation_key: &SharedString,
    panel: Option<&TryItOutPanel>,
    palette: &Palette,
    window: &mut Window,
    cx: &mut Context<OpenApiPreviewView>,
) -> impl IntoElement + use<> {
    let mut table = v_flex()
        .debug_selector(|| format!("openapi-parameters-table-{operation_key}"))
        .rounded_md()
        .border_1()
        .border_color(palette.border_variant)
        .overflow_hidden()
        .child(parameters_table_header(palette));

    for (index, parameter) in parameters.iter().enumerate() {
        let name_column = v_flex()
            .w_1_3()
            .flex_none()
            .gap_0p5()
            .child(
                h_flex()
                    .gap_0p5()
                    .child(
                        Label::new(parameter.name.clone())
                            .buffer_font(cx)
                            .weight(gpui::FontWeight::BOLD)
                            .color(palette.resolve(Color::Default)),
                    )
                    .when(parameter.required, |row| {
                        row.child(
                            Label::new("*")
                                .weight(gpui::FontWeight::BOLD)
                                .color(palette.resolve(Color::Error)),
                        )
                        .child(
                            Label::new("required")
                                .size(LabelSize::XSmall)
                                .color(palette.resolve(Color::Error)),
                        )
                    }),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Label::new(parameter.type_label.clone())
                            .size(LabelSize::XSmall)
                            .color(palette.resolve(Color::Muted))
                            .italic(),
                    )
                    .child(
                        Label::new(parameter.location.clone())
                            .size(LabelSize::XSmall)
                            .color(palette.resolve(Color::Muted)),
                    ),
            );

        let mut description_column = v_flex().flex_1().min_w_0().gap_1();
        if let Some(description) = parameter.description.clone() {
            description_column = description_column.child(render_prose(
                &description,
                LabelSize::Small,
                palette.resolve(Color::Muted),
                palette,
                cx,
            ));
        }
        if panel.is_some()
            && let Some(available) = available_values_label(&parameter.allowed_values)
        {
            description_column = description_column.child(
                Label::new(available)
                    .size(LabelSize::XSmall)
                    .color(palette.resolve(Color::Muted)),
            );
        }
        let matching_field = panel.and_then(|panel| {
            panel
                .parameter_fields
                .iter()
                .find(|field| field.name == parameter.name && field.location == parameter.location)
        });
        if let Some(field) = matching_field {
            description_column = description_column.child(render_parameter_input(
                operation_key,
                field,
                palette,
                window,
                cx,
            ));
        }

        let row = h_flex()
            .gap_3()
            .items_start()
            .px_2()
            .py_1p5()
            .when(index > 0, |row| {
                row.border_t_1().border_color(palette.border_variant)
            })
            .child(name_column)
            .child(description_column);
        table = table.child(row);
    }

    table
}

/// A single parameter's "Try it out" input: the free-text editor built for
/// it, or an enum dropdown when the parameter declares allowed values.
fn render_parameter_input(
    operation_key: &SharedString,
    field: &ParameterField,
    palette: &Palette,
    window: &mut Window,
    cx: &mut Context<OpenApiPreviewView>,
) -> AnyElement {
    match &field.input {
        ParameterInput::Text(editor) => div()
            .rounded_sm()
            .border_1()
            .border_color(palette.border_variant)
            .px_2()
            .py_1()
            .child(editor.clone())
            .into_any_element(),
        ParameterInput::Select {
            allowed_values,
            selected,
        } => render_parameter_enum_dropdown(
            operation_key.clone(),
            field.name.clone(),
            field.location.clone(),
            allowed_values,
            field.required,
            selected.clone(),
            window,
            cx,
        )
        .into_any_element(),
    }
}

/// A dropdown limited to an enum parameter's allowed values, matching the
/// reference view's own rendering of one. An optional parameter also gets a
/// blank leading entry (picking it clears the value, meaning "don't send
/// this"); a required one does not, since some value has to go out either way.
fn render_parameter_enum_dropdown(
    operation_key: SharedString,
    parameter_name: SharedString,
    parameter_location: SharedString,
    allowed_values: &[SharedString],
    required: bool,
    selected: Option<SharedString>,
    window: &mut Window,
    cx: &mut Context<OpenApiPreviewView>,
) -> impl IntoElement + use<> {
    let label = selected.unwrap_or_else(|| "--".into());
    let id = SharedString::from(format!(
        "openapi-param-enum-{operation_key}-{parameter_location}-{parameter_name}"
    ));
    let weak = cx.weak_entity();
    let values = allowed_values.to_vec();

    let menu = ContextMenu::build(window, cx, move |mut menu, _, _| {
        if !required {
            let weak = weak.clone();
            let operation_key = operation_key.clone();
            let parameter_name = parameter_name.clone();
            let parameter_location = parameter_location.clone();
            menu = menu.entry("--", None, move |_, cx| {
                weak.update(cx, |view, cx| {
                    view.set_parameter_value(
                        operation_key.clone(),
                        parameter_name.clone(),
                        parameter_location.clone(),
                        None,
                        cx,
                    )
                })
                .ok();
            });
        }
        for value in &values {
            let weak = weak.clone();
            let operation_key = operation_key.clone();
            let parameter_name = parameter_name.clone();
            let parameter_location = parameter_location.clone();
            let value = value.clone();
            menu = menu.entry(value.clone(), None, move |_, cx| {
                weak.update(cx, |view, cx| {
                    view.set_parameter_value(
                        operation_key.clone(),
                        parameter_name.clone(),
                        parameter_location.clone(),
                        Some(value.clone()),
                        cx,
                    )
                })
                .ok();
            });
        }
        menu
    });

    DropdownMenu::new(id, label, menu).full_width(true)
}

fn parameters_table_header(palette: &Palette) -> impl IntoElement {
    h_flex()
        .gap_3()
        .px_2()
        .py_1()
        .border_b_1()
        .border_color(palette.border_variant)
        .bg(palette.surface_background)
        .child(
            div().w_1_3().flex_none().child(
                Label::new("Name")
                    .size(LabelSize::XSmall)
                    .weight(gpui::FontWeight::SEMIBOLD)
                    .color(palette.resolve(Color::Muted)),
            ),
        )
        .child(
            div().flex_1().min_w_0().child(
                Label::new("Description")
                    .size(LabelSize::XSmall)
                    .weight(gpui::FontWeight::SEMIBOLD)
                    .color(palette.resolve(Color::Muted)),
            ),
        )
}

fn render_responses_table(
    document: &OpenApiDocument,
    responses: &[Response],
    operation_key: &SharedString,
    palette: &Palette,
    cx: &App,
) -> impl IntoElement + use<> {
    v_flex()
        .debug_selector(|| format!("openapi-responses-table-{operation_key}"))
        .rounded_md()
        .border_1()
        .border_color(palette.border_variant)
        .overflow_hidden()
        .child(responses_table_header(palette))
        .children(responses.iter().enumerate().map(|(index, response)| {
            h_flex()
                .gap_3()
                .items_start()
                .px_2()
                .py_1p5()
                .when(index > 0, |row| {
                    row.border_t_1().border_color(palette.border_variant)
                })
                .child(
                    div().w_1_5().flex_none().child(
                        Label::new(response.status.clone())
                            .buffer_font(cx)
                            .weight(gpui::FontWeight::BOLD)
                            .color(palette.resolve(status_color(&response.status))),
                    ),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_0p5()
                        .when_some(response.description.clone(), |details, description| {
                            details.child(render_prose(
                                &description,
                                LabelSize::Small,
                                palette.resolve(Color::Default),
                                palette,
                                cx,
                            ))
                        })
                        .when_some(response.type_label.clone(), |details, type_label| {
                            details.child(
                                Label::new(type_label)
                                    .size(LabelSize::XSmall)
                                    .color(palette.resolve(Color::Muted))
                                    .buffer_font(cx),
                            )
                        })
                        .children(response.content_types.iter().map(|content_type| {
                            Label::new(content_type.clone())
                                .size(LabelSize::XSmall)
                                .color(palette.resolve(Color::Muted))
                        }))
                        .children(response_example(document, response).map(|example| {
                            v_flex()
                                .gap_0p5()
                                .child(
                                    Label::new("Example value")
                                        .size(LabelSize::XSmall)
                                        .color(palette.resolve(Color::Muted)),
                                )
                                .child(
                                    div()
                                        .rounded_md()
                                        .border_1()
                                        .border_color(palette.border_variant)
                                        .bg(palette.elevated_surface_background)
                                        .px_2()
                                        .py_1()
                                        .child(
                                            Label::new(example)
                                                .size(LabelSize::XSmall)
                                                .buffer_font(cx)
                                                .color(palette.resolve(Color::Default)),
                                        ),
                                )
                        })),
                )
        }))
}

/// The shape a reader can expect back, built from the schema the response names.
/// A response naming nothing this document defines has no example to show, and
/// inventing one would be worse than leaving it out.
fn response_example(document: &OpenApiDocument, response: &Response) -> Option<SharedString> {
    let label = response.type_label.as_ref()?;
    // A collection is named after what it holds, so the schema to build from is
    // the item's -- shown back inside an array, which is what arrives.
    let (item_name, is_array) = match label.as_ref().strip_suffix("[]") {
        Some(item) => (SharedString::from(item.to_owned()), true),
        None => (label.clone(), false),
    };
    let skeleton = crate::api_collection::json_skeleton(document, Some(&item_name));
    if skeleton.as_object().is_none_or(serde_json::Map::is_empty) {
        return None;
    }
    let example = if is_array {
        serde_json::Value::Array(vec![skeleton])
    } else {
        skeleton
    };
    serde_json::to_string_pretty(&example)
        .ok()
        .map(SharedString::from)
}

fn responses_table_header(palette: &Palette) -> impl IntoElement {
    h_flex()
        .gap_3()
        .px_2()
        .py_1()
        .border_b_1()
        .border_color(palette.border_variant)
        .bg(palette.surface_background)
        .child(
            div().w_1_5().flex_none().child(
                Label::new("Code")
                    .size(LabelSize::XSmall)
                    .weight(gpui::FontWeight::SEMIBOLD)
                    .color(palette.resolve(Color::Muted)),
            ),
        )
        .child(
            div().flex_1().min_w_0().child(
                Label::new("Description")
                    .size(LabelSize::XSmall)
                    .weight(gpui::FontWeight::SEMIBOLD)
                    .color(palette.resolve(Color::Muted)),
            ),
        )
}

fn section_title(title: &'static str, palette: &Palette) -> impl IntoElement {
    Label::new(title.to_uppercase())
        .size(LabelSize::XSmall)
        .color(palette.resolve(Color::Muted))
        .weight(gpui::FontWeight::SEMIBOLD)
}

/// Renders Markdown prose as a wrapping row of labels, alternating plain text
/// with a highlighted chip for each backtick code span -- so a description
/// reads the way a reference page shows it instead of leaking backticks.
fn render_prose(
    text: &SharedString,
    size: LabelSize,
    color: Color,
    palette: &Palette,
    cx: &App,
) -> impl IntoElement + use<> {
    let mut row = h_flex().flex_wrap().gap_1();
    for span in split_code_spans(text) {
        row = match span {
            ProseSpan::Plain(text) => row.child(Label::new(text).size(size).color(color)),
            ProseSpan::Code(text) => row.child(code_chip(text, size, palette, cx)),
        };
    }
    row
}

/// A single inline code chip: `buffer_font` on a raised background, the same
/// treatment the response example block already gives a whole code snippet.
fn code_chip(
    text: SharedString,
    size: LabelSize,
    palette: &Palette,
    cx: &App,
) -> impl IntoElement + use<> {
    div()
        .px_1()
        .rounded_sm()
        .bg(palette.elevated_surface_background)
        .child(
            Label::new(text)
                .size(size)
                .buffer_font(cx)
                .color(palette.resolve(Color::Default)),
        )
}

fn new_try_it_out_field_editor(
    placeholder: &str,
    initial_value: &str,
    window: &mut Window,
    cx: &mut App,
) -> Entity<Editor> {
    cx.new(|cx| {
        let mut editor = Editor::single_line(window, cx);
        editor.set_placeholder_text(placeholder, window, cx);
        if !initial_value.is_empty() {
            editor.set_text(initial_value.to_string(), window, cx);
        }
        editor
    })
}

/// Builds the input a single parameter field starts with: a plain text editor
/// for most parameters, or a dropdown pinned to an enum's allowed values.
fn new_parameter_input(parameter: &Parameter, window: &mut Window, cx: &mut App) -> ParameterInput {
    if parameter.allowed_values.is_empty() {
        ParameterInput::Text(new_try_it_out_field_editor("Value", "", window, cx))
    } else {
        ParameterInput::Select {
            selected: default_enum_selection(parameter.required, &parameter.allowed_values),
            allowed_values: parameter.allowed_values.clone(),
        }
    }
}

/// The value an enum parameter's dropdown starts on: the first allowed value
/// when the parameter is required (there is no blank entry to leave it on),
/// otherwise unset -- an optional enum always offers a blank entry so leaving
/// it alone means "don't send this".
fn default_enum_selection(required: bool, allowed_values: &[SharedString]) -> Option<SharedString> {
    if required {
        allowed_values.first().cloned()
    } else {
        None
    }
}

fn new_try_it_out_body_editor(
    initial_value: &str,
    window: &mut Window,
    cx: &mut App,
) -> Entity<Editor> {
    cx.new(|cx| {
        let mut editor = Editor::multi_line(window, cx);
        editor.set_placeholder_text("Request body", window, cx);
        if !initial_value.is_empty() {
            editor.set_text(initial_value.to_string(), window, cx);
        }
        editor
    })
}

fn new_try_it_out_response_editor(window: &mut Window, cx: &mut App) -> Entity<Editor> {
    cx.new(|cx| {
        let mut editor = Editor::multi_line(window, cx);
        editor.set_read_only(true);
        editor
    })
}

/// One labelled field row of a "Try it out" panel: the parameter's name,
/// its location (query/path/header) when it has one, a required marker, and
/// the single-line editor the reader types the value into.
fn render_try_it_out_field(
    label: SharedString,
    location: Option<SharedString>,
    required: bool,
    editor: Entity<Editor>,
    palette: &Palette,
) -> impl IntoElement + use<> {
    v_flex()
        .gap_0p5()
        .child(
            h_flex()
                .gap_1()
                .items_center()
                .child(
                    Label::new(label)
                        .size(LabelSize::Small)
                        .weight(gpui::FontWeight::BOLD)
                        .color(palette.resolve(Color::Default)),
                )
                .when(required, |row| {
                    row.child(
                        Label::new("*")
                            .size(LabelSize::Small)
                            .color(palette.resolve(Color::Error)),
                    )
                })
                .when_some(location, |row, location| {
                    row.child(
                        Label::new(location)
                            .size(LabelSize::XSmall)
                            .color(palette.resolve(Color::Muted)),
                    )
                }),
        )
        .child(
            div()
                .rounded_sm()
                .border_1()
                .border_color(palette.border_variant)
                .px_2()
                .py_1()
                .child(editor),
        )
}

fn pill(text: SharedString, background: Hsla, text_color: Color) -> impl IntoElement {
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

fn method_accent_color(accent: MethodAccent, palette: &Palette) -> Hsla {
    match accent {
        MethodAccent::Info => palette.info,
        MethodAccent::Created => palette.created,
        MethodAccent::Warning => palette.warning,
        MethodAccent::Error => palette.error,
    }
}

impl Focusable for OpenApiPreviewView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for OpenApiPreviewView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = resolve_theme(self.reading_appearance, cx);
        let palette = Palette::from_theme(&theme);

        let contents = match self.document.clone() {
            Some(document) => {
                let mut body = v_flex()
                    .gap_4()
                    .child(self.render_header(&document, &palette, window, cx))
                    .children(self.render_notes(&document.notes, &palette));
                for group in &document.groups {
                    body = body.child(
                        self.render_group(group, &palette, window, cx)
                            .into_any_element(),
                    );
                }
                if !document.schemas.is_empty() {
                    body = body.child(self.render_schemas(&document.schemas, &palette, cx));
                }
                body.into_any_element()
            }
            None => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_1()
                .child(
                    Icon::new(IconName::FileCode)
                        .size(IconSize::Medium)
                        .color(palette.resolve(Color::Muted)),
                )
                .child(
                    Label::new("This file has no readable OpenAPI contract yet")
                        .color(palette.resolve(Color::Muted)),
                )
                .into_any_element(),
        };

        v_flex()
            .size_full()
            .relative()
            .bg(palette.background)
            .when_some(self.parse_error.clone(), |body, error| {
                body.child(
                    h_flex()
                        .w_full()
                        .gap_1p5()
                        .p_2()
                        .items_start()
                        .bg(palette.warning_background)
                        .child(
                            Icon::new(IconName::Warning)
                                .size(IconSize::XSmall)
                                .color(palette.resolve(Color::Warning)),
                        )
                        .child(
                            v_flex()
                                .gap_0p5()
                                .child(
                                    Label::new("The contract does not parse")
                                        .size(LabelSize::Small)
                                        .color(palette.resolve(Color::Default)),
                                )
                                .child(
                                    Label::new(error)
                                        .size(LabelSize::XSmall)
                                        .color(palette.resolve(Color::Muted))
                                        .buffer_font(cx),
                                )
                                .child(
                                    Label::new("Showing the last version that parsed.")
                                        .size(LabelSize::XSmall)
                                        .color(palette.resolve(Color::Muted)),
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
            .child(self.render_reading_controls(cx))
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
    fn a_collection_response_shows_its_items_inside_an_array() {
        let document = crate::openapi_document::parse(
            "openapi: 3.0.3\ninfo:\n  title: Pets\npaths:\n  /pets:\n    get:\n      responses:\n        '200':\n          description: ok\n          content:\n            application/json:\n              schema:\n                type: array\n                items:\n                  $ref: '#/components/schemas/Pet'\ncomponents:\n  schemas:\n    Pet:\n      type: object\n      properties:\n        id:\n          type: integer\n        name:\n          type: string\n",
        )
        .expect("parse");
        let response = &document.groups[0].operations[0].responses[0];
        assert_eq!(response.type_label.as_deref(), Some("Pet[]"));

        let example = response_example(&document, response).expect("an example for a known schema");
        assert!(
            example.starts_with('[') && example.contains("\"id\"") && example.contains("\"name\""),
            "a collection has to be shown as one, got {example}"
        );

        // A response naming nothing this document defines has nothing to show.
        let unknown = Response {
            status: "200".into(),
            description: None,
            content_types: Vec::new(),
            type_label: Some("Missing".into()),
        };
        assert!(response_example(&document, &unknown).is_none());
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

    #[test]
    fn a_required_enum_starts_on_its_first_value_but_an_optional_one_starts_blank() {
        let values: Vec<SharedString> = vec!["available".into(), "pending".into(), "sold".into()];

        assert_eq!(
            default_enum_selection(true, &values),
            Some(SharedString::from("available")),
            "a required enum has no blank entry, so it must start on a real value"
        );
        assert_eq!(
            default_enum_selection(false, &values),
            None,
            "an optional enum starts on its blank entry"
        );
        assert_eq!(default_enum_selection(true, &[]), None);
    }
}
