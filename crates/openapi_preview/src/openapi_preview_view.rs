use std::collections::{HashMap, HashSet};
use std::time::Duration;

use api_client::FolderId;
use api_client_ui::ApiClientStore;
use editor::{Editor, EditorEvent};
use gpui::{
    AnyElement, App, Entity, FocusHandle, Focusable, Hsla, ScrollHandle, SharedString,
    Subscription, Task, Window,
};
use theme::Appearance;
use ui::{Tooltip, WithScrollbar, prelude::*};
use workspace::preview_appearance::{
    observe_preview_appearance, preview_appearance, set_preview_appearance,
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
    /// The dropdown list a click opened, painted from the reading palette rather
    /// than the editor's theme.
    open_dropdown: Option<OpenDropdown>,
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
    /// Not stored: the choice lives in one place for every preview, and a copy
    /// here would keep showing the palette this view was opened with.
    _appearance_observation: Subscription,
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
                open_dropdown: None,
                selected_server_url: None,
                pending_parse: None,
                reparse_after_pending: false,
                _appearance_observation: observe_preview_appearance(cx),
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
        let next = preview_appearance(cx).next();
        // Remembered for every preview, not just this document: a reader who
        // wants light pages wants them for the next contract too.
        set_preview_appearance(next, cx);
        cx.notify();
    }

    /// Floating reading controls over the document, matching the Markdown
    /// preview's own: kept faint until the pointer reaches them, bottom-right.
    /// This is chrome sitting on top of the document, so -- like the Markdown
    /// preview's own control -- it follows the editor's theme rather than the
    /// palette the document itself is forced to read in.
    fn render_reading_controls(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let appearance = preview_appearance(cx);
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

        let Some(try_it_out::TryItOutOverrides {
            server_url,
            auth_header_value,
            body_text,
            parameters,
        }) = self.try_it_out_overrides(&operation_key, cx)
        else {
            return;
        };

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

    /// What the reader has filled in for an operation, or `None` when its panel
    /// is closed. Both sending and saving read from here, so a saved request is
    /// the request that would have been sent.
    fn try_it_out_overrides(
        &self,
        operation_key: &SharedString,
        cx: &App,
    ) -> Option<try_it_out::TryItOutOverrides> {
        let panel = self.try_it_out_panels.get(operation_key)?;
        Some(try_it_out::TryItOutOverrides {
            server_url: panel.server_editor.read(cx).text(cx),
            auth_header_value: panel.auth_editor.read(cx).text(cx),
            body_text: panel
                .body_editor
                .as_ref()
                .map(|editor| editor.read(cx).text(cx)),
            parameters: panel
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
                .collect(),
        })
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

        let mut imported = collection_from_document(&document, selection.clone());
        // What the reader typed into "Try it out" is what they mean to save: the
        // token is the one exception, since a collection is written to disk.
        if let OperationSelection::SingleOperation(operation_key) = &selection
            && let Some(overrides) = self.try_it_out_overrides(operation_key, cx)
            && let Some(request) = imported.requests.pop()
        {
            let overrides = try_it_out::without_secrets(overrides);
            let (request, collection) =
                try_it_out::apply_overrides(request, imported.collection, &overrides);
            imported.requests.push(request);
            imported.collection = collection;
        }
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
        _window: &mut Window,
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
                    .child(palette_button(
                        "openapi-save-document",
                        "Save to API Client".into(),
                        ButtonWeight::Outlined,
                        false,
                        *palette,
                        cx.listener(|this, _, window, cx| {
                            this.save_document_to_api_client(window, cx)
                        }),
                    )),
            )
            .children(self.render_servers_dropdown(document, palette, cx))
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
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if document.servers.is_empty() {
            return None;
        }
        let selected_url =
            resolve_selected_server(&document.servers, self.selected_server_url.as_ref())
                .map(|server| server.url.clone())
                .unwrap_or_default();

        Some(
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    Label::new("Server")
                        .size(LabelSize::Small)
                        .color(palette.resolve(Color::Muted)),
                )
                .child(palette_dropdown_trigger(
                    "openapi-servers-dropdown",
                    selected_url,
                    DropdownKey::Server,
                    FieldStyle::plain(),
                    *palette,
                    cx,
                ))
                .into_any_element(),
        )
    }

    /// The open dropdown list, hung over the page from the reading palette. It
    /// is rendered once, at the end of the page, so it paints above everything
    /// else without any of the page's own rows covering it.
    fn render_open_dropdown(
        &self,
        document: &OpenApiDocument,
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let open = self.open_dropdown.as_ref()?;
        let entries: Vec<(SharedString, SharedString, Option<SharedString>)> = match &open.key {
            DropdownKey::Server => document
                .servers
                .iter()
                .map(|server| {
                    let label: SharedString = match &server.description {
                        Some(description) => format!("{} -- {description}", server.url).into(),
                        None => server.url.clone(),
                    };
                    (server.url.clone(), label, Some(server.url.clone()))
                })
                .collect(),
            DropdownKey::Parameter {
                operation_key,
                name,
                location,
            } => {
                let panel = self.try_it_out_panels.get(operation_key)?;
                let field = panel
                    .parameter_fields
                    .iter()
                    .find(|field| &field.name == name && &field.location == location)?;
                let ParameterInput::Select { allowed_values, .. } = &field.input else {
                    return None;
                };
                let mut entries: Vec<(SharedString, SharedString, Option<SharedString>)> =
                    Vec::with_capacity(allowed_values.len() + 1);
                if !field.required {
                    entries.push(("--".into(), "--".into(), None));
                }
                for value in allowed_values {
                    entries.push((value.clone(), value.clone(), Some(value.clone())));
                }
                entries
            }
        };
        if entries.is_empty() {
            return None;
        }

        let key = open.key.clone();
        let selected_now: Option<SharedString> = match &key {
            DropdownKey::Server => {
                resolve_selected_server(&document.servers, self.selected_server_url.as_ref())
                    .map(|server| server.url.clone())
            }
            DropdownKey::Parameter {
                operation_key,
                name,
                location,
            } => self
                .try_it_out_panels
                .get(operation_key)
                .and_then(|panel| {
                    panel
                        .parameter_fields
                        .iter()
                        .find(|field| &field.name == name && &field.location == location)
                })
                .and_then(|field| match &field.input {
                    ParameterInput::Select { selected, .. } => selected.clone(),
                    ParameterInput::Text(_) => None,
                }),
        };

        let mut list = v_flex()
            .id("openapi-dropdown-list")
            .min_w(px(220.))
            .max_h(px(320.))
            .overflow_y_scroll()
            .p_1()
            .gap_0p5()
            .rounded_md()
            .border_1()
            .border_color(palette.border)
            .bg(palette.elevated_surface_background);
        for (index, (value, label, pick)) in entries.into_iter().enumerate() {
            let key = key.clone();
            let pick = pick.clone();
            list = list.child(palette_dropdown_entry(
                SharedString::from(format!("openapi-dropdown-entry-{index}")),
                label,
                selected_now.as_ref() == Some(&value),
                *palette,
                cx.listener(move |this, _, window, cx| {
                    this.apply_dropdown_pick(&key, pick.clone(), window, cx);
                }),
            ));
        }

        Some(
            gpui::deferred(
                gpui::anchored()
                    .position(open.position)
                    // Opened next to a window edge the list would otherwise hang
                    // off-screen, taking its rows with it.
                    .snap_to_window_with_margin(px(8.))
                    .child(
                        div()
                            .occlude()
                            // Anywhere but the list itself dismisses it. The handler
                            // belongs to the list, not to the page: a page-wide
                            // handler only sees clicks that leave the preview.
                            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                                this.open_dropdown = None;
                                cx.notify();
                            }))
                            .child(list),
                    ),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }

    /// Applies what a dropdown row means and closes the list.
    fn apply_dropdown_pick(
        &mut self,
        key: &DropdownKey,
        pick: Option<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match key {
            DropdownKey::Server => {
                if let Some(url) = pick {
                    self.select_server(url, window, cx);
                }
            }
            DropdownKey::Parameter {
                operation_key,
                name,
                location,
            } => self.set_parameter_value(
                operation_key.clone(),
                name.clone(),
                location.clone(),
                pick,
                cx,
            ),
        }
        self.open_dropdown = None;
        cx.notify();
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
        let accent = method_accent_color(method_accent(operation.method), palette.appearance);
        let hover_background = palette.element_hover;

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
                        div()
                            .id(SharedString::from(format!("openapi-save-operation-{key}")))
                            .flex_none()
                            .p_0p5()
                            .rounded_sm()
                            .cursor_pointer()
                            .hover(move |style| style.bg(hover_background))
                            .tooltip(Tooltip::text("Save to API Client"))
                            .child(
                                Icon::new(IconName::Bookmark)
                                    .size(IconSize::XSmall)
                                    .color(palette.resolve(Color::Muted)),
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.save_operation_to_api_client(key.clone(), window, cx)
                            })),
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
                .items_end()
                .border_b_1()
                .border_color(palette.border_variant)
                .pb_1()
                .child(
                    v_flex()
                        .gap_1()
                        .child(
                            Label::new("Parameters")
                                .weight(gpui::FontWeight::SEMIBOLD)
                                .color(palette.resolve(Color::Default)),
                        )
                        // The bar under the heading is what marks it as the
                        // section in hand, the way a reference page does.
                        .child(div().h(px(2.)).w_full().bg(method_accent_color(
                            method_accent(operation.method),
                            palette.appearance,
                        ))),
                )
                .child(self.render_try_it_out_toggle(operation, palette, cx)),
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
                details = details.child(render_try_it_out_body_editor(
                    body_editor.clone(),
                    palette,
                    cx,
                ));
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
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = operation.key();
        let active = self.try_it_out_panels.contains_key(&key);
        let toggle_key = key.clone();

        palette_button(
            SharedString::from(format!("openapi-try-it-out-toggle-{key}")),
            if active {
                "Cancel".into()
            } else {
                "Try it out".into()
            },
            if active {
                ButtonWeight::Danger
            } else {
                ButtonWeight::Outlined
            },
            false,
            *palette,
            cx.listener(move |this, _, window, cx| {
                this.toggle_try_it_out(toggle_key.clone(), window, cx);
            }),
        )
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
                cx,
            ))
            .child(render_try_it_out_field(
                "Authorization".into(),
                None,
                false,
                panel.auth_editor.clone(),
                palette,
                cx,
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
            .bg(palette.surface_background)
            .child(fields)
            .child(palette_button(
                SharedString::from(format!("openapi-try-it-out-execute-{operation_key}")),
                if is_sending {
                    "Sending…".into()
                } else {
                    "Execute".into()
                },
                ButtonWeight::Primary,
                is_sending,
                *palette,
                cx.listener(move |this, _, window, cx| {
                    this.execute_try_it_out(execute_key.clone(), window, cx);
                }),
            ))
            .child(
                // Clearing drops the task that carries the request, so it stays
                // out of reach until the reply is in: a cleared panel mid-flight
                // would look like nothing had been asked for.
                h_flex().justify_end().child(palette_button(
                    SharedString::from(format!("openapi-try-it-out-clear-{operation_key}")),
                    "Clear".into(),
                    ButtonWeight::Subtle,
                    is_idle || is_sending,
                    *palette,
                    cx.listener(move |this, _, window, cx| {
                        this.clear_try_it_out_response(clear_key.clone(), window, cx);
                    }),
                )),
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

                paint_body_editor_from_palette(&panel.response_body_editor, *palette, cx);
                result = result.child(
                    field_box(FieldStyle::plain(), *palette)
                        .debug_selector(|| "openapi-reply-body".to_string())
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

/// How much weight a palette-painted button carries.
#[derive(Clone, Copy, PartialEq)]
enum ButtonWeight {
    /// The one action a panel exists for: filled with the accent, and as wide as
    /// the panel, the way the reference view paints Execute.
    Primary,
    /// Turning the panel off, and anything else the reader may not want by
    /// accident: outlined in the error colour.
    Danger,
    /// An action worth an outline, but not the accent.
    Outlined,
    /// A side action that only needs a label.
    Subtle,
}

/// The fill of the one action a panel exists for. A dark theme's accent is a
/// pale blue, too pale to carry a white label, so the fill is deepened until it
/// can: the button then looks the same on either page instead of washing out on
/// one of them.
fn primary_fill(accent: Hsla) -> Hsla {
    Hsla {
        l: accent.l.min(0.55),
        ..accent
    }
}

/// The label a filled button carries: white, the way the reference view paints
/// its own, for as long as white can be read on the fill. A theme whose accent
/// is a bright yellow leaves no room for that, and takes a dark label instead.
fn contrasting_text(background: Hsla) -> Hsla {
    let light = gpui::hsla(0., 0., 1., 1.);
    if contrast_ratio(background, light) >= MIN_LABEL_CONTRAST {
        light
    } else {
        gpui::hsla(0., 0., 0.08, 1.)
    }
}

/// The contrast a large, semibold label needs to stay readable.
const MIN_LABEL_CONTRAST: f32 = 3.;

/// The WCAG contrast ratio between two opaque colours.
fn contrast_ratio(one: Hsla, other: Hsla) -> f32 {
    let one = relative_luminance(one);
    let other = relative_luminance(other);
    (one.max(other) + 0.05) / (one.min(other) + 0.05)
}

fn relative_luminance(color: Hsla) -> f32 {
    let rgba = gpui::Rgba::from(color);
    let linear = |channel: f32| {
        if channel <= 0.04045 {
            channel / 12.92
        } else {
            ((channel + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(rgba.r) + 0.7152 * linear(rgba.g) + 0.0722 * linear(rgba.b)
}

/// A button painted from the reading palette. `ui::Button` takes its colours
/// from the editor's theme, which is the wrong palette as soon as the reader
/// picks a different one for the page: a near-black chip on a light page reads
/// as a hole rather than a button.
fn palette_button(
    id: impl Into<ElementId>,
    label: SharedString,
    weight: ButtonWeight,
    disabled: bool,
    palette: Palette,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let (background, border, text) = match weight {
        ButtonWeight::Primary => {
            let fill = primary_fill(palette.accent);
            (fill, fill, contrasting_text(fill))
        }
        ButtonWeight::Danger => (gpui::transparent_black(), palette.error, palette.error),
        ButtonWeight::Outlined => (gpui::transparent_black(), palette.border, palette.text),
        ButtonWeight::Subtle => (
            gpui::transparent_black(),
            gpui::transparent_black(),
            palette.text_muted,
        ),
    };
    let fills_the_row = matches!(weight, ButtonWeight::Primary);
    let hover = match weight {
        ButtonWeight::Primary => primary_fill(palette.accent).opacity(0.85),
        _ => palette.element_hover,
    };
    h_flex()
        .id(id)
        .when(fills_the_row, |this| {
            this.w_full().justify_center().py_1p5()
        })
        .when(!fills_the_row, |this| this.py_0p5())
        .px_2()
        .rounded_md()
        .border_1()
        .border_color(border)
        .bg(background)
        .when(disabled, |this| this.opacity(0.5))
        .when(!disabled, |this| {
            this.cursor_pointer()
                .hover(move |style| style.bg(hover))
                .on_click(move |event, window, cx| on_click(event, window, cx))
        })
        .child(
            Label::new(label)
                .size(LabelSize::Small)
                .weight(gpui::FontWeight::SEMIBOLD)
                .color(Color::Custom(text)),
        )
        .into_any_element()
}

/// How a field-shaped control is laid out and marked.
#[derive(Clone, Copy)]
struct FieldStyle {
    /// A required value gets a heavier border, the way the reference view marks
    /// the parameters that must be filled in.
    required: bool,
    full_width: bool,
}

impl FieldStyle {
    fn plain() -> Self {
        Self {
            required: false,
            full_width: false,
        }
    }

    fn required(required: bool) -> Self {
        Self {
            required,
            full_width: false,
        }
    }

    fn wide(required: bool) -> Self {
        Self {
            required,
            full_width: true,
        }
    }
}

/// The border of a field box. `palette.border` alone is nearly invisible on a
/// light page, which is what made the inputs read as empty space, so the box is
/// drawn from the text colour instead: firm when a value is required, quieter
/// when it is optional.
fn field_border(required: bool, palette: Palette) -> Hsla {
    if required {
        palette.text_muted.opacity(0.9)
    } else {
        palette.text_muted.opacity(0.45)
    }
}

/// The box every fillable control sits in: the page's own brightest surface, a
/// border that is visible on both a light and a dark page, and room to breathe.
fn field_box(style: FieldStyle, palette: Palette) -> gpui::Div {
    div()
        .when(style.full_width, |this| this.w_full())
        .rounded_md()
        .border_1()
        .border_color(field_border(style.required, palette))
        .bg(palette.background)
        .px_2()
        .py_1()
}

/// Which list a click opened, so the open one can be found again on the next
/// render and closed when a value is picked.
#[derive(Clone, PartialEq)]
enum DropdownKey {
    /// The document header's servers list.
    Server,
    /// One enum parameter's allowed values, inside a "Try it out" panel.
    Parameter {
        operation_key: SharedString,
        name: SharedString,
        location: SharedString,
    },
}

/// The list a reader opened and where to hang it.
struct OpenDropdown {
    key: DropdownKey,
    position: gpui::Point<Pixels>,
}

/// The closed state of a dropdown: the value in hand plus a chevron, painted
/// from the reading palette. A `ui::DropdownMenu` would paint its list from the
/// editor's theme instead, which is what left a black list hanging over a light
/// page.
fn palette_dropdown_trigger(
    id: impl Into<ElementId>,
    label: SharedString,
    key: DropdownKey,
    style: FieldStyle,
    palette: Palette,
    cx: &mut Context<OpenApiPreviewView>,
) -> AnyElement {
    let hover = palette.element_hover;
    field_box(style, palette)
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_2()
        .cursor_pointer()
        .hover(move |style| style.bg(hover))
        .child(
            Label::new(label)
                .size(LabelSize::Small)
                .color(Color::Custom(palette.text)),
        )
        .child(
            Icon::new(IconName::ChevronDown)
                .size(IconSize::XSmall)
                .color(Color::Custom(palette.text_muted)),
        )
        .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _, cx| {
            // Hung a little below the click so the list never covers the trigger
            // that opened it.
            let position = event.position() + gpui::point(px(0.), px(14.));
            this.open_dropdown = Some(OpenDropdown {
                key: key.clone(),
                position,
            });
            cx.notify();
        }))
        .into_any_element()
}

/// One row of an open dropdown list.
fn palette_dropdown_entry(
    id: impl Into<ElementId>,
    label: SharedString,
    selected: bool,
    palette: Palette,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let hover = palette.element_hover;
    h_flex()
        .id(id)
        .w_full()
        .px_2()
        .py_1()
        .rounded_sm()
        .cursor_pointer()
        .when(selected, |this| this.bg(palette.element_background))
        .hover(move |style| style.bg(hover))
        .child(
            Label::new(label)
                .size(LabelSize::Small)
                .color(Color::Custom(palette.text)),
        )
        .on_click(move |event, window, cx| on_click(event, window, cx))
        .into_any_element()
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
    cx: &mut App,
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
    cx: &mut App,
) -> impl IntoElement + use<> {
    paint_body_editor_from_palette(&body_editor, *palette, cx);
    field_box(FieldStyle::plain(), *palette)
        .debug_selector(|| "openapi-request-body".to_string())
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
        .bg(palette.surface_background)
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
    _window: &mut Window,
    cx: &mut Context<OpenApiPreviewView>,
) -> AnyElement {
    match &field.input {
        ParameterInput::Text(editor) => palette_input(
            editor.clone(),
            FieldStyle::required(field.required),
            *palette,
            cx,
        ),
        ParameterInput::Select { selected, .. } => render_parameter_enum_dropdown(
            operation_key.clone(),
            field,
            selected.clone(),
            palette,
            cx,
        ),
    }
}

/// A dropdown limited to an enum parameter's allowed values, matching the
/// reference view's own rendering of one. An optional parameter also gets a
/// blank leading entry (picking it clears the value, meaning "don't send
/// this"); a required one does not, since some value has to go out either way.
fn render_parameter_enum_dropdown(
    operation_key: SharedString,
    field: &ParameterField,
    selected: Option<SharedString>,
    palette: &Palette,
    cx: &mut Context<OpenApiPreviewView>,
) -> AnyElement {
    let label = selected.unwrap_or_else(|| "--".into());
    let id = SharedString::from(format!(
        "openapi-param-enum-{operation_key}-{}-{}",
        field.location, field.name
    ));
    palette_dropdown_trigger(
        id,
        label,
        DropdownKey::Parameter {
            operation_key,
            name: field.name.clone(),
            location: field.location.clone(),
        },
        FieldStyle::wide(field.required),
        *palette,
        cx,
    )
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
    let mut row = h_flex().flex_wrap().gap_1().items_center();
    for span in split_code_spans(text) {
        row = match span {
            // One label per word: a whole sentence in a single label wraps on
            // its own and drags the chips around it out of the line, so the
            // paragraph is laid out word by word and the row does the wrapping.
            ProseSpan::Plain(text) => row.children(
                prose_words(&text)
                    .into_iter()
                    .map(|word| Label::new(word).size(size).color(color)),
            ),
            ProseSpan::Code(text) => row.child(code_chip(text, size, palette, cx)),
        };
    }
    row
}

/// The words of a plain prose run, punctuation kept with the word it belongs to.
fn prose_words(text: &SharedString) -> Vec<SharedString> {
    text.split_whitespace()
        .map(|word| SharedString::from(word.to_owned()))
        .collect()
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

/// How tall a body box may grow before the editor scrolls inside it. A
/// `multi_line` editor takes its height from its parent, and the page gives it
/// none -- it would paint as an empty sliver -- so both body editors size
/// themselves to their own text instead.
const BODY_EDITOR_MIN_LINES: usize = 4;
const BODY_EDITOR_MAX_LINES: usize = 24;

/// Sizes a body editor to its own text, and leaves the box to scroll whatever
/// does not fit. No scrollbar: an auto-height editor keeps wrapping its text to
/// the full width, so a thumb would be painted over the end of a long line
/// rather than beside it.
fn new_body_editor(window: &mut Window, cx: &mut Context<Editor>) -> Editor {
    Editor::auto_height(BODY_EDITOR_MIN_LINES, BODY_EDITOR_MAX_LINES, window, cx)
}

fn new_try_it_out_body_editor(
    initial_value: &str,
    window: &mut Window,
    cx: &mut App,
) -> Entity<Editor> {
    cx.new(|cx| {
        let mut editor = new_body_editor(window, cx);
        editor.set_placeholder_text("Request body", window, cx);
        if !initial_value.is_empty() {
            editor.set_text(initial_value.to_string(), window, cx);
        }
        editor
    })
}

fn new_try_it_out_response_editor(window: &mut Window, cx: &mut App) -> Entity<Editor> {
    cx.new(|cx| {
        let mut editor = new_body_editor(window, cx);
        editor.set_read_only(true);
        editor
    })
}

/// An editor paints its text with the editor theme's colour, which is the wrong
/// palette as soon as the reader picks a different one for the page -- light page
/// text stays light on a dark editor theme and the body reads as empty.
fn paint_editor_from_palette(editor: &Entity<Editor>, palette: Palette, cx: &mut App) {
    editor.update(cx, |editor, _| {
        editor.set_text_style_refinement(gpui::TextStyleRefinement {
            color: Some(palette.text),
            ..Default::default()
        });
    });
}

/// A body editor sizes itself to its own text, which also puts it in the UI
/// font -- the wrong face for JSON. Paint it in the buffer font, in the page's
/// own colour.
fn paint_body_editor_from_palette(editor: &Entity<Editor>, palette: Palette, cx: &mut App) {
    use settings::Settings as _;

    let settings = theme_settings::ThemeSettings::get_global(cx);
    let font = settings.buffer_font.clone();
    let font_size = settings.buffer_font_size(cx);
    let refinement = gpui::TextStyleRefinement {
        color: Some(palette.text),
        font_family: Some(font.family),
        font_features: Some(font.features),
        font_fallbacks: font.fallbacks,
        font_size: Some(font_size.into()),
        ..Default::default()
    };
    editor.update(cx, |editor, _| {
        editor.set_text_style_refinement(refinement);
    });
}

/// The box an input sits in, painted from the reading palette, with the editor's
/// own text colour set to match.
fn palette_input(
    editor: Entity<Editor>,
    style: FieldStyle,
    palette: Palette,
    cx: &mut App,
) -> AnyElement {
    paint_editor_from_palette(&editor, palette, cx);
    field_box(style, palette).child(editor).into_any_element()
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
    cx: &mut App,
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
        .child(palette_input(
            editor,
            FieldStyle::required(required),
            *palette,
            cx,
        ))
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
    Get,
    Post,
    Put,
    Delete,
    Patch,
    Head,
    Options,
    Trace,
}

/// Every method carries its own colour on a contract reference page, and a
/// reader recognises the row by that colour before reading a word of it.
fn method_accent(method: HttpMethod) -> MethodAccent {
    match method {
        HttpMethod::Get => MethodAccent::Get,
        HttpMethod::Post => MethodAccent::Post,
        HttpMethod::Put => MethodAccent::Put,
        HttpMethod::Delete => MethodAccent::Delete,
        HttpMethod::Patch => MethodAccent::Patch,
        HttpMethod::Head => MethodAccent::Head,
        HttpMethod::Options => MethodAccent::Options,
        HttpMethod::Trace => MethodAccent::Trace,
    }
}

/// The hues a reference page uses, and their counterparts for a dark page: the
/// light set is too dark to read against near-black, so each one is lifted
/// rather than reused.
/// Text laid over a solid method fill. A light fill needs dark text, and the
/// canonical amber and teal are light.
fn badge_foreground(fill: Hsla) -> Hsla {
    if fill.l > 0.6 {
        gpui::hsla(0., 0., 0.12, 1.)
    } else {
        gpui::white()
    }
}

fn method_accent_color(accent: MethodAccent, appearance: Appearance) -> Hsla {
    let (light, dark) = match accent {
        MethodAccent::Get => (0x61affe, 0x7cc3ff),
        MethodAccent::Post => (0x49cc90, 0x5fe0a4),
        MethodAccent::Put => (0xfca130, 0xffb454),
        MethodAccent::Delete => (0xf93e3e, 0xff6b6b),
        MethodAccent::Patch => (0x50e3c2, 0x6df0d3),
        MethodAccent::Head => (0x9012fe, 0xb267ff),
        MethodAccent::Options => (0x0d5aa7, 0x4d8fd6),
        MethodAccent::Trace => (0x6b6b6b, 0xa0a0a0),
    };
    let value = match appearance {
        Appearance::Light => light,
        Appearance::Dark => dark,
    };
    gpui::rgb(value).into()
}

impl Focusable for OpenApiPreviewView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for OpenApiPreviewView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = resolve_theme(preview_appearance(cx), cx);
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
            .children(
                self.document
                    .clone()
                    .and_then(|document| self.render_open_dropdown(&document, &palette, cx)),
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
    fn method_accent_names_every_verb_separately() {
        assert_eq!(method_accent(HttpMethod::Get), MethodAccent::Get);
        assert_eq!(method_accent(HttpMethod::Post), MethodAccent::Post);
        assert_eq!(method_accent(HttpMethod::Put), MethodAccent::Put);
        assert_eq!(method_accent(HttpMethod::Delete), MethodAccent::Delete);
        assert_eq!(method_accent(HttpMethod::Patch), MethodAccent::Patch);
        assert_eq!(method_accent(HttpMethod::Head), MethodAccent::Head);
        assert_eq!(method_accent(HttpMethod::Options), MethodAccent::Options);
        assert_eq!(method_accent(HttpMethod::Trace), MethodAccent::Trace);
    }

    /// A dark page needs its own set: the light hues are too dark against
    /// near-black, and a badge has to stay readable either way.
    #[test]
    fn every_method_colour_differs_between_a_light_and_a_dark_page() {
        for method in [
            HttpMethod::Get,
            HttpMethod::Post,
            HttpMethod::Put,
            HttpMethod::Delete,
            HttpMethod::Patch,
            HttpMethod::Head,
            HttpMethod::Options,
            HttpMethod::Trace,
        ] {
            let accent = method_accent(method);
            let light = method_accent_color(accent, Appearance::Light);
            let dark = method_accent_color(accent, Appearance::Dark);
            assert_ne!(
                light, dark,
                "{method:?} has to be lifted for a dark page, not reused"
            );
            assert!(
                dark.l > light.l,
                "{method:?} must read lighter against near-black"
            );
            assert!(
                badge_foreground(light).l < 0.3 || badge_foreground(light).l > 0.7,
                "badge text has to commit to dark or light"
            );
        }
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

    const ICONS_CONTRACT: &str = "openapi: 3.0.3\ninfo:\n  title: pd-instruments\n  version: 1.0.0\nservers:\n  - url: http://127.0.0.1:8080\ntags:\n  - name: icons\npaths:\n  /v1/icons/{platform}:\n    delete:\n      tags: [icons]\n      summary: Deletes an icon object.\n      parameters:\n        - $ref: '#/components/parameters/IconPlatformPath'\n      responses:\n        '200':\n          description: ok\ncomponents:\n  parameters:\n    IconPlatformPath:\n      name: platform\n      in: path\n      required: true\n      schema:\n        type: string\n        enum: [web, ios]\n";

    fn saved_requests(
        store: &Entity<ApiClientStore>,
        cx: &mut gpui::TestAppContext,
    ) -> Vec<(String, String, Option<String>)> {
        store.read_with(cx, |store, _| {
            store
                .requests
                .iter()
                .map(|request| {
                    let folder = request.folder_id.and_then(|folder_id| {
                        store
                            .folders
                            .iter()
                            .find(|folder| folder.id == folder_id)
                            .map(|folder| folder.name.clone())
                    });
                    (format!("{:?}", request.method), request.url.clone(), folder)
                })
                .collect()
        })
    }

    /// Saving an operation has to leave a request a reader can actually find: in
    /// the collection named after the contract, inside the folder named after
    /// the tag, with the method and address the contract declares.
    #[gpui::test]
    fn saving_one_operation_puts_a_findable_request_in_the_store(cx: &mut gpui::TestAppContext) {
        let store = cx.new(|cx| ApiClientStore::new(cx));
        let document = crate::openapi_document::parse(ICONS_CONTRACT).expect("parse");
        let key = document.groups[0].operations[0].key();

        let added = cx.update(|cx| {
            let imported = collection_from_document(
                &document,
                OperationSelection::SingleOperation(key.clone()),
            );
            apply_import_to_store(&store, imported, cx)
        });

        assert_eq!(added, 1, "one operation saved has to report one request");
        assert_eq!(
            saved_requests(&store, cx),
            vec![(
                "Delete".to_string(),
                "{{baseUrl}}/v1/icons/{{platform}}".to_string(),
                Some("icons".to_string()),
            )],
            "the request has to be in the tag's folder, with the contract's method and address"
        );
        store.read_with(cx, |store, _| {
            assert_eq!(store.collections.len(), 1);
            assert_eq!(store.collections[0].name, "pd-instruments");
        });
    }

    /// What the reader filled in under "Try it out" is what they mean to save.
    /// The token is the one exception: a collection is written to disk.
    #[test]
    fn saving_carries_the_values_filled_in_for_the_operation() {
        let document = crate::openapi_document::parse(ICONS_CONTRACT).expect("parse");
        let request = collection_from_document(
            &document,
            OperationSelection::SingleOperation(document.groups[0].operations[0].key()),
        );
        let base = request.requests.first().cloned().expect("one request");

        let overrides = try_it_out::TryItOutOverrides {
            server_url: "https://stage.example.com".to_string(),
            auth_header_value: String::new(),
            body_text: None,
            parameters: vec![try_it_out::ParameterOverride {
                name: "platform".to_string(),
                location: "path".to_string(),
                required: true,
                value: "ios".to_string(),
            }],
        };
        let (saved, collection) = try_it_out::apply_overrides(base, request.collection, &overrides);

        assert_eq!(
            collection
                .variables
                .iter()
                .find(|variable| variable.key == "platform")
                .map(|variable| variable.current_value.as_str()),
            Some("ios"),
            "the value chosen for a path parameter has to be saved with the request"
        );
        assert_eq!(
            collection
                .variables
                .iter()
                .find(|variable| variable.key == "baseUrl")
                .map(|variable| variable.current_value.as_str()),
            Some("https://stage.example.com"),
            "the server chosen for the try has to be saved too"
        );
        assert!(
            saved
                .headers
                .iter()
                .all(|header| !header.key.eq_ignore_ascii_case("authorization")),
            "a token must never reach a saved collection"
        );
    }

    /// Saving the same operation twice must not grow a second copy, and must not
    /// claim it saved something when it did not.
    #[gpui::test]
    fn saving_the_same_operation_twice_adds_nothing_the_second_time(cx: &mut gpui::TestAppContext) {
        let store = cx.new(|cx| ApiClientStore::new(cx));
        let document = crate::openapi_document::parse(ICONS_CONTRACT).expect("parse");
        let key = document.groups[0].operations[0].key();

        let mut counts = Vec::new();
        for _ in 0..2 {
            counts.push(cx.update(|cx| {
                let imported = collection_from_document(
                    &document,
                    OperationSelection::SingleOperation(key.clone()),
                );
                apply_import_to_store(&store, imported, cx)
            }));
        }

        assert_eq!(
            counts,
            vec![1, 0],
            "the second save has to report that nothing was added"
        );
        assert_eq!(saved_requests(&store, cx).len(), 1);
        store.read_with(cx, |store, _| {
            assert_eq!(store.collections.len(), 1, "one contract, one collection");
            assert_eq!(store.folders.len(), 1, "one tag, one folder");
        });
    }

    /// A collection that already holds requests -- imported some other way, with
    /// no folders -- must gain the tag folder and the new request, not silently
    /// merge into nothing.
    #[gpui::test]
    fn saving_into_an_existing_collection_adds_the_folder_and_the_request(
        cx: &mut gpui::TestAppContext,
    ) {
        let store = cx.new(|cx| ApiClientStore::new(cx));
        let collection_id = store.update(cx, |store, cx| {
            let id = store.create_collection("pd-instruments".to_string(), cx);
            store.create_request(id, "/v1/icons".to_string(), None, cx);
            id
        });
        let document = crate::openapi_document::parse(ICONS_CONTRACT).expect("parse");
        let key = document.groups[0].operations[0].key();

        let added = cx.update(|cx| {
            let imported =
                collection_from_document(&document, OperationSelection::SingleOperation(key));
            apply_import_to_store(&store, imported, cx)
        });

        assert_eq!(added, 1);
        store.read_with(cx, |store, _| {
            assert_eq!(
                store.collections.len(),
                1,
                "a contract already saved here must not spawn a second collection"
            );
            let folders: Vec<&str> = store
                .folders
                .iter()
                .filter(|folder| folder.collection_id == collection_id)
                .map(|folder| folder.name.as_str())
                .collect();
            assert_eq!(folders, vec!["icons"], "the tag's folder has to be created");
            let saved = store
                .requests
                .iter()
                .find(|request| request.url.contains("/v1/icons/"))
                .expect("the new request has to be in the store");
            assert_eq!(
                saved.folder_id,
                store.folders.first().map(|folder| folder.id),
                "the new request belongs in the tag's folder"
            );
        });
    }

    const TWO_SERVER_CONTRACT: &str = "openapi: 3.0.3\ninfo:\n  title: pd-instruments\n  version: 1.0.0\nservers:\n  - url: http://127.0.0.1:8080\n    description: local\n  - url: https://stage.example.com\n    description: stage\npaths:\n  /v1/icons:\n    get:\n      summary: Lists icons.\n      responses:\n        '200':\n          description: ok\n";

    /// A menu entry runs with the window already borrowed for the click, which is
    /// the context this drives: picking through it has to reach the view, and the
    /// panel already open has to follow the pick.
    #[gpui::test]
    async fn picking_a_server_changes_the_selection_and_the_open_try_it_out_panel(
        cx: &mut gpui::TestAppContext,
    ) {
        init_test(cx);
        let window = cx.add_window(|window, cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.set_text(TWO_SERVER_CONTRACT, window, cx);
            editor
        });
        let editor = window.root(cx).expect("the editor is the window's root");
        let cx = &mut gpui::VisualTestContext::from_window(window.into(), cx);
        let view = cx.update(|window, cx| OpenApiPreviewView::new(editor, window, cx));
        cx.run_until_parked();

        let key = view
            .read_with(cx, |view, _| {
                view.document
                    .as_ref()
                    .map(|document| document.groups[0].operations[0].key())
            })
            .expect("the contract has to parse into one operation");

        view.update_in(cx, |view, window, cx| {
            view.toggle_try_it_out(key.clone(), window, cx);
        });
        cx.run_until_parked();

        let seeded = view.read_with(cx, |view, cx| {
            view.try_it_out_panels[&key].server_editor.read(cx).text(cx)
        });
        assert_eq!(
            seeded, "http://127.0.0.1:8080",
            "the panel starts on the contract's first server"
        );

        // The same call a dropdown row makes: the row runs with the window it was
        // clicked in, and applies the pick through it.
        view.update_in(cx, |view, window, cx| {
            view.apply_dropdown_pick(
                &DropdownKey::Server,
                Some("https://stage.example.com".into()),
                window,
                cx,
            );
        });
        cx.run_until_parked();

        view.read_with(cx, |view, cx| {
            assert_eq!(
                view.selected_server_url.as_deref(),
                Some("https://stage.example.com"),
                "the pick has to be remembered for the next panel that opens"
            );
            assert_eq!(
                view.try_it_out_panels[&key].server_editor.read(cx).text(cx),
                "https://stage.example.com",
                "the panel already open has to follow the pick, since that is what a send reads"
            );
        });
    }

    #[test]
    fn a_filled_button_takes_a_label_that_reads_against_its_own_fill() {
        // The two blues a light and a dark theme actually hand out. Deepened for
        // the button, both have to carry the same white label.
        for accent in [
            Hsla::from(gpui::rgb(0x5c78e2)),
            Hsla::from(gpui::rgb(0x74ade8)),
        ] {
            let fill = primary_fill(accent);
            assert!(
                contrasting_text(fill).l > 0.8,
                "a blue button reads with a light label: {accent:?} -> {fill:?}"
            );
            assert!(
                contrast_ratio(fill, contrasting_text(fill)) >= MIN_LABEL_CONTRAST,
                "the label has to stand out from its own fill: {fill:?}"
            );
        }

        // A theme whose accent is a bright yellow cannot take a white label,
        // however deep the fill is allowed to be.
        let yellow = primary_fill(gpui::hsla(0.15, 0.9, 0.6, 1.));
        assert!(
            contrasting_text(yellow).l < 0.2,
            "a bright fill needs a dark label: {yellow:?}"
        );
    }

    #[test]
    fn a_required_field_is_outlined_more_firmly_than_an_optional_one() {
        let palette = crate::palette::sample_palette();

        let required = field_border(true, palette);
        let optional = field_border(false, palette);

        assert!(
            required.a > optional.a,
            "the required marker has to be visible in the box itself, not only in the label"
        );
        assert!(
            optional.a > 0.4,
            "an optional field still has to read as a box: {optional:?}"
        );
    }

    struct PreviewFrame {
        view: Entity<OpenApiPreviewView>,
    }

    impl Render for PreviewFrame {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(self.view.clone())
        }
    }

    /// A window holding nothing but the preview, so a real mouse event lands on
    /// what the preview actually painted.
    fn preview_window(
        contract: &str,
        cx: &mut gpui::TestAppContext,
    ) -> (
        gpui::WindowHandle<PreviewFrame>,
        Entity<OpenApiPreviewView>,
        gpui::VisualTestContext,
    ) {
        init_test(cx);
        let contract = contract.to_string();
        let window = cx.add_window(move |window, cx| {
            let editor = cx.new(|cx| {
                let mut editor = Editor::multi_line(window, cx);
                editor.set_text(contract, window, cx);
                editor
            });
            PreviewFrame {
                view: OpenApiPreviewView::new(editor, window, cx),
            }
        });
        let view = window
            .read_with(cx, |frame, _| frame.view.clone())
            .expect("the window holds the preview");
        let mut cx = gpui::VisualTestContext::from_window(window.into(), cx);
        draw_preview(window, &mut cx);
        (window, view, cx)
    }

    fn draw_preview(_window: gpui::WindowHandle<PreviewFrame>, cx: &mut gpui::VisualTestContext) {
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        cx.run_until_parked();
    }

    fn first_operation_key(
        view: &Entity<OpenApiPreviewView>,
        cx: &mut gpui::VisualTestContext,
    ) -> SharedString {
        view.read_with(cx, |view, _| {
            view.document
                .as_ref()
                .map(|document| document.groups[0].operations[0].key())
        })
        .expect("the contract has to parse into one operation")
    }

    const SAMPLE_REPLY: &str = "{\n  \"instruments\": [\n    {\n      \"id\": 1001\n    }\n  ]\n}";

    /// A window showing one operation open, its "Try it out" panel filled in, and
    /// a reply in hand -- the state a reader reads the reply in. The reply editor
    /// is left painted in `foreign`, the way a dark editor theme leaves it on a
    /// light page: a colour no reading palette would ever produce.
    fn preview_with_a_reply(
        foreign: Hsla,
        cx: &mut gpui::TestAppContext,
    ) -> (
        gpui::WindowHandle<PreviewFrame>,
        Entity<OpenApiPreviewView>,
        SharedString,
        gpui::VisualTestContext,
    ) {
        let (window, view, mut cx) = preview_window(TWO_SERVER_CONTRACT, cx);
        let key = first_operation_key(&view, &mut cx);

        view.update_in(&mut cx, |view, window, cx| {
            view.toggle_operation(key.clone(), cx);
            view.toggle_try_it_out(key.clone(), window, cx);
        });
        cx.run_until_parked();

        view.update_in(&mut cx, |view, window, cx| {
            let panel = view
                .try_it_out_panels
                .get_mut(&key)
                .expect("the panel was just opened");
            panel.send_state = TryItOutSendState::Success(TryItOutResponseMeta {
                status: 200,
                status_text: "OK".into(),
                elapsed_ms: 12,
                size_bytes: SAMPLE_REPLY.len(),
                headers: Vec::new(),
                body_truncated: false,
            });
            panel.response_body_editor.update(cx, |editor, cx| {
                editor.set_read_only(false);
                editor.set_text(SAMPLE_REPLY, window, cx);
                editor.set_read_only(true);
                editor.set_text_style_refinement(gpui::TextStyleRefinement {
                    color: Some(foreign),
                    ..Default::default()
                });
            });
            cx.notify();
        });
        draw_preview(window, &mut cx);
        (window, view, key, cx)
    }

    /// An editor paints its text with the editor theme's colour unless the render
    /// says otherwise, which is how a reply came out unreadable on a light page.
    #[gpui::test]
    fn the_reply_editor_is_painted_from_the_page_palette(cx: &mut gpui::TestAppContext) {
        let foreign = gpui::hsla(0.85, 1., 0.5, 1.);
        let (_window, view, key, mut cx) = preview_with_a_reply(foreign, cx);

        let page_text =
            cx.update(|_, cx| Palette::from_theme(&resolve_theme(preview_appearance(cx), cx)).text);
        let painted = view.update(&mut cx, |view, cx| {
            view.try_it_out_panels[&key]
                .response_body_editor
                .update(cx, |editor, cx| editor.style(cx).text.color)
        });

        assert_ne!(
            painted, foreign,
            "the render has to repaint the reply, not leave the editor theme's own colour"
        );
        assert_eq!(
            painted, page_text,
            "the reply has to be painted with the page's own text colour"
        );
    }

    /// The reported symptom: a reply arrives, the status line says 200 OK, and the
    /// body is nowhere -- the box paints as a sliver a line high because a
    /// `multi_line` editor takes its height from a parent that offers none.
    #[gpui::test]
    fn the_reply_body_box_paints_at_the_height_of_its_own_text(cx: &mut gpui::TestAppContext) {
        let (_window, _view, _key, mut cx) =
            preview_with_a_reply(gpui::hsla(0.85, 1., 0.5, 1.), cx);

        let bounds = cx
            .debug_bounds("openapi-reply-body")
            .expect("the reply body box has to be painted once a reply is in");

        assert!(
            bounds.size.height > px(48.),
            "a reply of {} lines has to be readable, not a one-line sliver: got {:?}",
            SAMPLE_REPLY.lines().count(),
            bounds.size
        );
        assert!(
            bounds.size.width > px(0.),
            "the reply body box has to occupy real width: got {:?}",
            bounds.size
        );
    }

    /// A reader dismisses an open list by clicking somewhere else on the page --
    /// which is still inside the preview, so the handler cannot live on the page.
    #[gpui::test]
    fn a_click_elsewhere_on_the_page_closes_an_open_dropdown(cx: &mut gpui::TestAppContext) {
        let (window, view, mut cx) = preview_window(TWO_SERVER_CONTRACT, cx);

        view.update(&mut cx, |view, cx| {
            view.open_dropdown = Some(OpenDropdown {
                key: DropdownKey::Server,
                position: gpui::point(px(120.), px(120.)),
            });
            cx.notify();
        });
        draw_preview(window, &mut cx);
        assert!(
            view.read_with(&cx, |view, _| view.open_dropdown.is_some()),
            "the list has to be open before the click that dismisses it"
        );

        let far_corner = cx.update(|window, _| {
            let viewport = window.viewport_size();
            gpui::point(viewport.width - px(24.), viewport.height - px(24.))
        });
        cx.simulate_event(gpui::MouseDownEvent {
            position: far_corner,
            button: gpui::MouseButton::Left,
            modifiers: gpui::Modifiers::none(),
            click_count: 1,
            first_mouse: false,
        });
        cx.run_until_parked();

        assert!(
            view.read_with(&cx, |view, _| view.open_dropdown.is_none()),
            "a click on the page, away from the list, has to dismiss it"
        );
    }

    fn init_test(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
    }

    #[test]
    fn prose_is_laid_out_word_by_word() {
        let words = prose_words(&"Deletes an icon,\n  only when unused.".into());
        assert_eq!(
            words,
            vec![
                SharedString::from("Deletes"),
                SharedString::from("an"),
                SharedString::from("icon,"),
                SharedString::from("only"),
                SharedString::from("when"),
                SharedString::from("unused."),
            ],
            "punctuation stays with its word, and line breaks in the source do not survive"
        );
        assert!(prose_words(&"   ".into()).is_empty());
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
