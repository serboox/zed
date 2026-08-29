use crate::code_generator::Snippet;
use crate::environment_diff_view::{EnvironmentDiffView, WhatIsCompared, side_of_the_comparison};
use crate::response_dock::{
    DockResponseEntry, ResponseDockPanel, SendGeneration, existing_response_tab,
    reveal_response_tab,
};
use crate::response_view::{ResponseData, ResponseTab, SendState};
use crate::store::{ApiClientStore, HistoryExchangeDetail, HistoryExchangeOutcome};
use crate::text_prompt_modal::TextPromptModal;
use api_client::{
    ApiKeyPlacement, AuthConfig, AwsSigV4Config, DYNAMIC_VARIABLE_NAMES, EnvironmentId, Header,
    HistoryEntry, HttpMethod, JwtAlgorithm, JwtAuthConfig, OAuth2Config, OAuth2GrantType,
    QueryParam, RawBodyContentType, Request, RequestBody, RequestId, ResolveMode, SavedExample,
    SystemDynamicVariableSource,
};
use editor::{Editor, EditorEvent, HighlightKey};
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, HighlightStyle, MouseButton,
    MouseDownEvent, Pixels, Point, Render, ScrollHandle, Size, Subscription, WeakEntity, Window,
    point, px,
};
use std::sync::Arc;
use ui::{
    Checkbox, ContextMenu, ContextMenuEntry, DocumentationSide, ElevationIndex, Icon, IconName,
    IconSize, Label, LabelSize, ScrollAxes, Scrollbars, ToggleState, Tooltip, WithScrollbar,
    cyberpunk, prelude::*,
};
use util::ResultExt;
use workspace::{Item, Toast, Workspace, item::ItemEvent, notifications::NotificationId};

/// The enabled `key -> value_for_send()` snapshot a script sees as
/// `pm.environment`/`pm.collectionVariables` when it starts running --
/// taken before the script runs so its changes can be diffed against this
/// afterwards, and only actually-changed keys get written back to the store.
fn variable_maps_for(
    store: &ApiClientStore,
    request: &Request,
) -> (
    std::collections::BTreeMap<String, String>,
    std::collections::BTreeMap<String, String>,
) {
    let environment = store
        .effective_environment_for(request)
        .map(|environment| {
            environment
                .variables
                .iter()
                .filter(|variable| variable.enabled)
                .map(|variable| (variable.key.clone(), variable.value_for_send().to_string()))
                .collect()
        })
        .unwrap_or_default();
    let collection_variables = store
        .collections
        .iter()
        .find(|collection| collection.id == request.collection_id)
        .map(|collection| {
            collection
                .variables
                .iter()
                .filter(|variable| variable.enabled)
                .map(|variable| (variable.key.clone(), variable.value_for_send().to_string()))
                .collect()
        })
        .unwrap_or_default();
    (environment, collection_variables)
}

/// Writes a script's variable changes back to the store: updates
/// `current_value` on existing variables (never `initial_value`, matching
/// `Variable`'s own documented split) and appends brand-new ones the script
/// introduced via `pm.environment.set`/`pm.collectionVariables.set`.
fn apply_script_variable_changes(
    store: &mut ApiClientStore,
    request: &Request,
    before_environment: &std::collections::BTreeMap<String, String>,
    after_environment: &std::collections::BTreeMap<String, String>,
    before_collection: &std::collections::BTreeMap<String, String>,
    after_collection: &std::collections::BTreeMap<String, String>,
    cx: &mut Context<ApiClientStore>,
) {
    if after_environment != before_environment {
        if let Some(effective_environment_id) = store
            .effective_environment_for(request)
            .map(|environment| environment.id)
        {
            store.update_environment(Some(effective_environment_id), cx, |environment| {
                for (key, value) in after_environment {
                    if let Some(variable) = environment
                        .variables
                        .iter_mut()
                        .find(|variable| &variable.key == key)
                    {
                        variable.current_value = value.clone();
                    } else {
                        environment
                            .variables
                            .push(api_client::Variable::new(key.clone(), value.clone()));
                    }
                }
            });
        }
    }
    if after_collection != before_collection {
        store.update_collection(request.collection_id, cx, |collection| {
            for (key, value) in after_collection {
                if let Some(variable) = collection
                    .variables
                    .iter_mut()
                    .find(|variable| &variable.key == key)
                {
                    variable.current_value = value.clone();
                } else {
                    collection
                        .variables
                        .push(api_client::Variable::new(key.clone(), value.clone()));
                }
            }
        });
    }
}

/// The request snapshot a pre-request/test script sees as `pm.request`.
/// Uses the resolved request (post `{{token}}` substitution) once one
/// exists (the test-script phase), otherwise falls back to the raw,
/// unresolved fields -- the pre-request script runs before resolution, so
/// there is no resolved form yet for it to see.
fn script_request_data(
    request: &Request,
    resolved: Option<&api_client::ResolvedRequest>,
) -> api_client::ScriptRequestData {
    match resolved {
        Some(resolved) => api_client::ScriptRequestData {
            method: resolved.method.clone(),
            url: resolved.url.clone(),
            headers: resolved.headers.clone(),
            body: resolved
                .body
                .as_ref()
                .map(|body| String::from_utf8_lossy(body).into_owned())
                .unwrap_or_default(),
        },
        None => api_client::ScriptRequestData {
            method: request.method.as_str().to_string(),
            url: request.url.clone(),
            headers: request
                .headers
                .iter()
                .filter(|header| header.enabled)
                .map(|header| (header.key.clone(), header.value.clone()))
                .collect(),
            body: match &request.body {
                RequestBody::Raw { text, .. } => text.clone(),
                _ => String::new(),
            },
        },
    }
}

/// Highlights every `{{...}}` token (stored or dynamic variable) in `editor`
/// with the theme's accent color, so a request author can see at a glance
/// which parts of a field will be substituted before the request is sent.
fn highlight_variable_tokens(editor: &Entity<Editor>, cx: &mut App) {
    editor.update(cx, |editor, cx| {
        let text = editor.text(cx);
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let mut ranges = Vec::new();
        let mut cursor = 0;
        while let Some(relative_start) = text[cursor..].find("{{") {
            let start = cursor + relative_start;
            let Some(relative_end) = text[start + 2..].find("}}") else {
                break;
            };
            let end = start + 2 + relative_end + 2;
            ranges.push(
                snapshot.anchor_before(multi_buffer::MultiBufferOffset(start))
                    ..snapshot.anchor_after(multi_buffer::MultiBufferOffset(end)),
            );
            cursor = end;
        }
        if ranges.is_empty() {
            editor.clear_highlights(HighlightKey::ApiClientVariableToken, cx);
        } else {
            let accent = cx.theme().colors().text_accent;
            let style = HighlightStyle {
                color: Some(accent),
                background_color: Some(accent.opacity(0.12)),
                ..Default::default()
            };
            editor.highlight_text(HighlightKey::ApiClientVariableToken, ranges, style, cx);
        }
    });
}

fn language_name_for_content_type(content_type: RawBodyContentType) -> Option<&'static str> {
    match content_type {
        RawBodyContentType::Json => Some("JSON"),
        RawBodyContentType::Xml => Some("XML"),
        RawBodyContentType::Html => Some("HTML"),
        RawBodyContentType::JavaScript => Some("JavaScript"),
        RawBodyContentType::Text => None,
    }
}

fn content_type_header_value(content_type: RawBodyContentType) -> &'static str {
    match content_type {
        RawBodyContentType::Json => "application/json",
        RawBodyContentType::Xml => "application/xml",
        RawBodyContentType::Html => "text/html",
        RawBodyContentType::JavaScript => "application/javascript",
        RawBodyContentType::Text => "text/plain",
    }
}

/// A lightweight, non-blocking well-formedness check for the URL field --
/// flags a URL that is clearly broken (no scheme and no `{{variable}}` token
/// that could resolve into one) so the user gets a heads-up before Send
/// rather than only after a failed round trip. Empty is not malformed: the
/// user simply hasn't typed anything yet.
fn url_looks_malformed(url: &str) -> bool {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return false;
    }
    let has_scheme = trimmed.contains("://");
    let starts_with_variable = trimmed.starts_with("{{");
    !has_scheme && !starts_with_variable
}

/// A lightweight, non-blocking JSON well-formedness check for the Raw body
/// editor when its content type is JSON -- surfaces a heads-up near the
/// editor rather than only failing after Send. Empty is not invalid: an
/// empty JSON body is a legitimate (if unusual) request.
fn json_body_is_invalid(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(trimmed).is_err()
}

gpui::actions!(
    api_client,
    [
        /// Moves to the next cell of a params or headers table.
        NextCell,
        /// Moves to the previous cell of a params or headers table.
        PreviousCell
    ]
);

/// A row of a table that the reader cannot type into: an auto-generated header.
/// `toggle` names the one they can still switch off; the two the transport works
/// out for itself (`Content-Length`, `Host`) carry None.
struct FixedRow {
    key: SharedString,
    value: SharedString,
    enabled: bool,
    toggle: Option<usize>,
}

/// The descriptions the rows hold now, by the key they belong to. The Bulk Edit
/// text form has only two columns, so a description would otherwise be thrown
/// away by a trip through it -- and a reader who went in to fix one value would
/// come back out having lost every note they had written.
fn descriptions_by_key(rows: &[KeyValueRow], cx: &App) -> Vec<(String, String)> {
    rows.iter()
        .filter_map(|row| {
            let description = row.description_editor.read(cx).text(cx);
            match description.is_empty() {
                true => None,
                false => Some((row.key_editor.read(cx).text(cx), description)),
            }
        })
        .collect()
}

/// Takes the description that belonged to `key`, so two rows with the same key
/// each get their own rather than sharing the first one.
fn take_description_of(kept: &mut Vec<(String, String)>, key: &str) -> String {
    match kept.iter().position(|(theirs, _)| theirs == key) {
        Some(at) => kept.remove(at).1,
        None => String::new(),
    }
}

/// What an editor says, or nothing at all when it says nothing -- a blank
/// description is an absent description, not an empty string in the file.
fn written_or_nothing(editor: &Entity<Editor>, cx: &App) -> Option<String> {
    let text = editor.read(cx).text(cx);
    match text.is_empty() {
        true => None,
        false => Some(text),
    }
}

fn new_single_line_editor(
    placeholder: &'static str,
    initial_value: &str,
    window: &mut Window,
    cx: &mut App,
) -> Entity<Editor> {
    cx.new(|cx| {
        let mut editor = Editor::single_line(window, cx);
        editor.set_placeholder_text(placeholder, window, cx);
        if !initial_value.is_empty() {
            editor.set_text(initial_value, window, cx);
        }
        editor
    })
}

struct KeyValueRow {
    key_editor: Entity<Editor>,
    value_editor: Entity<Editor>,
    description_editor: Entity<Editor>,
    enabled: bool,
}

impl KeyValueRow {
    /// A row nobody has written anything into. The table always keeps one of
    /// these at the end to type the next row into, and it is not part of the
    /// request until it says something.
    /// Nothing at all was typed into it. Deliberately not `trim`: a cell holding a
    /// space is a cell somebody typed in, and a row must never disappear because
    /// what it holds looks like nothing.
    fn is_blank(&self, cx: &App) -> bool {
        self.key_editor.read(cx).text(cx).is_empty()
            && self.value_editor.read(cx).text(cx).is_empty()
            && self.description_editor.read(cx).text(cx).is_empty()
    }

    fn cell(&self, column: Column) -> &Entity<Editor> {
        match column {
            Column::Key => &self.key_editor,
            Column::Value => &self.value_editor,
            Column::Description => &self.description_editor,
        }
    }
}

/// The three columns of a params/headers table, in the order they are read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Column {
    Key,
    Value,
    Description,
}

impl Column {
    const ALL: [Column; 3] = [Column::Key, Column::Value, Column::Description];

    fn label(self) -> &'static str {
        match self {
            Column::Key => "Key",
            Column::Value => "Value",
            Column::Description => "Description",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Column::Key => "key",
            Column::Value => "value",
            Column::Description => "description",
        }
    }
}

/// Parses Postman-style Bulk Edit text, one row per line: `key: value` is an
/// enabled row; `//key: value` is the same row disabled (commented out, not
/// deleted -- toggling Bulk Edit off and back on preserves it); a `//` line
/// whose remainder has no `:` is a free-form note the user left for
/// themselves and is never sent -- it's intentionally dropped here, since the
/// key-value row view this feeds has no place to show a bare comment.
/// Blank lines are ignored.
fn parse_bulk_key_value_text(text: &str) -> Vec<(String, String, bool)> {
    let mut rows = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let (enabled, content) = match trimmed.strip_prefix("//") {
            Some(rest) => (false, rest),
            None => (true, trimmed),
        };
        let Some((key, value)) = content.split_once(':') else {
            continue;
        };
        // A key that legitimately starts with `//` was escaped with a
        // leading `\` by `key_value_rows_to_bulk_text` so it isn't mistaken
        // for the disabled-row marker above; undo that escape here.
        let key = key.trim().strip_prefix('\\').unwrap_or(key.trim());
        rows.push((key.to_string(), value.trim().to_string(), enabled));
    }
    rows
}

/// The inverse of `parse_bulk_key_value_text`: renders each row as
/// `key: value`, prefixed with `//` when disabled. A key that itself starts
/// with `//` is escaped with a leading `\` so it round-trips instead of
/// being mistaken for the disabled-row marker.
fn key_value_rows_to_bulk_text(rows: &[KeyValueRow], cx: &App) -> String {
    rows.iter()
        // The row waiting to be typed into stands for nothing yet, and a `: ` line
        // in the text form is how it would show up.
        .filter(|row| !row.is_blank(cx))
        .map(|row| {
            let key = row.key_editor.read(cx).text(cx);
            let key = if key.starts_with("//") {
                format!("\\{key}")
            } else {
                key
            };
            let value = row.value_editor.read(cx).text(cx);
            if row.enabled {
                format!("{key}: {value}")
            } else {
                format!("//{key}: {value}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Turns the result of a one-off environment-comparison request into the
/// text shown in the Diff tab: a real diff against `baseline` on success, or
/// an explicit failure message on error -- pulled out of
/// `run_environment_comparison`'s async closure so it's unit-testable
/// without a real network round trip (which GPUI's deterministic test
/// scheduler cannot drive: a background Tokio thread waking a GPUI task is
/// flagged as nondeterministic and panics the test).
fn comparison_diff_text(
    baseline: &ResponseData,
    result: anyhow::Result<api_client::HttpResponseSummary>,
) -> String {
    match result {
        Ok(summary) => {
            let comparison_response = ResponseData::from_summary(summary);
            crate::response_view::response_diff_text(baseline, &comparison_response)
        }
        Err(error) => format!("Comparison request failed: {error}"),
    }
}

/// The two environments Send compares, if the reader asked for a comparison at
/// all.
///
/// The left is where the request goes anyway -- what it is sent to, or the
/// active environment -- and the right is what was asked for. With neither, the
/// asked-for environment stands on both sides: two sends to one environment can
/// answer differently, and that is a fair thing to ask.
///
/// An environment that has since been deleted is no comparison at all, so Send
/// goes back to being a plain send.
fn what_send_compares(
    request: &Request,
    store: &ApiClientStore,
) -> Option<(EnvironmentId, EnvironmentId)> {
    let against = request
        .compared_with()
        .filter(|id| store.environment_by_id(*id).is_some())?;
    let base = store
        .effective_environment_for(request)
        .map(|environment| environment.id)
        .unwrap_or(against);
    Some((base, against))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestTab {
    Params,
    Headers,
    Body,
    Auth,
    Scripts,
    Examples,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyKind {
    None,
    Raw,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthKind {
    Inherit,
    None,
    Basic,
    Bearer,
    ApiKey,
    OAuth2,
    AwsSigV4,
    Jwt,
}

/// Status of the last "Get New Access Token" attempt -- surfaced next to
/// the button so a failed or in-flight token exchange is never silent.
enum OAuth2Status {
    Idle,
    Requesting,
    Success,
    Error(String),
}

pub struct RequestView {
    focus_handle: FocusHandle,
    request_id: RequestId,
    store: Entity<ApiClientStore>,
    workspace: WeakEntity<Workspace>,
    title: SharedString,
    active_tab: RequestTab,
    method: HttpMethod,
    url_editor: Entity<Editor>,
    param_rows: Vec<KeyValueRow>,
    header_rows: Vec<KeyValueRow>,
    params_bulk_edit: bool,
    headers_bulk_edit: bool,
    param_bulk_editor: Entity<Editor>,
    header_bulk_editor: Entity<Editor>,
    body_kind: BodyKind,
    body_content_type: RawBodyContentType,
    body_editor: Entity<Editor>,
    auth_kind: AuthKind,
    auth_username_editor: Entity<Editor>,
    auth_password_editor: Entity<Editor>,
    auth_token_editor: Entity<Editor>,
    auth_api_key_key_editor: Entity<Editor>,
    auth_api_key_value_editor: Entity<Editor>,
    auth_api_key_placement: ApiKeyPlacement,
    oauth2_grant_type: OAuth2GrantType,
    oauth2_auth_url_editor: Entity<Editor>,
    oauth2_token_url_editor: Entity<Editor>,
    oauth2_client_id_editor: Entity<Editor>,
    oauth2_client_secret_editor: Entity<Editor>,
    oauth2_scope_editor: Entity<Editor>,
    oauth2_access_token: String,
    oauth2_refresh_token: String,
    oauth2_status: OAuth2Status,
    aws_access_key_editor: Entity<Editor>,
    aws_secret_key_editor: Entity<Editor>,
    aws_region_editor: Entity<Editor>,
    aws_service_editor: Entity<Editor>,
    aws_session_token_editor: Entity<Editor>,
    jwt_algorithm: JwtAlgorithm,
    jwt_secret_editor: Entity<Editor>,
    jwt_is_secret_base64_encoded: bool,
    jwt_payload_editor: Entity<Editor>,
    jwt_header_prefix_editor: Entity<Editor>,
    jwt_add_to_query_param: bool,
    jwt_query_param_key_editor: Entity<Editor>,
    send_state: SendState,
    /// The shape the code window was last opened on.
    code_snippet_shape: Snippet,
    response_tab: ResponseTab,
    pretty_body_editor: Entity<Editor>,
    raw_body_editor: Entity<Editor>,
    preview_body_editor: Entity<Editor>,
    response_is_html: bool,
    previous_response: Option<ResponseData>,
    diff_body_editor: Entity<Editor>,
    diff_comparison_environment: Option<EnvironmentId>,
    comparing_environment: bool,
    comparison_environment_handle: ui::PopoverMenuHandle<ContextMenu>,
    /// Set while the two sides of an across-environments comparison are in
    /// flight, so the button says so and cannot be pressed again meanwhile.
    comparing_environments: bool,
    /// The picker the Compare chip opens: the same list of environments as the
    /// one beside it, and the same pins.
    comparison_picker: Entity<picker::Picker<crate::environment_picker::EnvironmentPickerDelegate>>,
    environments_comparison_handle:
        ui::PopoverMenuHandle<picker::Picker<crate::environment_picker::EnvironmentPickerDelegate>>,
    pre_request_script_editor: Entity<Editor>,
    test_script_editor: Entity<Editor>,
    test_results: Vec<api_client::TestResult>,
    visualize_data: Option<serde_json::Value>,
    scroll_handle: ScrollHandle,
    /// The picker the chip opens, built once: it holds the reader's search text
    /// and which row they are on, and both belong to the picker rather than to
    /// each opening of it.
    environment_picker:
        Entity<picker::Picker<crate::environment_picker::EnvironmentPickerDelegate>>,
    environment_pin_handle:
        ui::PopoverMenuHandle<picker::Picker<crate::environment_picker::EnvironmentPickerDelegate>>,
    variable_picker_handle: ui::PopoverMenuHandle<ContextMenu>,
    method_selector_handle: ui::PopoverMenuHandle<ContextMenu>,
    auto_header_enabled: Vec<bool>,
    show_auto_headers: bool,
    response_fullscreen: bool,
    url_looks_malformed: bool,
    /// Set when the address bar was typed in, or when the parameter table was.
    /// The two hold the same query string, and whichever moved last decides the
    /// other; `syncing` keeps a write from being read back as a fresh edit.
    url_was_typed_in: bool,
    params_were_typed_in: bool,
    syncing: bool,
    /// The address bar text this view wrote itself. An edit that matches it is that
    /// write arriving back through the editor's subscription a moment later -- not
    /// the reader typing -- and must not be read as a fresh edit. A flag cannot
    /// tell the two apart: the subscription is delivered after the effect cycle
    /// that wrote the text has already ended.
    url_we_wrote: Option<String>,
    body_json_invalid: bool,
    /// Whether the most recent send successfully reached the shared
    /// response dock. When `false` (dock not registered, or the workspace is
    /// gone), `render` falls back to showing the response inline the way
    /// this view always used to, so a response is never silently lost.
    /// The send this view handed to the dock, if any. The dock shows one
    /// response for the whole workspace, so this is what tells the view whether
    /// what is on screen there is still its own reply or somebody else's.
    dock_generation: Option<SendGeneration>,
    /// Which tab the claim above was made against. A closed tab is replaced by a
    /// fresh one that numbers its sends from scratch, so the number alone cannot
    /// tell "my claim is stale" from "someone newer owns the tab".
    dock_tab: Option<gpui::EntityId>,
    _subscriptions: Vec<Subscription>,
}

impl RequestView {
    pub fn new(
        request: &Request,
        store: Entity<ApiClientStore>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let url_editor = new_single_line_editor(
            "https://api.example.com/v1/resource",
            &request.url,
            window,
            cx,
        );

        let (body_kind, body_content_type, body_text) = match &request.body {
            RequestBody::Raw { content_type, text } => (BodyKind::Raw, *content_type, text.clone()),
            _ => (BodyKind::None, RawBodyContentType::Json, String::new()),
        };
        let body_editor = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.set_placeholder_text("Request body", window, cx);
            if !body_text.is_empty() {
                editor.set_text(body_text, window, cx);
            }
            editor
        });

        let (auth_kind, username, password, token, api_key_key, api_key_value, api_key_placement) =
            match &request.auth {
                AuthConfig::Inherit => (
                    AuthKind::Inherit,
                    String::new(),
                    String::new(),
                    String::new(),
                    String::new(),
                    String::new(),
                    ApiKeyPlacement::Header,
                ),
                AuthConfig::None => (
                    AuthKind::None,
                    String::new(),
                    String::new(),
                    String::new(),
                    String::new(),
                    String::new(),
                    ApiKeyPlacement::Header,
                ),
                AuthConfig::Basic { username, password } => (
                    AuthKind::Basic,
                    username.clone(),
                    password.clone(),
                    String::new(),
                    String::new(),
                    String::new(),
                    ApiKeyPlacement::Header,
                ),
                AuthConfig::Bearer { token } => (
                    AuthKind::Bearer,
                    String::new(),
                    String::new(),
                    token.clone(),
                    String::new(),
                    String::new(),
                    ApiKeyPlacement::Header,
                ),
                AuthConfig::ApiKey {
                    key,
                    value,
                    placement,
                } => (
                    AuthKind::ApiKey,
                    String::new(),
                    String::new(),
                    String::new(),
                    key.clone(),
                    value.clone(),
                    *placement,
                ),
                AuthConfig::OAuth2(_) => (
                    AuthKind::OAuth2,
                    String::new(),
                    String::new(),
                    String::new(),
                    String::new(),
                    String::new(),
                    ApiKeyPlacement::Header,
                ),
                AuthConfig::AwsSigV4(_) => (
                    AuthKind::AwsSigV4,
                    String::new(),
                    String::new(),
                    String::new(),
                    String::new(),
                    String::new(),
                    ApiKeyPlacement::Header,
                ),
                AuthConfig::Jwt(_) => (
                    AuthKind::Jwt,
                    String::new(),
                    String::new(),
                    String::new(),
                    String::new(),
                    String::new(),
                    ApiKeyPlacement::Header,
                ),
            };

        let oauth2_config = match &request.auth {
            AuthConfig::OAuth2(config) => config.clone(),
            _ => OAuth2Config::default(),
        };

        let aws_config = match &request.auth {
            AuthConfig::AwsSigV4(config) => config.clone(),
            _ => AwsSigV4Config::default(),
        };

        let jwt_config = match &request.auth {
            AuthConfig::Jwt(config) => config.clone(),
            _ => JwtAuthConfig::default(),
        };

        let auth_username_editor = new_single_line_editor("Username", &username, window, cx);
        let auth_password_editor = new_single_line_editor("Password", &password, window, cx);
        let auth_token_editor = new_single_line_editor("Token", &token, window, cx);
        let auth_api_key_key_editor = new_single_line_editor("Key", &api_key_key, window, cx);
        let auth_api_key_value_editor = new_single_line_editor("Value", &api_key_value, window, cx);
        let oauth2_auth_url_editor =
            new_single_line_editor("Authorization URL", &oauth2_config.auth_url, window, cx);
        let oauth2_token_url_editor =
            new_single_line_editor("Token URL", &oauth2_config.token_url, window, cx);
        let oauth2_client_id_editor =
            new_single_line_editor("Client ID", &oauth2_config.client_id, window, cx);
        let oauth2_client_secret_editor =
            new_single_line_editor("Client Secret", &oauth2_config.client_secret, window, cx);
        let oauth2_scope_editor =
            new_single_line_editor("Scope (optional)", &oauth2_config.scope, window, cx);
        let aws_access_key_editor =
            new_single_line_editor("Access Key ID", &aws_config.access_key, window, cx);
        let aws_secret_key_editor =
            new_single_line_editor("Secret Access Key", &aws_config.secret_key, window, cx);
        let aws_region_editor =
            new_single_line_editor("Region (e.g. us-east-1)", &aws_config.region, window, cx);
        let aws_service_editor = new_single_line_editor(
            "Service (e.g. execute-api)",
            &aws_config.service,
            window,
            cx,
        );
        let aws_session_token_editor = new_single_line_editor(
            "Session Token (optional)",
            &aws_config.session_token,
            window,
            cx,
        );
        let jwt_secret_editor = new_single_line_editor("Secret", &jwt_config.secret, window, cx);
        let jwt_payload_editor = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.set_placeholder_text(r#"{"sub":"user-1"}"#, window, cx);
            if !jwt_config.payload.is_empty() {
                editor.set_text(jwt_config.payload.clone(), window, cx);
            }
            editor
        });
        let jwt_header_prefix_editor =
            new_single_line_editor("Bearer", &jwt_config.header_prefix, window, cx);
        let jwt_query_param_key_editor =
            new_single_line_editor("token", &jwt_config.query_param_key, window, cx);

        let pretty_body_editor = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.set_read_only(true);
            editor
        });
        let raw_body_editor = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.set_read_only(true);
            editor
        });
        let preview_body_editor = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.set_read_only(true);
            editor
        });
        let diff_body_editor = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.set_read_only(true);
            editor
        });
        let pre_request_script_editor = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.set_placeholder_text(
                "// Runs before the request is sent.\n// pm.environment.set(\"key\", \"value\");",
                window,
                cx,
            );
            if !request.pre_request_script.is_empty() {
                editor.set_text(request.pre_request_script.clone(), window, cx);
            }
            editor
        });
        let test_script_editor = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.set_placeholder_text("// Runs after the response arrives.\n// pm.test(\"status is 200\", () => { pm.expect(pm.response.code).to.equal(200); });", window, cx);
            if !request.test_script.is_empty() {
                editor.set_text(request.test_script.clone(), window, cx);
            }
            editor
        });

        let param_bulk_editor = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.set_placeholder_text("key: value, one per line", window, cx);
            editor
        });
        let header_bulk_editor = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.set_placeholder_text("key: value, one per line", window, cx);
            editor
        });

        // Built before the handles are moved into the view: the picker keeps its
        // own copies of both.
        let environment_picker = crate::environment_picker::environment_picker(
            store.clone(),
            workspace.clone(),
            request.id,
            crate::environment_picker::WhatThePickerIsFor::WhereItGoes,
            window,
            cx,
        );
        let comparison_picker = crate::environment_picker::environment_picker(
            store.clone(),
            workspace.clone(),
            request.id,
            crate::environment_picker::WhatThePickerIsFor::WhatToCompareWith,
            window,
            cx,
        );

        let mut this = Self {
            focus_handle: cx.focus_handle(),
            request_id: request.id,
            store,
            workspace,
            title: request.name.clone().into(),
            active_tab: RequestTab::Params,
            method: request.method.clone(),
            url_editor,
            param_rows: Vec::new(),
            header_rows: Vec::new(),
            params_bulk_edit: false,
            headers_bulk_edit: false,
            param_bulk_editor,
            header_bulk_editor,
            body_kind,
            body_content_type,
            body_editor,
            auth_kind,
            auth_username_editor,
            auth_password_editor,
            auth_token_editor,
            auth_api_key_key_editor,
            auth_api_key_value_editor,
            auth_api_key_placement: api_key_placement,
            oauth2_grant_type: oauth2_config.grant_type,
            oauth2_auth_url_editor,
            oauth2_token_url_editor,
            oauth2_client_id_editor,
            oauth2_client_secret_editor,
            oauth2_scope_editor,
            oauth2_access_token: oauth2_config.access_token,
            oauth2_refresh_token: oauth2_config.refresh_token,
            oauth2_status: OAuth2Status::Idle,
            aws_access_key_editor,
            aws_secret_key_editor,
            aws_region_editor,
            aws_service_editor,
            aws_session_token_editor,
            jwt_algorithm: jwt_config.algorithm,
            jwt_secret_editor,
            jwt_is_secret_base64_encoded: jwt_config.is_secret_base64_encoded,
            jwt_payload_editor,
            jwt_header_prefix_editor,
            jwt_add_to_query_param: jwt_config.add_to_query_param,
            jwt_query_param_key_editor,
            send_state: SendState::Idle,
            code_snippet_shape: Snippet::Curl,
            response_tab: ResponseTab::Pretty,
            pretty_body_editor,
            raw_body_editor,
            preview_body_editor,
            response_is_html: false,
            previous_response: None,
            diff_body_editor,
            diff_comparison_environment: None,
            comparing_environment: false,
            comparison_environment_handle: ui::PopoverMenuHandle::default(),
            comparing_environments: false,
            comparison_picker,
            environments_comparison_handle: ui::PopoverMenuHandle::default(),
            pre_request_script_editor,
            test_script_editor,
            test_results: Vec::new(),
            visualize_data: None,
            scroll_handle: ScrollHandle::new(),
            environment_picker,
            environment_pin_handle: ui::PopoverMenuHandle::default(),
            variable_picker_handle: ui::PopoverMenuHandle::default(),
            method_selector_handle: ui::PopoverMenuHandle::default(),
            auto_header_enabled: api_client::AUTO_HEADER_DEFAULTS
                .iter()
                .map(|(key, _)| {
                    !request
                        .settings
                        .disabled_auto_headers
                        .iter()
                        .any(|name| name.trim().eq_ignore_ascii_case(key))
                })
                .collect(),
            show_auto_headers: true,
            response_fullscreen: false,
            url_looks_malformed: false,
            url_was_typed_in: false,
            params_were_typed_in: false,
            syncing: false,
            url_we_wrote: None,
            body_json_invalid: false,
            dock_generation: None,
            dock_tab: None,
            _subscriptions: Vec::new(),
        };

        // The picker's rows come from the store, and an environment can be made
        // or deleted while a request tab is open. Kept in step here rather than
        // rebuilt on every render, which is where a list of a dozen names does
        // not belong.
        let observer = cx.observe_in(&this.store.clone(), window, |this, _store, window, cx| {
            this.environment_picker
                .update(cx, |picker, cx| picker.refresh(window, cx));
            this.comparison_picker
                .update(cx, |picker, cx| picker.refresh(window, cx));
        });
        this._subscriptions.push(observer);

        for param in &request.params {
            this.push_param_row(
                param.key.clone(),
                param.value.clone(),
                param.description.clone().unwrap_or_default(),
                param.enabled,
                window,
                cx,
            );
        }
        for header in &request.headers {
            this.push_header_row(
                header.key.clone(),
                header.value.clone(),
                header.description.clone().unwrap_or_default(),
                header.enabled,
                window,
                cx,
            );
        }

        if this.body_kind == BodyKind::Raw {
            // `RequestView::new` runs inside `cx.new` while callers
            // (`ApiClientPanel::open_request`, `HistoryView::reopen`) are
            // themselves inside `workspace.update(...)`. `sync_body_language`
            // reads the same `Workspace` entity, which double-lease-panics
            // while it's still being updated -- defer it past the end of
            // this effect cycle, once the outer update has released its lease.
            let content_type = this.body_content_type;
            cx.defer_in(window, move |this, window, cx| {
                this.sync_body_language(content_type, window, cx);
            });
        }

        this.watch_editor(this.param_bulk_editor.clone(), window, cx, |this, _, cx| {
            this.persist_params_from_bulk_text(cx);
        });
        this.watch_editor(
            this.header_bulk_editor.clone(),
            window,
            cx,
            |this, _, cx| {
                this.persist_headers_from_bulk_text(cx);
            },
        );

        this.watch_editor(this.url_editor.clone(), window, cx, |this, editor, cx| {
            let url = editor.read(cx).text(cx);
            this.url_looks_malformed = url_looks_malformed(&url);
            match this.url_we_wrote.as_deref() == Some(url.as_str()) {
                true => this.url_we_wrote = None,
                false => this.url_was_typed_in = true,
            }
            let request_id = this.request_id;
            this.store.update(cx, |store, cx| {
                store.update_request(request_id, cx, |request| request.url = url);
            });
        });
        this.watch_editor(this.body_editor.clone(), window, cx, |this, editor, cx| {
            if this.body_content_type == RawBodyContentType::Json {
                let text = editor.read(cx).text(cx);
                this.body_json_invalid = json_body_is_invalid(&text);
            } else {
                this.body_json_invalid = false;
            }
            this.persist_body(cx);
        });
        this.watch_editor(
            this.auth_username_editor.clone(),
            window,
            cx,
            |this, _, cx| {
                this.persist_auth(cx);
            },
        );
        this.watch_editor(
            this.auth_password_editor.clone(),
            window,
            cx,
            |this, _, cx| {
                this.persist_auth(cx);
            },
        );
        this.watch_editor(this.auth_token_editor.clone(), window, cx, |this, _, cx| {
            this.persist_auth(cx);
        });
        this.watch_editor(
            this.auth_api_key_key_editor.clone(),
            window,
            cx,
            |this, _, cx| {
                this.persist_auth(cx);
            },
        );
        this.watch_editor(
            this.auth_api_key_value_editor.clone(),
            window,
            cx,
            |this, _, cx| {
                this.persist_auth(cx);
            },
        );
        this.watch_editor(
            this.oauth2_auth_url_editor.clone(),
            window,
            cx,
            |this, _, cx| {
                this.persist_auth(cx);
            },
        );
        this.watch_editor(
            this.oauth2_token_url_editor.clone(),
            window,
            cx,
            |this, _, cx| {
                this.persist_auth(cx);
            },
        );
        this.watch_editor(
            this.oauth2_client_id_editor.clone(),
            window,
            cx,
            |this, _, cx| {
                this.persist_auth(cx);
            },
        );
        this.watch_editor(
            this.oauth2_client_secret_editor.clone(),
            window,
            cx,
            |this, _, cx| {
                this.persist_auth(cx);
            },
        );
        this.watch_editor(
            this.oauth2_scope_editor.clone(),
            window,
            cx,
            |this, _, cx| {
                this.persist_auth(cx);
            },
        );
        this.watch_editor(
            this.aws_access_key_editor.clone(),
            window,
            cx,
            |this, _, cx| {
                this.persist_auth(cx);
            },
        );
        this.watch_editor(
            this.aws_secret_key_editor.clone(),
            window,
            cx,
            |this, _, cx| {
                this.persist_auth(cx);
            },
        );
        this.watch_editor(this.aws_region_editor.clone(), window, cx, |this, _, cx| {
            this.persist_auth(cx);
        });
        this.watch_editor(
            this.aws_service_editor.clone(),
            window,
            cx,
            |this, _, cx| {
                this.persist_auth(cx);
            },
        );
        this.watch_editor(
            this.aws_session_token_editor.clone(),
            window,
            cx,
            |this, _, cx| {
                this.persist_auth(cx);
            },
        );
        this.watch_editor(this.jwt_secret_editor.clone(), window, cx, |this, _, cx| {
            this.persist_auth(cx);
        });
        this.watch_editor(
            this.jwt_payload_editor.clone(),
            window,
            cx,
            |this, _, cx| {
                this.persist_auth(cx);
            },
        );
        this.watch_editor(
            this.jwt_header_prefix_editor.clone(),
            window,
            cx,
            |this, _, cx| {
                this.persist_auth(cx);
            },
        );
        this.watch_editor(
            this.jwt_query_param_key_editor.clone(),
            window,
            cx,
            |this, _, cx| {
                this.persist_auth(cx);
            },
        );
        this.watch_editor(
            this.pre_request_script_editor.clone(),
            window,
            cx,
            |this, _, cx| {
                this.persist_scripts(cx);
            },
        );
        this.watch_editor(
            this.test_script_editor.clone(),
            window,
            cx,
            |this, _, cx| {
                this.persist_scripts(cx);
            },
        );

        this
    }

    /// Subscribes to `editor`'s buffer edits: refreshes variable-token
    /// highlighting immediately, then runs `on_change` (which is responsible
    /// for persisting the new value back to the store).
    fn watch_editor(
        &mut self,
        editor: Entity<Editor>,
        _window: &mut Window,
        cx: &mut Context<Self>,
        on_change: impl Fn(&mut Self, &Entity<Editor>, &mut Context<Self>) + 'static,
    ) {
        highlight_variable_tokens(&editor, cx);
        let subscription = cx.subscribe(&editor, move |this, editor, event: &EditorEvent, cx| {
            if matches!(event, EditorEvent::BufferEdited) {
                highlight_variable_tokens(&editor, cx);
                on_change(this, &editor, cx);
            }
        });
        self._subscriptions.push(subscription);
    }

    fn push_param_row(
        &mut self,
        key: String,
        value: String,
        description: String,
        enabled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let row = self.new_row("Key", key, value, description, enabled, window, cx);
        for column in Column::ALL {
            let editor = row.cell(column).clone();
            self.watch_editor(editor, window, cx, |this, _, cx| {
                this.persist_params(cx);
            });
        }
        self.param_rows.push(row);
    }

    fn push_header_row(
        &mut self,
        key: String,
        value: String,
        description: String,
        enabled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let row = self.new_row("Header", key, value, description, enabled, window, cx);
        for column in Column::ALL {
            let editor = row.cell(column).clone();
            self.watch_editor(editor, window, cx, |this, _, cx| {
                this.persist_headers(cx);
            });
        }
        self.header_rows.push(row);
    }

    fn new_row(
        &mut self,
        key_placeholder: &'static str,
        key: String,
        value: String,
        description: String,
        enabled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> KeyValueRow {
        KeyValueRow {
            key_editor: new_single_line_editor(key_placeholder, &key, window, cx),
            value_editor: new_single_line_editor("Value", &value, window, cx),
            description_editor: new_single_line_editor("Description", &description, window, cx),
            enabled,
        }
    }

    /// The address bar and the parameter table hold one query string between them:
    /// what is typed after the `?` shows up as rows, and what is written into the
    /// rows shows up after the `?`. Whichever was typed in last decides.
    ///
    /// A row whose key names a place in the path (`:instrument_id`) is not a query
    /// parameter and never goes into the query string.
    fn keep_the_query_and_the_table_in_step(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.syncing || self.params_bulk_edit {
            self.url_was_typed_in = false;
            self.params_were_typed_in = false;
            return;
        }
        let url_moved = std::mem::take(&mut self.url_was_typed_in);
        let params_moved = std::mem::take(&mut self.params_were_typed_in);
        // Nothing is written into the address bar while it is the thing being typed
        // in. Building a row emits an edit of its own a moment later, and answering
        // that by rewriting the address bar would put a `=` in front of the letter
        // the reader is halfway through typing.
        let reader_is_in_the_address_bar = self
            .url_editor
            .read(cx)
            .focus_handle(cx)
            .contains_focused(window, cx);
        self.syncing = true;
        if url_moved {
            self.take_the_query_from_the_url(window, cx);
        } else if params_moved && !reader_is_in_the_address_bar {
            self.write_the_query_into_the_url(window, cx);
        }
        self.syncing = false;
    }

    /// Rebuilds the rows from the address bar: a row for every `:name` place the
    /// path holds, then a row for every pair after the `?`.
    ///
    /// What the table knows and the address bar cannot say -- whether a row is
    /// switched off, its value for a place, its description -- is carried across:
    /// the places by their position, so renaming `:instrument_id` while typing keeps
    /// the value beside it, and the query rows by their key.
    fn take_the_query_from_the_url(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let url = self.url_editor.read(cx).text(cx);
        let places = api_client::path_places(&url);
        let pairs = api_client::query_of(&url)
            .map(api_client::query_pairs)
            .unwrap_or_default();
        let known: Vec<(String, String, bool, String)> = self
            .param_rows
            .iter()
            .filter(|row| !row.is_blank(cx))
            .map(|row| {
                (
                    row.key_editor.read(cx).text(cx),
                    row.value_editor.read(cx).text(cx),
                    row.enabled,
                    row.description_editor.read(cx).text(cx),
                )
            })
            .collect();

        let (known_places, known_query): (Vec<_>, Vec<_>) = known
            .iter()
            .cloned()
            .partition(|(key, _, _, _)| key.starts_with(':'));
        let nothing_moved = places
            .iter()
            .eq(known_places.iter().map(|(key, _, _, _)| key))
            && pairs
                .iter()
                .map(|(key, value)| (key, value))
                .eq(known_query.iter().map(|(key, value, _, _)| (key, value)));
        if nothing_moved {
            return;
        }

        self.param_rows.clear();
        for (at, place) in places.iter().enumerate() {
            // By position, so a place being renamed keeps what was written beside it.
            let (value, enabled, description) = known_places
                .get(at)
                .map(|(_, value, enabled, description)| {
                    (value.clone(), *enabled, description.clone())
                })
                .unwrap_or_else(|| (String::new(), true, String::new()));
            self.push_param_row(place.clone(), value, description, enabled, window, cx);
        }
        for (key, value) in pairs {
            let remembered = known_query
                .iter()
                .find(|(theirs, _, _, _)| theirs == &key)
                .map(|(_, _, enabled, description)| (*enabled, description.clone()));
            let (enabled, description) = remembered.unwrap_or((true, String::new()));
            self.push_param_row(key, value, description, enabled, window, cx);
        }
        self.write_params_to_the_store(cx);
        // Building those rows emits edits of their own, which arrive after this
        // effect cycle and would otherwise read as the reader editing the table.
        cx.defer_in(window, |this, _window, _cx| {
            this.params_were_typed_in = false;
        });
        cx.notify();
    }

    /// Writes the switched-on query rows back into the address bar, leaving the
    /// path, its places and the fragment alone.
    fn write_the_query_into_the_url(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pairs: Vec<(String, String)> = self
            .param_rows
            .iter()
            .filter(|row| !row.is_blank(cx))
            .filter(|row| row.enabled)
            .map(|row| {
                (
                    row.key_editor.read(cx).text(cx),
                    row.value_editor.read(cx).text(cx),
                )
            })
            // A row with an empty key is still a row when something was typed into
            // it -- `?=1` is a query somebody wrote on purpose. The row nobody has
            // typed into is already gone, filtered out as blank above.
            .filter(|(key, _)| !key.starts_with(':'))
            .collect();
        let url = self.url_editor.read(cx).text(cx);
        let written = api_client::url_with_query(&url, &pairs);
        if written == url {
            return;
        }
        self.url_we_wrote = Some(written.clone());
        self.url_editor.update(cx, |editor, cx| {
            editor.set_text(written.clone(), window, cx);
        });
        self.url_looks_malformed = url_looks_malformed(&written);
        let request_id = self.request_id;
        self.store.update(cx, |store, cx| {
            store.update_request(request_id, cx, |request| request.url = written);
        });
        cx.notify();
    }

    /// Keeps exactly one blank row at the end of both tables, which is the row the
    /// next line is typed into -- the way a spreadsheet always has one more row
    /// than it has content.
    fn keep_a_row_to_type_into(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let params_need_one = self
            .param_rows
            .last()
            .map(|row| !row.is_blank(cx))
            .unwrap_or(true);
        if params_need_one {
            self.push_param_row(
                String::new(),
                String::new(),
                String::new(),
                true,
                window,
                cx,
            );
        }
        let headers_need_one = self
            .header_rows
            .last()
            .map(|row| !row.is_blank(cx))
            .unwrap_or(true);
        if headers_need_one {
            self.push_header_row(
                String::new(),
                String::new(),
                String::new(),
                true,
                window,
                cx,
            );
        }
    }

    /// Adds a blank row. The reader adds rows by typing into the blank one the
    /// table keeps at its end; this is how a test seeds several rows at once
    /// without typing into each of them.
    #[cfg(test)]
    fn add_param_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.push_param_row(
            String::new(),
            String::new(),
            String::new(),
            true,
            window,
            cx,
        );
        self.persist_params(cx);
        cx.notify();
    }

    fn remove_param_row(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.param_rows.len() {
            self.param_rows.remove(index);
            self.persist_params(cx);
            cx.notify();
        }
    }

    fn toggle_param_row(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(row) = self.param_rows.get_mut(index) {
            row.enabled = !row.enabled;
            self.persist_params(cx);
            cx.notify();
        }
    }

    #[cfg(test)]
    fn add_header_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.push_header_row(
            String::new(),
            String::new(),
            String::new(),
            true,
            window,
            cx,
        );
        self.persist_headers(cx);
        cx.notify();
    }

    fn remove_header_row(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.header_rows.len() {
            self.header_rows.remove(index);
            self.persist_headers(cx);
            cx.notify();
        }
    }

    fn toggle_header_row(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(row) = self.header_rows.get_mut(index) {
            row.enabled = !row.enabled;
            self.persist_headers(cx);
            cx.notify();
        }
    }

    fn persist_params(&mut self, cx: &mut Context<Self>) {
        if !self.syncing {
            self.params_were_typed_in = true;
        }
        self.write_params_to_the_store(cx);
    }

    fn write_params_to_the_store(&self, cx: &mut Context<Self>) {
        let params: Vec<QueryParam> = self
            .param_rows
            .iter()
            .filter(|row| !row.is_blank(cx))
            .map(|row| QueryParam {
                key: row.key_editor.read(cx).text(cx),
                value: row.value_editor.read(cx).text(cx),
                enabled: row.enabled,
                description: written_or_nothing(&row.description_editor, cx),
            })
            .collect();
        let request_id = self.request_id;
        self.store.update(cx, |store, cx| {
            store.update_request(request_id, cx, |request| request.params = params);
        });
    }

    fn persist_headers(&self, cx: &mut Context<Self>) {
        let headers: Vec<Header> = self
            .header_rows
            .iter()
            .filter(|row| !row.is_blank(cx))
            .map(|row| Header {
                key: row.key_editor.read(cx).text(cx),
                value: row.value_editor.read(cx).text(cx),
                enabled: row.enabled,
                description: written_or_nothing(&row.description_editor, cx),
            })
            .collect();
        let request_id = self.request_id;
        self.store.update(cx, |store, cx| {
            store.update_request(request_id, cx, |request| request.headers = headers);
        });
    }

    fn persist_params_from_bulk_text(&self, cx: &mut Context<Self>) {
        let text = self.param_bulk_editor.read(cx).text(cx);
        let mut kept = descriptions_by_key(&self.param_rows, cx);
        let params: Vec<QueryParam> = parse_bulk_key_value_text(&text)
            .into_iter()
            .map(|(key, value, enabled)| {
                let description = take_description_of(&mut kept, &key);
                QueryParam {
                    key,
                    value,
                    enabled,
                    description: match description.is_empty() {
                        true => None,
                        false => Some(description),
                    },
                }
            })
            .collect();
        let request_id = self.request_id;
        self.store.update(cx, |store, cx| {
            store.update_request(request_id, cx, |request| request.params = params);
        });
    }

    fn persist_headers_from_bulk_text(&self, cx: &mut Context<Self>) {
        let text = self.header_bulk_editor.read(cx).text(cx);
        let mut kept = descriptions_by_key(&self.header_rows, cx);
        let headers: Vec<Header> = parse_bulk_key_value_text(&text)
            .into_iter()
            .map(|(key, value, enabled)| {
                let description = take_description_of(&mut kept, &key);
                Header {
                    key,
                    value,
                    enabled,
                    description: match description.is_empty() {
                        true => None,
                        false => Some(description),
                    },
                }
            })
            .collect();
        let request_id = self.request_id;
        self.store.update(cx, |store, cx| {
            store.update_request(request_id, cx, |request| request.headers = headers);
        });
    }

    /// Switches the Params tab between the row-based editor and a Bulk Edit
    /// textarea. Entering bulk mode seeds the textarea from the current
    /// rows; leaving it parses the textarea and rebuilds the rows from it --
    /// the store itself already reflects the bulk text at every keystroke
    /// via `persist_params_from_bulk_text`, so `send` sees live edits even if
    /// the user never switches back.
    fn toggle_params_bulk_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.params_bulk_edit {
            let text = self.param_bulk_editor.read(cx).text(cx);
            let mut kept = descriptions_by_key(&self.param_rows, cx);
            self.param_rows.clear();
            for (key, value, enabled) in parse_bulk_key_value_text(&text) {
                let description = take_description_of(&mut kept, &key);
                self.push_param_row(key, value, description, enabled, window, cx);
            }
            self.persist_params(cx);
        } else {
            let text = key_value_rows_to_bulk_text(&self.param_rows, cx);
            self.param_bulk_editor.update(cx, |editor, cx| {
                editor.set_text(text, window, cx);
            });
        }
        self.params_bulk_edit = !self.params_bulk_edit;
        cx.notify();
    }

    fn toggle_headers_bulk_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.headers_bulk_edit {
            let text = self.header_bulk_editor.read(cx).text(cx);
            let mut kept = descriptions_by_key(&self.header_rows, cx);
            self.header_rows.clear();
            for (key, value, enabled) in parse_bulk_key_value_text(&text) {
                let description = take_description_of(&mut kept, &key);
                self.push_header_row(key, value, description, enabled, window, cx);
            }
            self.persist_headers(cx);
        } else {
            let text = key_value_rows_to_bulk_text(&self.header_rows, cx);
            self.header_bulk_editor.update(cx, |editor, cx| {
                editor.set_text(text, window, cx);
            });
        }
        self.headers_bulk_edit = !self.headers_bulk_edit;
        cx.notify();
    }

    fn persist_body(&self, cx: &mut Context<Self>) {
        let body = match self.body_kind {
            BodyKind::None => RequestBody::None,
            BodyKind::Raw => RequestBody::Raw {
                content_type: self.body_content_type,
                text: self.body_editor.read(cx).text(cx),
            },
        };
        let request_id = self.request_id;
        self.store.update(cx, |store, cx| {
            store.update_request(request_id, cx, |request| request.body = body);
        });
    }

    fn persist_scripts(&self, cx: &mut Context<Self>) {
        let pre_request_script = self.pre_request_script_editor.read(cx).text(cx);
        let test_script = self.test_script_editor.read(cx).text(cx);
        let request_id = self.request_id;
        self.store.update(cx, |store, cx| {
            store.update_request(request_id, cx, |request| {
                request.pre_request_script = pre_request_script;
                request.test_script = test_script;
            });
        });
    }

    fn persist_auth(&self, cx: &mut Context<Self>) {
        let auth = match self.auth_kind {
            AuthKind::Inherit => AuthConfig::Inherit,
            AuthKind::None => AuthConfig::None,
            AuthKind::Basic => AuthConfig::Basic {
                username: self.auth_username_editor.read(cx).text(cx),
                password: self.auth_password_editor.read(cx).text(cx),
            },
            AuthKind::Bearer => AuthConfig::Bearer {
                token: self.auth_token_editor.read(cx).text(cx),
            },
            AuthKind::ApiKey => AuthConfig::ApiKey {
                key: self.auth_api_key_key_editor.read(cx).text(cx),
                value: self.auth_api_key_value_editor.read(cx).text(cx),
                placement: self.auth_api_key_placement,
            },
            AuthKind::OAuth2 => AuthConfig::OAuth2(OAuth2Config {
                grant_type: self.oauth2_grant_type,
                auth_url: self.oauth2_auth_url_editor.read(cx).text(cx),
                token_url: self.oauth2_token_url_editor.read(cx).text(cx),
                client_id: self.oauth2_client_id_editor.read(cx).text(cx),
                client_secret: self.oauth2_client_secret_editor.read(cx).text(cx),
                scope: self.oauth2_scope_editor.read(cx).text(cx),
                access_token: self.oauth2_access_token.clone(),
                refresh_token: self.oauth2_refresh_token.clone(),
            }),
            AuthKind::AwsSigV4 => AuthConfig::AwsSigV4(AwsSigV4Config {
                access_key: self.aws_access_key_editor.read(cx).text(cx),
                secret_key: self.aws_secret_key_editor.read(cx).text(cx),
                region: self.aws_region_editor.read(cx).text(cx),
                service: self.aws_service_editor.read(cx).text(cx),
                session_token: self.aws_session_token_editor.read(cx).text(cx),
            }),
            AuthKind::Jwt => AuthConfig::Jwt(JwtAuthConfig {
                algorithm: self.jwt_algorithm,
                secret: self.jwt_secret_editor.read(cx).text(cx),
                is_secret_base64_encoded: self.jwt_is_secret_base64_encoded,
                payload: self.jwt_payload_editor.read(cx).text(cx),
                header_prefix: self.jwt_header_prefix_editor.read(cx).text(cx),
                add_to_query_param: self.jwt_add_to_query_param,
                query_param_key: self.jwt_query_param_key_editor.read(cx).text(cx),
            }),
        };
        let request_id = self.request_id;
        self.store.update(cx, |store, cx| {
            store.update_request(request_id, cx, |request| request.auth = auth);
        });
    }

    fn set_method(&mut self, method: HttpMethod, cx: &mut Context<Self>) {
        self.method = method.clone();
        let request_id = self.request_id;
        self.store.update(cx, |store, cx| {
            store.update_request(request_id, cx, |request| request.method = method);
        });
        cx.notify();
    }

    /// Opens a text prompt to set an arbitrary HTTP method (e.g. `PURGE`,
    /// `LOCK`, `MKCOL`) -- the data model already supports
    /// `HttpMethod::Custom`, but the UI only offered the 7 standard chips
    /// until now.
    fn start_custom_method(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let current = match &self.method {
            HttpMethod::Custom(name) => name.clone(),
            _ => String::new(),
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let view = cx.entity();
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                TextPromptModal::new(
                    "Custom HTTP Method",
                    "Set",
                    "Method (e.g. PURGE, LOCK, MKCOL)",
                    &current,
                    Arc::new(move |name, _window, cx| {
                        let trimmed = name.trim().to_uppercase();
                        if trimmed.is_empty() {
                            return;
                        }
                        view.update(cx, |view, cx| {
                            view.set_method(HttpMethod::Custom(trimmed), cx);
                        });
                    }),
                    window,
                    cx,
                )
            });
        });
    }

    fn set_body_kind(&mut self, kind: BodyKind, cx: &mut Context<Self>) {
        self.body_kind = kind;
        self.persist_body(cx);
        cx.notify();
    }

    fn set_body_content_type(
        &mut self,
        content_type: RawBodyContentType,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.body_content_type = content_type;
        self.persist_body(cx);
        self.sync_content_type_header(content_type, window, cx);
        self.sync_body_language(content_type, window, cx);
        self.body_json_invalid = if content_type == RawBodyContentType::Json {
            json_body_is_invalid(&self.body_editor.read(cx).text(cx))
        } else {
            false
        };
        cx.notify();
    }

    /// Attaches the language matching `content_type` to the body editor's
    /// buffer, so switching content types re-highlights the existing text
    /// rather than leaving it in whatever language was attached before.
    /// `Text` has no language (plain text is plain text).
    fn sync_body_language(
        &mut self,
        content_type: RawBodyContentType,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(language_name) = language_name_for_content_type(content_type) else {
            if let Some(buffer) = self.body_editor.read(cx).buffer().read(cx).as_singleton() {
                buffer.update(cx, |buffer, cx| buffer.set_language(None, cx));
            }
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let languages = workspace.read(cx).app_state().languages.clone();
        let language_task = languages.language_for_name(language_name);
        let body_editor = self.body_editor.clone();
        cx.spawn_in(window, async move |_, cx| {
            let language = language_task.await.log_err();
            body_editor.update(cx, |editor, cx| {
                if let Some(buffer) = editor.buffer().read(cx).as_singleton() {
                    buffer.update(cx, |buffer, cx| buffer.set_language(language, cx));
                }
            });
        })
        .detach();
    }

    /// Ensures exactly one `Content-Type` header exists with the value
    /// matching `content_type`, adding it if missing or overwriting an
    /// existing one's value -- switching the Raw body's content type is an
    /// explicit user action, so it's reasonable to always reassert the
    /// matching header rather than leaving a stale value from a previous
    /// content type in place.
    fn sync_content_type_header(
        &mut self,
        content_type: RawBodyContentType,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let value = content_type_header_value(content_type);
        let existing = self.header_rows.iter().position(|row| {
            row.key_editor
                .read(cx)
                .text(cx)
                .eq_ignore_ascii_case("Content-Type")
        });
        match existing {
            Some(index) => {
                let value_editor = self.header_rows[index].value_editor.clone();
                value_editor.update(cx, |editor, cx| {
                    editor.set_text(value, window, cx);
                });
            }
            None => {
                self.push_header_row(
                    "Content-Type".into(),
                    value.into(),
                    String::new(),
                    true,
                    window,
                    cx,
                );
            }
        }
        self.persist_headers(cx);
    }

    /// Pretty-prints the Raw body editor's current text in place, matching
    /// its content type. A no-op (rather than an error) when the text isn't
    /// well-formed JSON/XML -- reuses the same detection `pretty_print_body`
    /// already applies to response bodies, so a request body and a response
    /// body format identically for the same content type.
    fn format_body(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.body_editor.read(cx).text(cx);
        let content_type = content_type_header_value(self.body_content_type);
        let Some((formatted, _)) =
            crate::response_view::pretty_print_body(text.as_bytes(), content_type)
        else {
            return;
        };
        self.body_editor.update(cx, |editor, cx| {
            editor.set_text(formatted, window, cx);
        });
        self.persist_body(cx);
    }

    /// Inserts `text` at every current cursor/selection in `editor`,
    /// replacing any selected text -- the same mechanic a real keystroke or
    /// paste would produce, so a variable picker's insertion behaves exactly
    /// like typing the token by hand.
    fn insert_text_at_cursor(
        editor: &Entity<Editor>,
        text: String,
        _window: &mut Window,
        cx: &mut App,
    ) {
        editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            let ranges: Vec<std::ops::Range<multi_buffer::MultiBufferOffset>> = editor
                .selections
                .all::<multi_buffer::MultiBufferOffset>(&snapshot)
                .into_iter()
                .map(|selection| selection.range())
                .collect();
            editor.edit(ranges.into_iter().map(|range| (range, text.clone())), cx);
        });
    }

    /// A one-line description of what `{{$name}}` actually resolves to,
    /// matching `SystemDynamicVariableSource::resolve_dynamic`'s real
    /// implementation in `variable_resolution.rs` -- shown as a popover
    /// entry's documentation aside so a user can tell what a dynamic
    /// variable does before inserting it, not just its name.
    fn dynamic_variable_description(name: &str) -> &'static str {
        match name {
            "guid" | "randomUUID" => "A randomly generated v4 UUID.",
            "timestamp" => "The current Unix timestamp, in seconds.",
            "isoTimestamp" => "The current time as an ISO 8601 timestamp (YYYY-MM-DDTHH:MM:SSZ).",
            "randomInt" => "A random integer between 0 and 999.",
            "randomEmail" => "A random placeholder email address (firstname.lastname@example.com).",
            "randomFirstName" => "A random first name.",
            "randomLastName" => "A random last name.",
            "randomFullName" => "A random full name.",
            "randomWord" => "A single random placeholder (lorem ipsum-style) word.",
            "randomWords" => "Three random placeholder (lorem ipsum-style) words.",
            "randomIP" => "A random IPv4-shaped address.",
            _ => "",
        }
    }

    /// A dropdown listing every dynamic `$variable` (with a hover
    /// description of what it actually resolves to) and every variable
    /// visible to this request's effective environment plus the global
    /// environment, for inserting a `{{name}}`/`{{$name}}` token into
    /// `editor` at the cursor without having to remember exact spelling.
    fn render_variable_picker(
        &self,
        editor: Entity<Editor>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let store = self.store.read(cx);
        let request = store
            .requests
            .iter()
            .find(|request| request.id == self.request_id)
            .cloned();
        let mut variable_names: Vec<String> = Vec::new();
        if let Some(request) = &request
            && let Some(environment) = store.effective_environment_for(request)
        {
            variable_names.extend(
                environment
                    .variables
                    .iter()
                    .filter(|variable| variable.enabled)
                    .map(|variable| variable.key.clone()),
            );
        }
        for variable in &store.global_environment.variables {
            if variable.enabled && !variable_names.contains(&variable.key) {
                variable_names.push(variable.key.clone());
            }
        }

        div()
            .id("request-variable-picker")
            .debug_selector(|| "request-variable-picker".to_string())
            .child(
                ui::PopoverMenu::new("request-variable-picker-popover")
                    .with_handle(self.variable_picker_handle.clone())
                    .trigger(
                        Button::new("request-variable-picker-trigger", "Variables")
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::Small),
                    )
                    .menu(move |window, cx| {
                        let editor = editor.clone();
                        let variable_names = variable_names.clone();
                        Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                            for name in DYNAMIC_VARIABLE_NAMES {
                                let editor = editor.clone();
                                let token = format!("{{{{${name}}}}}");
                                let description = Self::dynamic_variable_description(name);
                                menu = menu.item(
                                    ContextMenuEntry::new(format!("${name}"))
                                        .documentation_aside(DocumentationSide::Left, {
                                            let description = description.to_string();
                                            move |_cx| {
                                                Label::new(description.clone()).into_any_element()
                                            }
                                        })
                                        .handler(move |window, cx| {
                                            Self::insert_text_at_cursor(
                                                &editor,
                                                token.clone(),
                                                window,
                                                cx,
                                            );
                                        }),
                                );
                            }
                            if !variable_names.is_empty() {
                                menu = menu.separator();
                                for name in &variable_names {
                                    let editor = editor.clone();
                                    let token = format!("{{{{{name}}}}}");
                                    menu = menu.entry(name.clone(), None, move |window, cx| {
                                        Self::insert_text_at_cursor(
                                            &editor,
                                            token.clone(),
                                            window,
                                            cx,
                                        );
                                    });
                                }
                            }
                            menu
                        }))
                    }),
            )
    }

    fn set_auth_kind(&mut self, kind: AuthKind, cx: &mut Context<Self>) {
        self.auth_kind = kind;
        self.persist_auth(cx);
        cx.notify();
    }

    fn set_api_key_placement(&mut self, placement: ApiKeyPlacement, cx: &mut Context<Self>) {
        self.auth_api_key_placement = placement;
        self.persist_auth(cx);
        cx.notify();
    }

    fn set_jwt_algorithm(&mut self, algorithm: JwtAlgorithm, cx: &mut Context<Self>) {
        self.jwt_algorithm = algorithm;
        self.persist_auth(cx);
        cx.notify();
    }

    fn set_jwt_secret_base64_encoded(&mut self, is_base64_encoded: bool, cx: &mut Context<Self>) {
        self.jwt_is_secret_base64_encoded = is_base64_encoded;
        self.persist_auth(cx);
        cx.notify();
    }

    fn set_jwt_add_to_query_param(&mut self, add_to_query_param: bool, cx: &mut Context<Self>) {
        self.jwt_add_to_query_param = add_to_query_param;
        self.persist_auth(cx);
        cx.notify();
    }

    fn set_oauth2_grant_type(&mut self, grant_type: OAuth2GrantType, cx: &mut Context<Self>) {
        self.oauth2_grant_type = grant_type;
        self.persist_auth(cx);
        cx.notify();
    }

    fn current_oauth2_config(&self, cx: &App) -> OAuth2Config {
        OAuth2Config {
            grant_type: self.oauth2_grant_type,
            auth_url: self.oauth2_auth_url_editor.read(cx).text(cx),
            token_url: self.oauth2_token_url_editor.read(cx).text(cx),
            client_id: self.oauth2_client_id_editor.read(cx).text(cx),
            client_secret: self.oauth2_client_secret_editor.read(cx).text(cx),
            scope: self.oauth2_scope_editor.read(cx).text(cx),
            access_token: self.oauth2_access_token.clone(),
            refresh_token: self.oauth2_refresh_token.clone(),
        }
    }

    fn apply_oauth2_token(&mut self, response: api_client::TokenResponse, cx: &mut Context<Self>) {
        self.oauth2_access_token = response.access_token;
        if let Some(refresh_token) = response.refresh_token {
            self.oauth2_refresh_token = refresh_token;
        }
        self.oauth2_status = OAuth2Status::Success;
        self.persist_auth(cx);
        cx.notify();
    }

    fn get_new_access_token(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let config = self.current_oauth2_config(cx);
        let client = self.store.read(cx).http_client.clone();
        self.oauth2_status = OAuth2Status::Requesting;
        cx.notify();

        match config.grant_type {
            OAuth2GrantType::ClientCredentials => {
                cx.spawn_in(window, async move |this, cx| {
                    let request = api_client::client_credentials_token_request(&config);
                    match api_client::exchange_token(&client, &request).await {
                        Ok(response) => {
                            this.update(cx, |this, cx| this.apply_oauth2_token(response, cx))
                                .ok();
                        }
                        Err(error) => {
                            let message = error.to_string();
                            this.update(cx, |this, cx| {
                                this.oauth2_status = OAuth2Status::Error(message);
                                cx.notify();
                            })
                            .ok();
                        }
                    }
                })
                .detach();
            }
            OAuth2GrantType::AuthorizationCodePkce => {
                cx.spawn_in(window, async move |this, cx| {
                    let outcome = async {
                        let (listener, port) =
                            crate::redirect_capture::bind_loopback_port().await?;
                        let redirect_uri = format!("http://127.0.0.1:{port}/callback");
                        let pending = api_client::begin_pkce_authorization();
                        let auth_url =
                            api_client::build_authorization_url(&config, &redirect_uri, &pending);
                        cx.update(|_, cx| cx.open_url(&auth_url))?;
                        let raw_redirect =
                            crate::redirect_capture::accept_one_redirect(listener).await?;
                        let code = api_client::parse_authorization_redirect(
                            &raw_redirect,
                            &pending.state,
                        )?;
                        let request = api_client::authorization_code_token_request(
                            &config,
                            &code,
                            &pending.verifier,
                            &redirect_uri,
                        );
                        api_client::exchange_token(&client, &request).await
                    }
                    .await;
                    match outcome {
                        Ok(response) => {
                            this.update(cx, |this, cx| this.apply_oauth2_token(response, cx))
                                .ok();
                        }
                        Err(error) => {
                            let message = error.to_string();
                            this.update(cx, |this, cx| {
                                this.oauth2_status = OAuth2Status::Error(message);
                                cx.notify();
                            })
                            .ok();
                        }
                    }
                })
                .detach();
            }
        }
    }

    fn delete_example(&mut self, example_id: api_client::ExampleId, cx: &mut Context<Self>) {
        let request_id = self.request_id;
        self.store.update(cx, |store, cx| {
            store.update_request(request_id, cx, |request| {
                request.examples.retain(|example| example.id != example_id);
            });
        });
    }

    /// Prompts for a name, then saves the current response (and the request
    /// that produced it) as a named `SavedExample` on the request -- the
    /// UI-facing counterpart to `SavedExample`, Postman's own "save as
    /// example" flow. Only available while `send_state` holds a real
    /// response; there is nothing to save from `Idle`/`Sending`/`Error`.
    fn toggle_response_fullscreen(&mut self, cx: &mut Context<Self>) {
        self.response_fullscreen = !self.response_fullscreen;
        cx.notify();
    }

    fn save_response_as_example(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let SendState::Success(response) = &self.send_state else {
            return;
        };
        let Some(request) = self
            .store
            .read(cx)
            .requests
            .iter()
            .find(|r| r.id == self.request_id)
            .cloned()
        else {
            return;
        };
        let response_status = response.status;
        let response_headers = response.headers.clone();
        let response_body = String::from_utf8_lossy(&response.body).into_owned();
        let request_id = self.request_id;
        let store = self.store.clone();

        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                TextPromptModal::new(
                    "Save as Example",
                    "Save",
                    "Example name",
                    &format!("{response_status} {}", request.name),
                    Arc::new(move |name, _window, cx| {
                        let example = SavedExample::new(
                            name,
                            request.method.clone(),
                            request.url.clone(),
                            request.headers.clone(),
                            match &request.body {
                                RequestBody::Raw { text, .. } => text.clone(),
                                _ => String::new(),
                            },
                            response_status,
                            response_headers.clone(),
                            response_body.clone(),
                        );
                        store.update(cx, |store, cx| {
                            store.update_request(request_id, cx, |request| {
                                request.examples.push(example);
                            });
                        });
                    }),
                    window,
                    cx,
                )
            });
        });
    }

    /// Opens a preview dialog showing the generated `curl` command with
    /// shell syntax highlighting, so the author can look it over before
    /// deciding whether to copy it -- rather than silently placing it on
    /// the clipboard the instant the button is clicked.
    /// Shows the request as code: whichever shape the reader last asked for, and a
    /// picker for the rest. What is copied is the same request Send would make.
    fn show_as_code(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(request) = self
            .store
            .read(cx)
            .requests
            .iter()
            .find(|r| r.id == self.request_id)
            .cloned()
        else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let languages = workspace.read(cx).app_state().languages.clone();
        let store = self.store.clone();
        let shown = self.code_snippet_shape;
        let view = cx.entity().downgrade();
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                let modal = CodeSnippetModal::new(request, store, languages, shown, window, cx);
                // The shape the reader picked is remembered here, so the window
                // opens on it rather than on cURL again. Where they left the window
                // is remembered by the window itself, for whichever request opens
                // it next.
                cx.observe_release(&cx.entity(), move |_, modal: &mut CodeSnippetModal, cx| {
                    let shown = modal.shown();
                    view.update(cx, |view, _| {
                        view.code_snippet_shape = shown;
                    })
                    .log_err();
                })
                .detach();
                modal
            });
        });
    }

    /// Looks up the workspace's shared response dock, if one is registered.
    /// `None` here is the "dock unavailable" case every `route_*_to_dock`
    /// caller must fall back from -- either the workspace itself is gone, or
    /// (e.g. during startup, before `initialize_panels` finishes) the panel
    /// simply hasn't been added yet.
    fn find_response_dock(&self, cx: &App) -> Option<Entity<ResponseDockPanel>> {
        let workspace = self.workspace.upgrade()?;
        existing_response_tab(workspace.read(cx), cx)
    }

    /// The response tab, opened beside the terminals if it is not there yet.
    /// Every reply lands in that one tab, so the answers sit with the rest of the
    /// output instead of in a panel of their own.
    fn open_response_tab(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<ResponseDockPanel>> {
        let workspace = self.workspace.upgrade()?;
        workspace.update(cx, |workspace, cx| {
            reveal_response_tab(workspace, window, cx)
        })
    }

    fn reveal_response_dock(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_response_tab(window, cx);
    }

    /// Reports that a response could not be handed off to the dock, so it is
    /// never silently lost: the caller already keeps rendering its own copy
    /// inline (see `render`'s use of `response_shown_in_dock`), and this adds
    /// a toast pointing that out, unless the workspace itself is gone (in
    /// which case there is no window left to show a toast in either).
    fn warn_response_dock_unavailable(&self, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            log::warn!(
                "api client: workspace no longer exists; showing the response in the request view instead"
            );
            return;
        };
        workspace.update(cx, |workspace, cx| {
            workspace.show_toast(
                Toast::new(
                    NotificationId::named("api-client-response-dock-missing".into()),
                    "Couldn't open the Response tab -- showing the response here instead.",
                ),
                cx,
            );
        });
    }

    /// Shared tail of every `route_*_to_dock` call: on success, reveals the
    /// dock and marks the response as shown there (dropping the now-moot
    /// inline fullscreen mode); on failure, `render` falls back to showing
    /// the response inline instead, so nothing is lost either way.
    fn route_to_dock(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        apply: impl FnOnce(&mut ResponseDockPanel, &mut Context<ResponseDockPanel>),
    ) {
        match self.open_response_tab(window, cx) {
            Some(dock) => {
                dock.update(cx, apply);
                self.reveal_response_dock(window, cx);
                self.response_fullscreen = false;
            }
            None => {
                self.dock_generation = None;
                self.dock_tab = None;
                self.warn_response_dock_unavailable(cx);
            }
        }
    }

    fn route_sending_to_dock(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let title = self.title.clone();
        let claimed = std::cell::Cell::new(None);
        self.route_to_dock(window, cx, |dock, cx| {
            claimed.set(Some((cx.entity_id(), dock.begin_send(title, cx))));
        });
        if let Some((tab, generation)) = claimed.get() {
            self.dock_generation = Some(generation);
            self.dock_tab = Some(tab);
        }
    }

    /// Claims the tab again when the one this view claimed is no longer there --
    /// the reader closed it mid-send -- because a fresh tab starts its own
    /// numbering and would otherwise dismiss the reply as belonging to an older
    /// send.
    fn reclaim_response_tab_if_replaced(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.find_response_dock(cx).map(|tab| tab.entity_id());
        if self.dock_generation.is_none() || current != self.dock_tab {
            self.route_sending_to_dock(window, cx);
        }
    }

    fn route_error_to_dock(
        &mut self,
        message: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A reply can arrive without this view having claimed the tab -- a
        // saved example replayed straight into `apply_response`, for one -- and
        // it still belongs on screen.
        self.reclaim_response_tab_if_replaced(window, cx);
        let Some(generation) = self.dock_generation else {
            return;
        };
        let title = self.title.clone();
        self.route_to_dock(window, cx, move |dock, cx| {
            dock.show_error(generation, title, message, cx)
        });
    }

    fn route_response_to_dock(
        &mut self,
        entry: DockResponseEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.reclaim_response_tab_if_replaced(window, cx);
        let Some(generation) = self.dock_generation else {
            return;
        };
        self.route_to_dock(window, cx, move |dock, cx| {
            dock.show_response(generation, entry, cx)
        });
    }

    /// True while the dock is showing this view's own reply. Once another
    /// request takes the dock over, this view shows its response inline again
    /// rather than pointing at a panel that has moved on.
    fn response_shown_in_dock(&self, cx: &App) -> bool {
        let Some(generation) = self.dock_generation else {
            return false;
        };
        self.find_response_dock(cx)
            .is_some_and(|dock| dock.read(cx).showing() == generation)
    }

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.send_state, SendState::Sending) {
            return;
        }
        let Some(request) = self
            .store
            .read(cx)
            .requests
            .iter()
            .find(|r| r.id == self.request_id)
            .cloned()
        else {
            return;
        };
        // A comparison asked for in the Compare chip is carried out here rather
        // than when it was asked for: choosing an environment is not sending
        // anything to it, and Send is what the reader presses when they mean it.
        if let Some((base, against)) = what_send_compares(&request, self.store.read(cx)) {
            self.compare_across_environments(base, against, window, cx);
            return;
        }
        let client = self.store.read(cx).http_client.clone();

        self.send_state = SendState::Sending;
        self.test_results.clear();
        self.visualize_data = None;
        self.route_sending_to_dock(window, cx);
        cx.notify();

        let request_id = self.request_id;
        let store = self.store.clone();
        cx.spawn_in(window, async move |this, cx| {
            if !request.pre_request_script.trim().is_empty() {
                let (before_environment, before_collection) =
                    store.update(cx, |store, _| variable_maps_for(store, &request));
                let script_request = script_request_data(&request, None);
                let script = request.pre_request_script.clone();
                let environment_for_script = before_environment.clone();
                let collection_for_script = before_collection.clone();
                let script_result = cx
                    .background_spawn(async move {
                        api_client::run_pre_request_script(
                            &script,
                            &environment_for_script,
                            &collection_for_script,
                            &script_request,
                        )
                    })
                    .await;
                match script_result {
                    Ok(result) => {
                        store.update(cx, |store, cx| {
                            apply_script_variable_changes(
                                store,
                                &request,
                                &before_environment,
                                &result.environment,
                                &before_collection,
                                &result.collection_variables,
                                cx,
                            );
                        });
                    }
                    Err(error) => {
                        let message = format!("Pre-request script failed: {error}");
                        this.update_in(cx, |this, window, cx| {
                            this.send_state = SendState::Error(message.clone());
                            this.route_error_to_dock(message, window, cx);
                            cx.notify();
                        })
                        .ok();
                        return;
                    }
                }
            }

            let (resolved, environment_name) = store.update(cx, |store, _| {
                let context = store.variable_context_for(&request);
                let dynamic = SystemDynamicVariableSource;
                let resolve = |text: &str| {
                    api_client::resolve(text, &context, &dynamic, ResolveMode::ForSend)
                };
                let resolved = api_client::build_resolved_request(&request, &resolve);
                let environment_name = store
                    .effective_environment_for(&request)
                    .map(|environment| environment.name.clone());
                (resolved, environment_name)
            });

            let sent_at_unix_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_millis() as u64)
                .unwrap_or(0);
            let result = api_client::execute(&client, &resolved).await;
            match result {
                Ok(summary) => {
                    let status = summary.status;
                    let response = ResponseData::from_summary(summary);
                    // Cloned before `apply_response` takes ownership of `response`
                    // below -- the history detail needs its own copy of the same
                    // response that just went on screen.
                    let response_for_history = response.clone();

                    if request.test_script.trim().is_empty() {
                        this.update_in(cx, |this, window, cx| {
                            this.apply_response(response, window, cx);
                        })
                        .ok();
                    } else {
                        let (before_environment, before_collection) =
                            store.update(cx, |store, _| variable_maps_for(store, &request));
                        let script_request = script_request_data(&request, Some(&resolved));
                        let script_response = api_client::ScriptResponseData {
                            status: response.status,
                            headers: response.headers.clone(),
                            body: String::from_utf8_lossy(&response.body).into_owned(),
                        };
                        let script = request.test_script.clone();
                        let environment_for_script = before_environment.clone();
                        let collection_for_script = before_collection.clone();
                        let script_result = cx
                            .background_spawn(async move {
                                api_client::run_test_script(
                                    &script,
                                    &environment_for_script,
                                    &collection_for_script,
                                    &script_request,
                                    &script_response,
                                )
                            })
                            .await;
                        if let Ok(result) = &script_result {
                            store.update(cx, |store, cx| {
                                apply_script_variable_changes(
                                    store,
                                    &request,
                                    &before_environment,
                                    &result.environment,
                                    &before_collection,
                                    &result.collection_variables,
                                    cx,
                                );
                            });
                        }
                        this.update_in(cx, |this, window, cx| {
                            match script_result {
                                Ok(result) => {
                                    this.test_results = result.test_results;
                                    this.visualize_data = result.visualize_data;
                                }
                                Err(error) => {
                                    this.test_results = vec![api_client::TestResult {
                                        name: "Tests script".to_string(),
                                        passed: false,
                                        error: Some(error.to_string()),
                                    }];
                                }
                            }
                            this.apply_response(response, window, cx);
                        })
                        .ok();
                    }

                    store.update(cx, |store, cx| {
                        let entry = HistoryEntry::new(
                            request_id,
                            resolved.method.clone(),
                            resolved.url.clone(),
                            Some(status),
                            sent_at_unix_ms,
                        );
                        store.record_history_detail(
                            entry.id,
                            HistoryExchangeDetail {
                                request: resolved,
                                outcome: HistoryExchangeOutcome::Success(response_for_history),
                                environment_name,
                            },
                        );
                        store.record_history_entry(entry, cx);
                    });
                }
                Err(error) => {
                    let message = error.to_string();
                    let message_for_history = message.clone();
                    this.update_in(cx, |this, window, cx| {
                        this.send_state = SendState::Error(message.clone());
                        this.route_error_to_dock(message, window, cx);
                        cx.notify();
                    })
                    .ok();
                    store.update(cx, |store, cx| {
                        let entry = HistoryEntry::new(
                            request_id,
                            resolved.method.clone(),
                            resolved.url.clone(),
                            None,
                            sent_at_unix_ms,
                        );
                        store.record_history_detail(
                            entry.id,
                            HistoryExchangeDetail {
                                request: resolved,
                                outcome: HistoryExchangeOutcome::Error(message_for_history),
                                environment_name,
                            },
                        );
                        store.record_history_entry(entry, cx);
                    });
                }
            }
        })
        .detach();
    }

    fn apply_response(
        &mut self,
        response: ResponseData,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A fresh send makes any prior one-off environment comparison stale
        // (it was diffed against the response this call is about to
        // replace), so fall back to the default "vs Previous Response" mode
        // rather than silently keeping a diff that no longer describes the
        // response now on screen.
        self.diff_comparison_environment = None;
        if let SendState::Success(previous) = &self.send_state {
            let diff_text = crate::response_view::response_diff_text(previous, &response);
            self.diff_body_editor.update(cx, |editor, cx| {
                editor.set_read_only(false);
                editor.set_text(diff_text, window, cx);
                editor.set_read_only(true);
            });
            self.previous_response = Some(previous.clone());
        } else if self.response_tab == ResponseTab::Diff {
            self.response_tab = ResponseTab::Pretty;
        }
        if (self.response_tab == ResponseTab::TestResults && self.test_results.is_empty())
            || (self.response_tab == ResponseTab::Visualize && self.visualize_data.is_none())
        {
            self.response_tab = ResponseTab::Pretty;
        }

        let raw_text = String::from_utf8_lossy(&response.body).into_owned();
        self.raw_body_editor.update(cx, |editor, cx| {
            editor.set_read_only(false);
            editor.set_text(raw_text.clone(), window, cx);
            editor.set_read_only(true);
        });

        self.response_is_html =
            crate::response_view::is_html_content(response.content_type(), &response.body);
        if self.response_is_html {
            let preview_text = crate::response_view::strip_html_to_readable_text(&raw_text);
            self.preview_body_editor.update(cx, |editor, cx| {
                editor.set_read_only(false);
                editor.set_text(preview_text, window, cx);
                editor.set_read_only(true);
            });
        } else if self.response_tab == ResponseTab::Preview {
            self.response_tab = ResponseTab::Pretty;
        }

        let pretty =
            crate::response_view::pretty_print_body(&response.body, response.content_type());
        let (pretty_text, language_name) = match &pretty {
            Some((text, language_name)) => (text.clone(), Some(*language_name)),
            None => (raw_text, None),
        };
        self.pretty_body_editor.update(cx, |editor, cx| {
            editor.set_read_only(false);
            editor.set_text(pretty_text, window, cx);
            editor.set_read_only(true);
        });
        if let Some(language_name) = language_name
            && let Some(workspace) = self.workspace.upgrade()
        {
            let languages = workspace.read(cx).app_state().languages.clone();
            let language_task = languages.language_for_name(language_name);
            let editor = self.pretty_body_editor.clone();
            cx.spawn_in(window, async move |_, cx| {
                let language = language_task.await.log_err();
                editor.update(cx, |editor, cx| {
                    if let Some(buffer) = editor.buffer().read(cx).as_singleton() {
                        buffer.update(cx, |buffer, cx| buffer.set_language(language, cx));
                    }
                });
            })
            .detach();
        }

        let dock_entry = DockResponseEntry {
            request_title: self.title.clone(),
            response: response.clone(),
            response_is_html: self.response_is_html,
            pretty_body_editor: self.pretty_body_editor.clone(),
            raw_body_editor: self.raw_body_editor.clone(),
            preview_body_editor: self.preview_body_editor.clone(),
            test_results: self.test_results.clone(),
            visualize_data: self.visualize_data.clone(),
        };
        self.send_state = SendState::Success(response);
        self.route_response_to_dock(dock_entry, window, cx);
        cx.notify();
    }

    /// Switches the Diff tab's comparison source. `None` returns to the
    /// default "vs Previous Response" mode (recomputed immediately from
    /// whatever `previous_response` is already on hand); `Some(id)` fires a
    /// one-off request against that environment and diffs it against the
    /// response currently on screen.
    fn set_diff_comparison_environment(
        &mut self,
        environment_id: Option<EnvironmentId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.diff_comparison_environment = environment_id;
        match environment_id {
            None => {
                if let (SendState::Success(current), Some(previous)) =
                    (&self.send_state, &self.previous_response)
                {
                    let diff_text = crate::response_view::response_diff_text(previous, current);
                    self.diff_body_editor.update(cx, |editor, cx| {
                        editor.set_read_only(false);
                        editor.set_text(diff_text, window, cx);
                        editor.set_read_only(true);
                    });
                }
                cx.notify();
            }
            Some(environment_id) => self.run_environment_comparison(environment_id, window, cx),
        }
    }

    /// Sends the current request against `environment_id` (bypassing this
    /// request's pinned environment and the store's globally active one --
    /// the user explicitly picked this environment for a single
    /// comparison) and diffs the result against the response already shown,
    /// reusing `response_diff_text` so the compared bodies are
    /// pretty-printed first exactly like the "vs Previous Response" mode.
    fn run_environment_comparison(
        &mut self,
        environment_id: EnvironmentId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let SendState::Success(baseline) = &self.send_state else {
            return;
        };
        let baseline = baseline.clone();
        let Some(request) = self
            .store
            .read(cx)
            .requests
            .iter()
            .find(|r| r.id == self.request_id)
            .cloned()
        else {
            return;
        };
        let client = self.store.read(cx).http_client.clone();
        self.comparing_environment = true;
        cx.notify();

        let store = self.store.clone();
        cx.spawn_in(window, async move |this, cx| {
            let resolved = store.update(cx, |store, _| {
                let context = store.variable_context_for_environment(&request, environment_id);
                let dynamic = SystemDynamicVariableSource;
                let resolve = |text: &str| {
                    api_client::resolve(text, &context, &dynamic, ResolveMode::ForSend)
                };
                api_client::build_resolved_request(&request, &resolve)
            });
            let result = api_client::execute(&client, &resolved).await;
            this.update_in(cx, |this, window, cx| {
                this.comparing_environment = false;
                let diff_text = comparison_diff_text(&baseline, result);
                this.diff_body_editor.update(cx, |editor, cx| {
                    editor.set_read_only(false);
                    editor.set_text(diff_text, window, cx);
                    editor.set_read_only(true);
                });
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The comparison-source selector shown at the top of the Diff tab:
    /// "vs Previous Response" (the default, always-available mode) or "vs
    /// Environment..." which reveals a picker of every environment to fire
    /// a one-off comparison request against.
    fn render_diff_comparison_selector(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let comparison_name = self
            .diff_comparison_environment
            .and_then(|id| store.environment_by_id(id))
            .map(|environment| environment.name.clone());
        let trigger_label: SharedString = if self.comparing_environment {
            "Comparing...".into()
        } else {
            match &comparison_name {
                Some(name) => format!("vs {name}").into(),
                None => "vs Previous Response".into(),
            }
        };
        let environments: Vec<(EnvironmentId, String)> = store
            .environments
            .iter()
            .map(|environment| (environment.id, environment.name.clone()))
            .collect();
        let view = cx.entity();
        let popover_handle = self.comparison_environment_handle.clone();

        div()
            .id("diff-comparison-selector")
            .debug_selector(|| "diff-comparison-selector".to_string())
            .child(
                ui::PopoverMenu::new("diff-comparison-selector-popover")
                    .with_handle(popover_handle)
                    .trigger(
                        Button::new("diff-comparison-selector-trigger", trigger_label)
                            .start_icon(Icon::new(IconName::Diff))
                            .style(if comparison_name.is_some() {
                                ButtonStyle::Tinted(ui::TintColor::Accent)
                            } else {
                                ButtonStyle::Subtle
                            })
                            .disabled(self.comparing_environment),
                    )
                    .menu(move |window, cx| {
                        let view = view.clone();
                        let environments = environments.clone();
                        Some(ContextMenu::build(window, cx, move |menu, _, _| {
                            let menu = menu.entry("vs Previous Response", None, {
                                let view = view.clone();
                                move |window, cx| {
                                    view.update(cx, |view, cx| {
                                        view.set_diff_comparison_environment(None, window, cx);
                                    });
                                }
                            });
                            environments.iter().fold(menu, |menu, (id, name)| {
                                let view = view.clone();
                                let id = *id;
                                menu.entry(format!("vs {name}"), None, move |window, cx| {
                                    view.update(cx, |view, cx| {
                                        view.set_diff_comparison_environment(Some(id), window, cx);
                                    });
                                })
                            })
                        }))
                    }),
            )
    }

    fn render_chip(
        label: &'static str,
        is_selected: bool,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Self, &gpui::ClickEvent, &mut Window, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        Self::render_chip_scoped("request", label, is_selected, cx, on_click)
    }

    /// The semantic accent each HTTP method reads as across the tree, the
    /// method selector, and history -- a fixed mapping so the same verb
    /// always carries the same color everywhere it appears.
    pub(crate) fn method_color(method: &HttpMethod) -> Color {
        match method {
            HttpMethod::Get => Color::Info,
            HttpMethod::Post => Color::Success,
            HttpMethod::Put | HttpMethod::Patch => Color::Warning,
            HttpMethod::Delete => Color::Error,
            HttpMethod::Head | HttpMethod::Options | HttpMethod::Custom(_) => Color::Muted,
        }
    }

    /// Same mapping as `method_color`, keyed by the method's display label --
    /// for call sites (e.g. history entries) that only kept the method as a
    /// plain string rather than a parsed `HttpMethod`.
    pub(crate) fn method_color_for_label(label: &str) -> Color {
        match label {
            "GET" => Color::Info,
            "POST" => Color::Success,
            "PUT" | "PATCH" => Color::Warning,
            "DELETE" => Color::Error,
            _ => Color::Muted,
        }
    }

    /// A compact, flat, color-coded method badge -- e.g. the small "GET" tag
    /// shown next to a request's name in the tree and in history. Not a
    /// button: purely a label, distinct from the selectable method chips in
    /// `render_method_selector`.
    pub(crate) fn render_method_badge(
        label: SharedString,
        color: Color,
        cx: &App,
    ) -> impl IntoElement {
        div()
            .px_1()
            .rounded_sm()
            .bg(color.color(cx).opacity(0.16))
            .child(
                Label::new(label)
                    .size(LabelSize::XSmall)
                    .color(color)
                    .buffer_font(cx),
            )
    }

    /// A single compact dropdown for picking the HTTP method, replacing a
    /// full row of always-visible chips -- frees a whole row of vertical
    /// space for the URL bar, matching the near-universal
    /// `[Method] [URL] [Send]` single-row layout most REST clients use.
    fn render_method_selector(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let method = self.method.clone();
        let color = Self::method_color(&method);
        let label: SharedString = match &method {
            HttpMethod::Custom(name) => name.clone().into(),
            other => other.as_str().to_string().into(),
        };
        let view = cx.entity();
        let popover_handle = self.method_selector_handle.clone();
        let tint = color.color(cx);

        div()
            .id("request-method-selector")
            .debug_selector(|| "request-method-selector".to_string())
            .child(
                ui::PopoverMenu::new("request-method-selector-popover")
                    .with_handle(popover_handle)
                    .trigger(
                        ui::ButtonLike::new("request-method-selector-trigger")
                            .style(ButtonStyle::Subtle)
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap_1()
                                    .px_1()
                                    .rounded_md()
                                    .bg(tint.opacity(0.16))
                                    .child(
                                        Label::new(label)
                                            .size(LabelSize::Small)
                                            .color(color)
                                            .buffer_font(cx),
                                    )
                                    .child(
                                        Icon::new(IconName::ChevronDown)
                                            .size(IconSize::XSmall)
                                            .color(color),
                                    ),
                            ),
                    )
                    .menu(move |window, cx| {
                        let view = view.clone();
                        Some(ContextMenu::build(window, cx, move |menu, _, _| {
                            let entry =
                                |menu: ContextMenu, label: &'static str, method: HttpMethod| {
                                    let view = view.clone();
                                    menu.entry(label, None, move |_window, cx| {
                                        view.update(cx, |view, cx| {
                                            view.set_method(method.clone(), cx)
                                        });
                                    })
                                };
                            let menu = entry(menu, "GET", HttpMethod::Get);
                            let menu = entry(menu, "POST", HttpMethod::Post);
                            let menu = entry(menu, "PUT", HttpMethod::Put);
                            let menu = entry(menu, "PATCH", HttpMethod::Patch);
                            let menu = entry(menu, "DELETE", HttpMethod::Delete);
                            let menu = entry(menu, "HEAD", HttpMethod::Head);
                            let menu = entry(menu, "OPTIONS", HttpMethod::Options);
                            menu.entry("Custom...", None, {
                                let view = view.clone();
                                move |window, cx| {
                                    view.update(cx, |view, cx| {
                                        view.start_custom_method(window, cx);
                                    });
                                }
                            })
                        }))
                    }),
            )
    }

    /// Sends this request to two environments at once and opens a tab that
    /// diffs the two answers. Both are sent fresh and together: comparing
    /// stage against production is only worth anything if both were asked the
    /// same question at the same moment.
    fn compare_across_environments(
        &mut self,
        left_id: EnvironmentId,
        right_id: EnvironmentId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The same environment on both sides is allowed on purpose: two sends
        // to one environment can answer differently, and telling whether they
        // do is exactly what a reader asks this for.
        if self.comparing_environments {
            return;
        }
        let Some(request) = self
            .store
            .read(cx)
            .requests
            .iter()
            .find(|candidate| candidate.id == self.request_id)
            .cloned()
        else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            self.warn_response_dock_unavailable(cx);
            return;
        };
        let store = self.store.clone();
        let client = store.read(cx).http_client.clone();
        let names = |cx: &App| {
            let store = store.read(cx);
            (
                store
                    .environment_by_id(left_id)
                    .map(|environment| SharedString::from(environment.name.clone()))
                    .unwrap_or_else(|| "Left".into()),
                store
                    .environment_by_id(right_id)
                    .map(|environment| SharedString::from(environment.name.clone()))
                    .unwrap_or_else(|| "Right".into()),
            )
        };
        let (left_name, right_name) = names(cx);
        let compared = WhatIsCompared {
            request_id: self.request_id,
            left: left_id,
            right: right_id,
        };
        let title = self.title.clone();
        self.comparing_environments = true;
        cx.notify();

        let workspace = workspace.downgrade();
        cx.spawn_in(window, async move |this, cx| {
            let mut resolve_against = |environment_id: EnvironmentId| {
                store.update(cx, |store, _| {
                    let context = store.variable_context_for_environment(&request, environment_id);
                    let dynamic = SystemDynamicVariableSource;
                    let resolve = |text: &str| {
                        api_client::resolve(text, &context, &dynamic, ResolveMode::ForSend)
                    };
                    api_client::build_resolved_request(&request, &resolve)
                })
            };
            let left_request = resolve_against(left_id);
            let right_request = resolve_against(right_id);
            let (left_result, right_result) = smol::future::zip(
                api_client::execute(&client, &left_request),
                api_client::execute(&client, &right_request),
            )
            .await;

            this.update(cx, |this, cx| {
                this.comparing_environments = false;
                cx.notify();
            })
            .ok();

            let opened = EnvironmentDiffView::open(
                compared,
                title,
                side_of_the_comparison(left_name, left_result),
                side_of_the_comparison(right_name, right_result),
                store.clone(),
                workspace,
                cx,
            )
            .await;
            if let Err(error) = opened {
                log::error!("failed to open the environment comparison: {error}");
            }
        })
        .detach();
    }

    /// The chip that says which environment the next send is compared against,
    /// and opens the same list of environments the other chip does -- the same
    /// pins, the same search, and every environment in it, including the one the
    /// request is already sent to: two sends to one environment can answer
    /// differently, and that is worth being able to ask.
    fn render_environments_comparison(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let store = self.store.read(cx);
        let compared_with = store
            .requests
            .iter()
            .find(|request| request.id == self.request_id)
            .and_then(|request| request.compared_with())
            .and_then(|id| store.environment_by_id(id));
        let label: SharedString = match (&compared_with, self.comparing_environments) {
            (_, true) => "Comparing...".into(),
            (Some(environment), false) => format!("vs {}", environment.name).into(),
            (None, false) => "Compare".into(),
        };
        let armed = compared_with.is_some();
        let picker = self.comparison_picker.clone();
        let popover_handle = self.environments_comparison_handle.clone();
        let shown_on_the_chip = label.clone();

        div()
            .id("request-environments-comparison")
            .debug_selector(|| "request-environments-comparison".to_string())
            .child(
                div()
                    .debug_selector(move || {
                        format!("request-environments-comparison:{shown_on_the_chip}")
                    })
                    .child(
                        picker::popover_menu::PickerPopoverMenu::new(
                            picker,
                            Button::new("request-environments-comparison-trigger", label)
                                .start_icon(Icon::new(IconName::Diff).size(IconSize::Small))
                                .style(match armed {
                                    true => ButtonStyle::Tinted(ui::TintColor::Accent),
                                    false => ButtonStyle::Subtle,
                                })
                                .disabled(self.comparing_environments),
                            move |_window, cx| {
                                Tooltip::simple("Compare sends against another environment", cx)
                            },
                            gpui::Anchor::TopLeft,
                            cx,
                        )
                        .with_handle(popover_handle)
                        .render(window, cx),
                    ),
            )
    }

    /// The chip that says which environment this request is sent to, and opens
    /// the picker where that is chosen and environments are pinned.
    fn render_environment_pin(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let store = self.store.read(cx);
        let chosen = store
            .requests
            .iter()
            .find(|request| request.id == self.request_id)
            .and_then(|request| request.chosen_environment())
            .and_then(|id| store.environment_by_id(id));
        let label: SharedString = match chosen {
            Some(environment) => environment.name.clone().into(),
            None => "Active Environment".into(),
        };
        let is_chosen = chosen.is_some();
        let picker = self.environment_picker.clone();
        let popover_handle = self.environment_pin_handle.clone();

        // The label goes into a selector of its own so a test can read what the
        // chip actually says, which is the only place the choice is named.
        let shown_on_the_chip = label.clone();
        div()
            .id("request-environment-pin")
            .debug_selector(|| "request-environment-pin".to_string())
            .child(
                div()
                    .debug_selector(move || format!("request-environment-pin:{shown_on_the_chip}"))
                    .child(
                        picker::popover_menu::PickerPopoverMenu::new(
                            picker,
                            Button::new("request-environment-pin-trigger", label)
                                .start_icon(Icon::new(IconName::Pin))
                                .style(if is_chosen {
                                    ButtonStyle::Tinted(ui::TintColor::Accent)
                                } else {
                                    ButtonStyle::Subtle
                                }),
                            move |_window, cx| Tooltip::simple("Environment for this request", cx),
                            gpui::Anchor::TopLeft,
                            cx,
                        )
                        .with_handle(popover_handle)
                        .render(window, cx),
                    ),
            )
    }

    /// Same visual chip as `render_chip`, but with a caller-supplied `scope`
    /// folded into the element id/debug selector -- needed wherever two chip
    /// strips can show the same label (e.g. the request tab strip's
    /// "Headers" and the response tab strip's "Headers" would otherwise
    /// collide on one element id).
    fn render_chip_scoped(
        scope: &'static str,
        label: &'static str,
        is_selected: bool,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Self, &gpui::ClickEvent, &mut Window, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        let colors = cx.theme().colors();
        div()
            .id(SharedString::from(format!("{scope}-chip-{label}")))
            .debug_selector(move || format!("{scope}-chip-{label}"))
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

    fn render_field(
        label: SharedString,
        editor: Entity<Editor>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let colors = cx.theme().colors();
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
            .child(
                div()
                    .w_full()
                    .px_2()
                    .py_1p5()
                    .rounded_md()
                    .border_1()
                    .border_color(colors.border)
                    .bg(colors.background)
                    .child(editor),
            )
    }

    fn toggle_auto_header(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(enabled) = self.auto_header_enabled.get_mut(index) {
            *enabled = !*enabled;
        }
        self.persist_disabled_auto_headers(cx);
        cx.notify();
    }

    fn persist_disabled_auto_headers(&self, cx: &mut Context<Self>) {
        let disabled: Vec<String> = api_client::AUTO_HEADER_DEFAULTS
            .iter()
            .zip(self.auto_header_enabled.iter())
            .filter(|(_, enabled)| !**enabled)
            .map(|((key, _), _)| key.to_string())
            .collect();
        let request_id = self.request_id;
        self.store.update(cx, |store, cx| {
            store.update_request(request_id, cx, |request| {
                request.settings.disabled_auto_headers = disabled;
            });
        });
    }

    fn toggle_show_auto_headers(&mut self, cx: &mut Context<Self>) {
        self.show_auto_headers = !self.show_auto_headers;
        cx.notify();
    }

    /// Every currently-enabled auto-generated header's `(key, value)` pair --
    /// the exact set `build_resolved_request` layers on top of the user's own
    /// headers at send time (skipping any the user already set explicitly
    /// under the same name). Test-only: lets tests assert on the enabled set
    /// without duplicating the zip/filter over `auto_header_enabled`.
    #[cfg(test)]
    fn enabled_auto_headers(&self) -> Vec<(String, String)> {
        api_client::AUTO_HEADER_DEFAULTS
            .iter()
            .zip(self.auto_header_enabled.iter())
            .filter(|(_, enabled)| **enabled)
            .map(|((key, value), _)| (key.to_string(), value.to_string()))
            .collect()
    }

    /// The auto-generated headers as rows of the same table the reader's own
    /// headers live in: what the request sends, read in one place, rather than a
    /// list above a table.
    ///
    /// Each is switchable except the two the transport works out for itself
    /// (`Content-Length`, `Host`), which are there to be read.
    fn automatic_header_rows(&self) -> Vec<FixedRow> {
        if !self.show_auto_headers {
            return Vec::new();
        }
        let mut rows: Vec<FixedRow> = api_client::AUTO_HEADER_DEFAULTS
            .iter()
            .enumerate()
            .map(|(index, (key, value))| FixedRow {
                key: SharedString::from(*key),
                value: SharedString::from(*value),
                enabled: self.auto_header_enabled.get(index).copied().unwrap_or(true),
                toggle: Some(index),
            })
            .collect();
        for key in ["Content-Length", "Host"] {
            rows.push(FixedRow {
                key: SharedString::from(key),
                value: SharedString::from("<calculated when request is sent>"),
                enabled: true,
                toggle: None,
            });
        }
        rows
    }

    /// The switch that shows or hides those rows.
    fn render_auto_headers_switch(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("hide-auto-headers-toggle")
            .debug_selector(|| "hide-auto-headers-toggle".to_string())
            .cursor_pointer()
            .child(
                Label::new(match self.show_auto_headers {
                    true => "Hide auto-generated headers",
                    false => "Show auto-generated headers",
                })
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .on_click(cx.listener(|this, _, _window, cx| this.toggle_show_auto_headers(cx)))
            .into_any_element()
    }

    fn render_bulk_edit_toggle(
        bulk_edit_active: bool,
        toggle_id: &'static str,
        on_toggle: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id(toggle_id)
            .debug_selector(move || toggle_id.to_string())
            .cursor_pointer()
            .child(
                Label::new(if bulk_edit_active {
                    "Key-Value Edit"
                } else {
                    "Bulk Edit"
                })
                .size(LabelSize::Small)
                .color(Color::Accent),
            )
            .on_click(cx.listener(move |this, _, window, cx| on_toggle(this, window, cx)))
            .into_any_element()
    }

    fn render_bulk_editor(
        editor: Entity<Editor>,
        selector_id: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        div()
            .id(selector_id)
            .debug_selector(move || selector_id.to_string())
            .w_full()
            .min_h(px(160.))
            .px_2()
            .py_1p5()
            .rounded_md()
            .border_1()
            .border_color(colors.border)
            .bg(colors.background)
            .child(editor)
            .into_any_element()
    }

    /// Params and headers as a table with lines, the way a spreadsheet shows rows:
    /// a heading over each column, one row a line, and a blank row at the end to
    /// type the next one into. Cells share their borders, so the table reads as a
    /// grid rather than as a stack of separate boxes.
    fn render_key_value_rows(
        rows: &[KeyValueRow],
        which: &'static str,
        fixed: Vec<FixedRow>,
        on_toggle_fixed: impl Fn(&mut Self, usize, &mut Context<Self>) + 'static + Clone,
        on_toggle: impl Fn(&mut Self, usize, &mut Context<Self>) + 'static + Clone,
        on_remove: impl Fn(&mut Self, usize, &mut Context<Self>) + 'static + Clone,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let colors = cx.theme().colors();
        let last = rows.len().saturating_sub(1);
        let mut table = v_flex()
            .id(SharedString::from(format!("{which}-table")))
            .debug_selector(move || format!("{which}-table"))
            .key_context("ApiClientKeyValueTable")
            .w_full()
            .border_1()
            .border_color(colors.border)
            .child(
                h_flex()
                    .w_full()
                    .bg(colors.title_bar_background)
                    .child(div().w(px(26.)).flex_none())
                    .children(Column::ALL.map(|column| {
                        div()
                            .flex_1()
                            .min_w_0()
                            .px_2()
                            .py_1()
                            .border_l_1()
                            .border_color(colors.border)
                            .debug_selector(move || format!("{which}-heading-{}", column.name()))
                            .child(
                                Label::new(column.label())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                    }))
                    .child(div().w(px(26.)).flex_none()),
            );

        for automatic in fixed {
            let on_toggle_fixed = on_toggle_fixed.clone();
            let key = automatic.key.clone();
            let switchable = automatic.toggle;
            table = table.child(
                h_flex()
                    .id(SharedString::from(format!("{which}-fixed-{key}")))
                    .debug_selector({
                        let key = key.clone();
                        move || format!("{which}-fixed-{key}")
                    })
                    .w_full()
                    .items_stretch()
                    .border_t_1()
                    .border_color(colors.border)
                    .child(
                        div()
                            .id(SharedString::from(format!("{which}-fixed-toggle-{key}")))
                            .w(px(26.))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .debug_selector({
                                let key = key.clone();
                                move || format!("auto-header-toggle-{key}")
                            })
                            .when_some(switchable, |cell, index| {
                                cell.cursor_pointer().on_click(cx.listener(
                                    move |this, _, _window, cx| on_toggle_fixed(this, index, cx),
                                ))
                            })
                            .child(
                                Icon::new(match automatic.enabled {
                                    true => IconName::Check,
                                    false => IconName::Close,
                                })
                                .size(IconSize::XSmall)
                                .color(match switchable {
                                    // The two the transport works out for itself are
                                    // told apart by being dimmer: they are there to
                                    // be read, not switched.
                                    None => Color::Disabled,
                                    Some(_) => Color::Muted,
                                }),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .px_2()
                            .py_1()
                            .border_l_1()
                            .border_color(colors.border)
                            .child(Label::new(automatic.key).color(Color::Muted)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .px_2()
                            .py_1()
                            .border_l_1()
                            .border_color(colors.border)
                            .child(Label::new(automatic.value).color(match switchable {
                                None => Color::Disabled,
                                Some(_) => Color::Muted,
                            })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .px_2()
                            .py_1()
                            .border_l_1()
                            .border_color(colors.border),
                    )
                    .child(div().w(px(26.)).flex_none()),
            );
        }

        for (index, row) in rows.iter().enumerate() {
            let on_toggle = on_toggle.clone();
            let on_remove = on_remove.clone();
            let waiting_to_be_typed_into = index == last;
            table = table.child(
                h_flex()
                    .id(SharedString::from(format!("{which}-row-{index}")))
                    .debug_selector(move || format!("{which}-row-{index}"))
                    .w_full()
                    .items_stretch()
                    .border_t_1()
                    .border_color(colors.border)
                    .child(
                        div()
                            .id(SharedString::from(format!("{which}-toggle-{index}")))
                            .w(px(26.))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            // The blank row stands for nothing yet, so there is
                            // nothing to switch off or throw away in it.
                            .when(!waiting_to_be_typed_into, |cell| {
                                cell.debug_selector(move || format!("{which}-toggle-{index}"))
                                    .cursor_pointer()
                                    .child(
                                        Icon::new(match row.enabled {
                                            true => IconName::Check,
                                            false => IconName::Close,
                                        })
                                        .size(IconSize::XSmall)
                                        .color(match row
                                            .enabled
                                        {
                                            true => Color::Accent,
                                            false => Color::Muted,
                                        }),
                                    )
                                    .on_click(cx.listener(move |this, _, _window, cx| {
                                        on_toggle(this, index, cx)
                                    }))
                            }),
                    )
                    .children(Column::ALL.map(|column| {
                        div()
                            .flex_1()
                            .min_w_0()
                            .px_2()
                            .py_1()
                            .border_l_1()
                            .border_color(colors.border)
                            .debug_selector(move || {
                                format!("{which}-cell-{index}-{}", column.name())
                            })
                            .child(row.cell(column).clone())
                    }))
                    .child(
                        div()
                            .id(SharedString::from(format!("{which}-remove-{index}")))
                            .w(px(26.))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .when(!waiting_to_be_typed_into, |cell| {
                                cell.debug_selector(move || format!("{which}-remove-{index}"))
                                    .cursor_pointer()
                                    .child(
                                        Icon::new(IconName::Trash)
                                            .size(IconSize::XSmall)
                                            .color(Color::Muted),
                                    )
                                    .on_click(cx.listener(move |this, _, _window, cx| {
                                        on_remove(this, index, cx)
                                    }))
                            }),
                    ),
            );
        }
        table
    }

    /// Moves the writing point to the next cell, and from the last cell of a row
    /// to the first cell of the row below -- what a table does everywhere else.
    fn step_through_the_cells(
        &mut self,
        forwards: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let rows: &[KeyValueRow] = match self.active_tab {
            RequestTab::Params => &self.param_rows,
            RequestTab::Headers => &self.header_rows,
            _ => return,
        };
        let mut cells: Vec<Entity<Editor>> = Vec::with_capacity(rows.len() * Column::ALL.len());
        for row in rows {
            for column in Column::ALL {
                cells.push(row.cell(column).clone());
            }
        }
        let Some(at) = cells.iter().position(|editor| {
            editor
                .read(cx)
                .focus_handle(cx)
                .contains_focused(window, cx)
        }) else {
            return;
        };
        let next = match forwards {
            true => (at + 1) % cells.len(),
            false => (at + cells.len() - 1) % cells.len(),
        };
        let handle = cells[next].read(cx).focus_handle(cx);
        window.focus(&handle, cx);
        cx.notify();
    }

    fn render_tab_body(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        // A spreadsheet always has one more row than it has content, and that is
        // where the next row is typed. Kept up to date here, since this is where
        // the tables are about to be drawn.
        if matches!(self.active_tab, RequestTab::Params | RequestTab::Headers) {
            self.keep_a_row_to_type_into(window, cx);
        }
        match self.active_tab {
            RequestTab::Params => {
                let toggle = Self::render_bulk_edit_toggle(
                    self.params_bulk_edit,
                    "params-bulk-edit-toggle",
                    Self::toggle_params_bulk_edit,
                    cx,
                );
                let body = if self.params_bulk_edit {
                    Self::render_bulk_editor(
                        self.param_bulk_editor.clone(),
                        "params-bulk-editor",
                        cx,
                    )
                    .into_any_element()
                } else {
                    Self::render_key_value_rows(
                        &self.param_rows,
                        "params",
                        Vec::new(),
                        Self::toggle_auto_header,
                        Self::toggle_param_row,
                        Self::remove_param_row,
                        cx,
                    )
                    .into_any_element()
                };
                v_flex()
                    .gap_2()
                    .child(toggle)
                    .child(body)
                    .into_any_element()
            }
            RequestTab::Headers => {
                let toggle = Self::render_bulk_edit_toggle(
                    self.headers_bulk_edit,
                    "headers-bulk-edit-toggle",
                    Self::toggle_headers_bulk_edit,
                    cx,
                );
                let body = if self.headers_bulk_edit {
                    Self::render_bulk_editor(
                        self.header_bulk_editor.clone(),
                        "headers-bulk-editor",
                        cx,
                    )
                    .into_any_element()
                } else {
                    Self::render_key_value_rows(
                        &self.header_rows,
                        "headers",
                        self.automatic_header_rows(),
                        Self::toggle_auto_header,
                        Self::toggle_header_row,
                        Self::remove_header_row,
                        cx,
                    )
                    .into_any_element()
                };
                v_flex()
                    .gap_2()
                    .child(
                        h_flex()
                            .gap_3()
                            .items_center()
                            .child(self.render_auto_headers_switch(cx))
                            .child(toggle),
                    )
                    .child(body)
                    .into_any_element()
            }
            RequestTab::Body => {
                let body_kind = self.body_kind;
                let content_type = self.body_content_type;
                let mut column = v_flex().gap_2().child(
                    h_flex()
                        .gap_1()
                        .child(Self::render_chip_scoped(
                            "body-kind",
                            "None",
                            body_kind == BodyKind::None,
                            cx,
                            |this, _, _, cx| {
                                this.set_body_kind(BodyKind::None, cx);
                            },
                        ))
                        .child(Self::render_chip_scoped(
                            "body-kind",
                            "Raw",
                            body_kind == BodyKind::Raw,
                            cx,
                            |this, _, _, cx| {
                                this.set_body_kind(BodyKind::Raw, cx);
                            },
                        )),
                );
                if body_kind == BodyKind::Raw {
                    let type_row = h_flex()
                        .gap_1()
                        .child(Self::render_chip_scoped(
                            "content-type",
                            "Text",
                            content_type == RawBodyContentType::Text,
                            cx,
                            |this, _, window, cx| {
                                this.set_body_content_type(RawBodyContentType::Text, window, cx);
                            },
                        ))
                        .child(Self::render_chip_scoped(
                            "content-type",
                            "JSON",
                            content_type == RawBodyContentType::Json,
                            cx,
                            |this, _, window, cx| {
                                this.set_body_content_type(RawBodyContentType::Json, window, cx);
                            },
                        ))
                        .child(Self::render_chip_scoped(
                            "content-type",
                            "XML",
                            content_type == RawBodyContentType::Xml,
                            cx,
                            |this, _, window, cx| {
                                this.set_body_content_type(RawBodyContentType::Xml, window, cx);
                            },
                        ))
                        .child(Self::render_chip_scoped(
                            "content-type",
                            "HTML",
                            content_type == RawBodyContentType::Html,
                            cx,
                            |this, _, window, cx| {
                                this.set_body_content_type(RawBodyContentType::Html, window, cx);
                            },
                        ))
                        .child(Self::render_chip_scoped(
                            "content-type",
                            "JavaScript",
                            content_type == RawBodyContentType::JavaScript,
                            cx,
                            |this, _, window, cx| {
                                this.set_body_content_type(
                                    RawBodyContentType::JavaScript,
                                    window,
                                    cx,
                                );
                            },
                        ))
                        .child(
                            div()
                                .id("request-format-body-hitbox")
                                .debug_selector(|| "request-format-body".to_string())
                                .child(
                                    Button::new("request-format-body", "Format")
                                        .style(ButtonStyle::Subtle)
                                        .label_size(LabelSize::Small)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.format_body(window, cx);
                                        })),
                                ),
                        )
                        .child(self.render_variable_picker(self.body_editor.clone(), cx));
                    let colors = cx.theme().colors();
                    column = column.child(type_row).child(
                        div()
                            .w_full()
                            .flex_1()
                            .min_h(px(400.))
                            .px_2()
                            .py_1p5()
                            .rounded_md()
                            .border_1()
                            .border_color(colors.border)
                            .bg(colors.background)
                            .child(self.body_editor.clone()),
                    );
                    if self.body_json_invalid {
                        column = column.child(
                            div()
                                .id("request-body-json-warning")
                                .debug_selector(|| "request-body-json-warning".to_string())
                                .child(
                                    Label::new("This body is not valid JSON -- Send may fail or the server may reject it.")
                                        .size(LabelSize::XSmall)
                                        .color(Color::Warning),
                                ),
                        );
                    }
                } else {
                    column = column.child(
                        Label::new("This request has no body.")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    );
                }
                let _ = window;
                column.into_any_element()
            }
            RequestTab::Auth => {
                let auth_kind = self.auth_kind;
                let mut column = v_flex().gap_3().child(
                    h_flex()
                        .gap_1()
                        .child(Self::render_chip_scoped(
                            "auth-kind",
                            "Inherit",
                            auth_kind == AuthKind::Inherit,
                            cx,
                            |this, _, _, cx| {
                                this.set_auth_kind(AuthKind::Inherit, cx);
                            },
                        ))
                        .child(Self::render_chip_scoped(
                            "auth-kind",
                            "None",
                            auth_kind == AuthKind::None,
                            cx,
                            |this, _, _, cx| {
                                this.set_auth_kind(AuthKind::None, cx);
                            },
                        ))
                        .child(Self::render_chip_scoped(
                            "auth-kind",
                            "Basic",
                            auth_kind == AuthKind::Basic,
                            cx,
                            |this, _, _, cx| {
                                this.set_auth_kind(AuthKind::Basic, cx);
                            },
                        ))
                        .child(Self::render_chip_scoped(
                            "auth-kind",
                            "Bearer",
                            auth_kind == AuthKind::Bearer,
                            cx,
                            |this, _, _, cx| {
                                this.set_auth_kind(AuthKind::Bearer, cx);
                            },
                        ))
                        .child(Self::render_chip_scoped(
                            "auth-kind",
                            "API Key",
                            auth_kind == AuthKind::ApiKey,
                            cx,
                            |this, _, _, cx| {
                                this.set_auth_kind(AuthKind::ApiKey, cx);
                            },
                        ))
                        .child(Self::render_chip_scoped(
                            "auth-kind",
                            "OAuth 2.0",
                            auth_kind == AuthKind::OAuth2,
                            cx,
                            |this, _, _, cx| {
                                this.set_auth_kind(AuthKind::OAuth2, cx);
                            },
                        ))
                        .child(Self::render_chip_scoped(
                            "auth-kind",
                            "AWS Signature v4",
                            auth_kind == AuthKind::AwsSigV4,
                            cx,
                            |this, _, _, cx| {
                                this.set_auth_kind(AuthKind::AwsSigV4, cx);
                            },
                        ))
                        .child(Self::render_chip_scoped(
                            "auth-kind",
                            "JWT Bearer",
                            auth_kind == AuthKind::Jwt,
                            cx,
                            |this, _, _, cx| {
                                this.set_auth_kind(AuthKind::Jwt, cx);
                            },
                        )),
                );
                match auth_kind {
                    AuthKind::Inherit | AuthKind::None => {
                        column = column.child(
                            Label::new(if auth_kind == AuthKind::Inherit {
                                "Uses whatever auth the containing folder or collection resolves to."
                            } else {
                                "No authorization is sent with this request."
                            })
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                        );
                    }
                    AuthKind::Basic => {
                        column = column
                            .child(Self::render_field(
                                "Username".into(),
                                self.auth_username_editor.clone(),
                                cx,
                            ))
                            .child(Self::render_field(
                                "Password".into(),
                                self.auth_password_editor.clone(),
                                cx,
                            ));
                    }
                    AuthKind::Bearer => {
                        column = column.child(Self::render_field(
                            "Token".into(),
                            self.auth_token_editor.clone(),
                            cx,
                        ));
                    }
                    AuthKind::ApiKey => {
                        let placement = self.auth_api_key_placement;
                        column = column
                            .child(Self::render_field(
                                "Key".into(),
                                self.auth_api_key_key_editor.clone(),
                                cx,
                            ))
                            .child(Self::render_field(
                                "Value".into(),
                                self.auth_api_key_value_editor.clone(),
                                cx,
                            ))
                            .child(
                                h_flex()
                                    .gap_1()
                                    .child(Self::render_chip_scoped(
                                        "api-key-placement",
                                        "Header",
                                        placement == ApiKeyPlacement::Header,
                                        cx,
                                        |this, _, _, cx| {
                                            this.set_api_key_placement(ApiKeyPlacement::Header, cx)
                                        },
                                    ))
                                    .child(Self::render_chip_scoped(
                                        "api-key-placement",
                                        "Query",
                                        placement == ApiKeyPlacement::Query,
                                        cx,
                                        |this, _, _, cx| {
                                            this.set_api_key_placement(ApiKeyPlacement::Query, cx)
                                        },
                                    )),
                            );
                    }
                    AuthKind::OAuth2 => {
                        let grant_type = self.oauth2_grant_type;
                        column = column
                            .child(
                                h_flex()
                                    .gap_1()
                                    .child(Self::render_chip_scoped(
                                        "oauth2-grant-type",
                                        "Authorization Code (PKCE)",
                                        grant_type == OAuth2GrantType::AuthorizationCodePkce,
                                        cx,
                                        |this, _, _, cx| {
                                            this.set_oauth2_grant_type(
                                                OAuth2GrantType::AuthorizationCodePkce,
                                                cx,
                                            )
                                        },
                                    ))
                                    .child(Self::render_chip_scoped(
                                        "oauth2-grant-type",
                                        "Client Credentials",
                                        grant_type == OAuth2GrantType::ClientCredentials,
                                        cx,
                                        |this, _, _, cx| {
                                            this.set_oauth2_grant_type(
                                                OAuth2GrantType::ClientCredentials,
                                                cx,
                                            )
                                        },
                                    )),
                            )
                            .when(
                                grant_type == OAuth2GrantType::AuthorizationCodePkce,
                                |column| {
                                    column.child(Self::render_field(
                                        "Authorization URL".into(),
                                        self.oauth2_auth_url_editor.clone(),
                                        cx,
                                    ))
                                },
                            )
                            .child(Self::render_field(
                                "Token URL".into(),
                                self.oauth2_token_url_editor.clone(),
                                cx,
                            ))
                            .child(Self::render_field(
                                "Client ID".into(),
                                self.oauth2_client_id_editor.clone(),
                                cx,
                            ))
                            .child(Self::render_field(
                                "Client Secret".into(),
                                self.oauth2_client_secret_editor.clone(),
                                cx,
                            ))
                            .child(Self::render_field(
                                "Scope".into(),
                                self.oauth2_scope_editor.clone(),
                                cx,
                            ))
                            .child(
                                div()
                                    .id("oauth2-get-token-hitbox")
                                    .debug_selector(|| "oauth2-get-token".to_string())
                                    .child(
                                        Button::new("oauth2-get-token", "Get New Access Token")
                                            .style(ButtonStyle::Subtle)
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.get_new_access_token(window, cx)
                                            })),
                                    ),
                            )
                            .child(match &self.oauth2_status {
                                OAuth2Status::Idle => {
                                    Label::new(if self.oauth2_access_token.is_empty() {
                                        "No access token yet."
                                    } else {
                                        "Access token acquired."
                                    })
                                    .size(LabelSize::Small)
                                    .color(Color::Muted)
                                }
                                OAuth2Status::Requesting => {
                                    Label::new("Requesting access token...")
                                        .size(LabelSize::Small)
                                        .color(Color::Muted)
                                }
                                OAuth2Status::Success => Label::new("Access token acquired.")
                                    .size(LabelSize::Small)
                                    .color(Color::Success),
                                OAuth2Status::Error(message) => {
                                    Label::new(format!("Failed to get an access token: {message}"))
                                        .size(LabelSize::Small)
                                        .color(Color::Error)
                                }
                            });
                    }
                    AuthKind::AwsSigV4 => {
                        column = column
                            .child(Self::render_field(
                                "Access Key ID".into(),
                                self.aws_access_key_editor.clone(),
                                cx,
                            ))
                            .child(Self::render_field(
                                "Secret Access Key".into(),
                                self.aws_secret_key_editor.clone(),
                                cx,
                            ))
                            .child(Self::render_field(
                                "Region".into(),
                                self.aws_region_editor.clone(),
                                cx,
                            ))
                            .child(Self::render_field(
                                "Service".into(),
                                self.aws_service_editor.clone(),
                                cx,
                            ))
                            .child(Self::render_field(
                                "Session Token".into(),
                                self.aws_session_token_editor.clone(),
                                cx,
                            ));
                    }
                    AuthKind::Jwt => {
                        let algorithm = self.jwt_algorithm;
                        let is_base64_encoded = self.jwt_is_secret_base64_encoded;
                        let add_to_query_param = self.jwt_add_to_query_param;
                        let algorithm_row = [
                            (JwtAlgorithm::HS256, "HS256"),
                            (JwtAlgorithm::HS384, "HS384"),
                            (JwtAlgorithm::HS512, "HS512"),
                            (JwtAlgorithm::RS256, "RS256"),
                            (JwtAlgorithm::RS384, "RS384"),
                            (JwtAlgorithm::RS512, "RS512"),
                        ]
                        .into_iter()
                        .fold(h_flex().gap_1(), |row, (value, label)| {
                            row.child(Self::render_chip_scoped(
                                "jwt-algorithm",
                                label,
                                algorithm == value,
                                cx,
                                move |this, _, _, cx| {
                                    this.set_jwt_algorithm(value, cx);
                                },
                            ))
                        });
                        column = column
                            .child(algorithm_row)
                            .child(Self::render_field(
                                "Secret".into(),
                                self.jwt_secret_editor.clone(),
                                cx,
                            ))
                            .child(
                                h_flex()
                                    .gap_1()
                                    .child(Self::render_chip_scoped(
                                        "jwt-secret-encoding",
                                        "Plain Text",
                                        !is_base64_encoded,
                                        cx,
                                        |this, _, _, cx| {
                                            this.set_jwt_secret_base64_encoded(false, cx)
                                        },
                                    ))
                                    .child(Self::render_chip_scoped(
                                        "jwt-secret-encoding",
                                        "Base64",
                                        is_base64_encoded,
                                        cx,
                                        |this, _, _, cx| {
                                            this.set_jwt_secret_base64_encoded(true, cx)
                                        },
                                    )),
                            )
                            .child(
                                Label::new("Payload (JSON claims)")
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .child({
                                let colors = cx.theme().colors();
                                div()
                                    .w_full()
                                    .min_h(px(120.))
                                    .px_2()
                                    .py_1p5()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(colors.border)
                                    .bg(colors.background)
                                    .child(self.jwt_payload_editor.clone())
                            })
                            .child(
                                h_flex()
                                    .gap_1()
                                    .child(Self::render_chip_scoped(
                                        "jwt-token-placement",
                                        "Header",
                                        !add_to_query_param,
                                        cx,
                                        |this, _, _, cx| this.set_jwt_add_to_query_param(false, cx),
                                    ))
                                    .child(Self::render_chip_scoped(
                                        "jwt-token-placement",
                                        "Query Param",
                                        add_to_query_param,
                                        cx,
                                        |this, _, _, cx| this.set_jwt_add_to_query_param(true, cx),
                                    )),
                            );
                        column = if add_to_query_param {
                            column.child(Self::render_field(
                                "Query Param Key".into(),
                                self.jwt_query_param_key_editor.clone(),
                                cx,
                            ))
                        } else {
                            column.child(Self::render_field(
                                "Header Prefix".into(),
                                self.jwt_header_prefix_editor.clone(),
                                cx,
                            ))
                        };
                    }
                }
                column.into_any_element()
            }
            RequestTab::Scripts => {
                let border = cx.theme().colors().border;
                let background = cx.theme().colors().background;
                v_flex()
                    .gap_3()
                    .child(
                        Label::new("Pre-request Script")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        div()
                            .min_h(px(120.))
                            .px_2()
                            .py_1p5()
                            .rounded_md()
                            .border_1()
                            .border_color(border)
                            .bg(background)
                            .child(self.pre_request_script_editor.clone()),
                    )
                    .child(
                        Label::new("Tests")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        div()
                            .min_h(px(120.))
                            .px_2()
                            .py_1p5()
                            .rounded_md()
                            .border_1()
                            .border_color(border)
                            .bg(background)
                            .child(self.test_script_editor.clone()),
                    )
                    .into_any_element()
            }
            RequestTab::Examples => {
                let examples: Vec<SavedExample> = self
                    .store
                    .read(cx)
                    .requests
                    .iter()
                    .find(|r| r.id == self.request_id)
                    .map(|r| r.examples.clone())
                    .unwrap_or_default();
                if examples.is_empty() {
                    return Label::new(
                        "No saved examples yet -- send a request, then use \"Save as Example\" \
                         next to the response.",
                    )
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .into_any_element();
                }
                let mut list = v_flex().gap_1();
                for example in examples {
                    let example_id = example.id;
                    let status_color = if (200..300).contains(&example.response_status) {
                        Color::Success
                    } else if (400..600).contains(&example.response_status) {
                        Color::Error
                    } else {
                        Color::Warning
                    };
                    list = list.child(
                        h_flex()
                            .id(SharedString::from(format!("example-row-{example_id}")))
                            .justify_between()
                            .gap_2()
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .border_1()
                            .border_color(cx.theme().colors().border_variant)
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(Label::new(example.name.clone()).size(LabelSize::Small))
                                    .child(
                                        Label::new(example.response_status.to_string())
                                            .size(LabelSize::Small)
                                            .color(status_color),
                                    )
                                    .child(
                                        Label::new(format!(
                                            "{} {}",
                                            example.request_method.as_str(),
                                            example.request_url
                                        ))
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                    ),
                            )
                            .child(
                                div()
                                    .id(SharedString::from(format!(
                                        "example-delete-hitbox-{example_id}"
                                    )))
                                    .debug_selector(move || format!("example-delete-{example_id}"))
                                    .child(
                                        Icon::new(IconName::Trash)
                                            .size(IconSize::Small)
                                            .color(Color::Muted),
                                    )
                                    .cursor_pointer()
                                    .on_click(cx.listener(move |this, _, _window, cx| {
                                        this.delete_example(example_id, cx);
                                    })),
                            ),
                    );
                }
                list.into_any_element()
            }
        }
    }

    /// Replaces `render_response_section` once a response has been handed
    /// off to the shared dock -- a thin pointer at the dock rather than a
    /// second copy of the same response.
    fn render_response_dock_redirect(&self, cx: &mut Context<Self>) -> AnyElement {
        let border_variant = cx.theme().colors().border_variant;
        v_flex()
            .id("api-client-response-in-dock")
            .debug_selector(|| "api-client-response-in-dock".to_string())
            .pt_3()
            .border_t_1()
            .border_color(border_variant)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Label::new("Response shown in the Response panel below")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Button::new("api-client-response-reveal-dock", "Show Response")
                            .style(ButtonStyle::Subtle)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.reveal_response_dock(window, cx);
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_response_section(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let border = cx.theme().colors().border;
        let border_variant = cx.theme().colors().border_variant;
        let background = cx.theme().colors().background;
        let line_height = window.line_height();
        match &self.send_state {
            SendState::Idle => v_flex()
                .id("request-response-idle-hint")
                .debug_selector(|| "request-response-idle-hint".to_string())
                .pt_3()
                .border_t_1()
                .border_color(border_variant)
                .child(
                    Label::new("Click Send to see the response")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element(),
            SendState::Sending => v_flex()
                .pt_3()
                .border_t_1()
                .border_color(border_variant)
                .child(
                    Label::new("Sending...")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element(),
            SendState::Error(message) => v_flex()
                .pt_3()
                .gap_1()
                .border_t_1()
                .border_color(border_variant)
                .child(
                    Label::new("Request failed")
                        .size(LabelSize::Small)
                        .color(Color::Error),
                )
                .child(
                    Label::new(message.clone())
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element(),
            SendState::Success(response) => {
                let status = response.status;
                let status_text = response.status_text.clone();
                let elapsed_ms = response.elapsed_ms;
                let size = crate::response_view::format_size(response.size_bytes);
                let status_color = if (200..300).contains(&status) {
                    Color::Success
                } else if (400..600).contains(&status) {
                    Color::Error
                } else {
                    Color::Warning
                };
                let response_tab = self.response_tab;
                let headers = response.headers.clone();
                let cookies = response.cookies.clone();
                let response_is_html = self.response_is_html;

                let summary_row = h_flex()
                    .flex_none()
                    .gap_3()
                    .items_center()
                    .child(
                        h_flex()
                            .gap_3()
                            .items_center()
                            .child(Self::render_method_badge(
                                format!("{status} {status_text}").into(),
                                status_color,
                                cx,
                            ))
                            .child(
                                Label::new(format!("{elapsed_ms} ms"))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(Label::new(size).size(LabelSize::Small).color(Color::Muted)),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                div()
                                    .id("request-response-fullscreen-hitbox")
                                    .debug_selector(|| "request-response-fullscreen".to_string())
                                    .child(
                                        IconButton::new(
                                            "request-response-fullscreen",
                                            if self.response_fullscreen {
                                                IconName::Minimize
                                            } else {
                                                IconName::Maximize
                                            },
                                        )
                                        .icon_size(IconSize::XSmall)
                                        .tooltip(Tooltip::text(if self.response_fullscreen {
                                            "Exit fullscreen"
                                        } else {
                                            "View response fullscreen"
                                        }))
                                        .on_click(
                                            cx.listener(|this, _, _window, cx| {
                                                this.toggle_response_fullscreen(cx)
                                            }),
                                        ),
                                    ),
                            )
                            .child(
                                div()
                                    .id("request-save-example-hitbox")
                                    .debug_selector(|| "request-save-example".to_string())
                                    .child(
                                        Button::new("request-save-example", "Save as Example")
                                            .style(ButtonStyle::Subtle)
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.save_response_as_example(window, cx)
                                            })),
                                    ),
                            ),
                    );

                let mut tab_strip = h_flex().gap_2().child(Self::render_chip_scoped(
                    "response-tab",
                    "Pretty",
                    response_tab == ResponseTab::Pretty,
                    cx,
                    |this, _, _, cx| {
                        this.response_tab = ResponseTab::Pretty;
                        cx.notify();
                    },
                ));
                if response_is_html {
                    tab_strip = tab_strip.child(Self::render_chip_scoped(
                        "response-tab",
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
                    .child(Self::render_chip_scoped(
                        "response-tab",
                        "Raw",
                        response_tab == ResponseTab::Raw,
                        cx,
                        |this, _, _, cx| {
                            this.response_tab = ResponseTab::Raw;
                            cx.notify();
                        },
                    ))
                    .child(Self::render_chip_scoped(
                        "response-tab",
                        "Headers",
                        response_tab == ResponseTab::Headers,
                        cx,
                        |this, _, _, cx| {
                            this.response_tab = ResponseTab::Headers;
                            cx.notify();
                        },
                    ))
                    .child(Self::render_chip_scoped(
                        "response-tab",
                        "Cookies",
                        response_tab == ResponseTab::Cookies,
                        cx,
                        |this, _, _, cx| {
                            this.response_tab = ResponseTab::Cookies;
                            cx.notify();
                        },
                    ));
                tab_strip = tab_strip.child(Self::render_chip_scoped(
                    "response-tab",
                    "Timing",
                    response_tab == ResponseTab::Timing,
                    cx,
                    |this, _, _, cx| {
                        this.response_tab = ResponseTab::Timing;
                        cx.notify();
                    },
                ));
                let has_previous_response = self.previous_response.is_some();
                if has_previous_response {
                    tab_strip = tab_strip.child(Self::render_chip_scoped(
                        "response-tab",
                        "Diff",
                        response_tab == ResponseTab::Diff,
                        cx,
                        |this, _, _, cx| {
                            this.response_tab = ResponseTab::Diff;
                            cx.notify();
                        },
                    ));
                }
                if !self.test_results.is_empty() {
                    tab_strip = tab_strip.child(Self::render_chip_scoped(
                        "response-tab",
                        "Test Results",
                        response_tab == ResponseTab::TestResults,
                        cx,
                        |this, _, _, cx| {
                            this.response_tab = ResponseTab::TestResults;
                            cx.notify();
                        },
                    ));
                }
                if self.visualize_data.is_some() {
                    tab_strip = tab_strip.child(Self::render_chip_scoped(
                        "response-tab",
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
                        for test in &self.test_results {
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
                        let text = self
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
                                    .min_h(px(400.))
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
                    ResponseTab::Diff => v_flex()
                        .flex_1()
                        .gap_1()
                        .child(self.render_diff_comparison_selector(cx))
                        .child(
                            div()
                                .flex_1()
                                .min_h(px(400.))
                                .px_2()
                                .py_1p5()
                                .rounded_md()
                                .border_1()
                                .border_color(border)
                                .bg(background)
                                .child(self.diff_body_editor.clone()),
                        )
                        .into_any_element(),
                    ResponseTab::Pretty | ResponseTab::Preview | ResponseTab::Raw => {
                        let editor = match response_tab {
                            ResponseTab::Pretty => self.pretty_body_editor.clone(),
                            ResponseTab::Preview => self.preview_body_editor.clone(),
                            _ => self.raw_body_editor.clone(),
                        };
                        let mut column = v_flex().gap_1().flex_1();
                        if response_tab == ResponseTab::Preview {
                            column = column.child(
                                Label::new("Rendered as plain text -- GPUI has no sandboxed HTML renderer, so scripts and styles are stripped rather than executed.")
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            );
                        }
                        // `flex_1` pins flex-basis to 0, which would discard
                        // the explicit `h` below and always fall back to
                        // shrinking to nothing -- `flex_initial` (shrink but
                        // no forced grow, flex-basis: auto) lets the editor's
                        // own content-driven height act as its natural size,
                        // which the `response_region` wrapper in `render`
                        // then caps at the remaining window space and scrolls
                        // once it doesn't fit.
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
                    ResponseTab::Headers => crate::response_view::render_pairs(
                        "response-header",
                        "No headers in this response.",
                        headers
                            .iter()
                            .map(|(name, value)| crate::response_view::Pair {
                                name: name.clone().into(),
                                value: value.clone().into(),
                                also: None,
                            })
                            .collect(),
                        cx,
                    ),
                    ResponseTab::Timing => {
                        crate::response_view::render_timing(response.timings, cx)
                    }
                    ResponseTab::Cookies => crate::response_view::render_pairs(
                        "response-cookie",
                        "No cookies in this response.",
                        cookies
                            .iter()
                            .map(|cookie| crate::response_view::Pair {
                                name: cookie.name.clone().into(),
                                value: cookie.value.clone().into(),
                                also: match cookie.attributes.is_empty() {
                                    true => None,
                                    false => Some(cookie.attributes.clone().into()),
                                },
                            })
                            .collect(),
                        cx,
                    ),
                };

                v_flex()
                    .pt_2()
                    .gap_2()
                    .border_t_1()
                    .border_color(border_variant)
                    // The tabs and what the response was share one row: a row of
                    // its own for three short words is a row of the document the
                    // reader does not see.
                    .child(
                        h_flex()
                            .w_full()
                            .gap_3()
                            .justify_between()
                            .child(
                                div()
                                    .debug_selector(|| "response-tabs".to_string())
                                    .child(tab_strip),
                            )
                            .child(
                                div()
                                    .debug_selector(|| "response-summary".to_string())
                                    .child(summary_row),
                            ),
                    )
                    .child(body)
                    .into_any_element()
            }
        }
    }
}

impl EventEmitter<()> for RequestView {}

impl Focusable for RequestView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for RequestView {
    type Event = ();

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.title.clone()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Send))
    }

    fn to_item_events(_event: &Self::Event, _f: &mut dyn FnMut(ItemEvent)) {}
}

impl Render for RequestView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The address bar and the parameter table are two views of one query
        // string; whichever was typed in last brings the other along. Done here
        // rather than in the editors' own callbacks, which have no window to build
        // a row's editors with.
        self.keep_the_query_and_the_table_in_step(window, cx);
        let border = cx.theme().colors().border;
        let background = cx.theme().colors().background;
        let editor_background = cx.theme().colors().editor_background;

        // Method, URL, and Send share one row -- the near-universal REST
        // client layout -- rather than a whole row of always-visible method
        // chips above a separate URL row.
        let url_row = h_flex()
            .w_full()
            .gap_2()
            .items_center()
            .child(self.render_method_selector(cx))
            .child(
                div()
                    .flex_1()
                    .px_2()
                    .py_1p5()
                    .rounded_md()
                    .border_1()
                    .border_color(border)
                    .bg(background)
                    .child(self.url_editor.clone()),
            )
            .child(
                div()
                    .id("request-copy-curl-hitbox")
                    .debug_selector(|| "request-copy-curl".to_string())
                    .child(
                        Button::new("request-copy-curl", "Code")
                            .style(ButtonStyle::Subtle)
                            .start_icon(Icon::new(IconName::Code).size(IconSize::Small))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.show_as_code(window, cx)),
                            ),
                    ),
            )
            .child(self.render_environment_pin(window, cx))
            .child(self.render_environments_comparison(window, cx))
            .child({
                let is_sending = matches!(self.send_state, SendState::Sending);
                // Sending is the one thing this view exists for, so it is the
                // largest thing in the row -- but it wears the same accent as every
                // other link here rather than a slab of colour, which at this size
                // glares.
                let accent = cyberpunk::Accent::Cyan;
                div()
                    .id("request-send-hitbox")
                    .debug_selector(|| "request-send".to_string())
                    // A hand-built block still has to answer to a screen reader the
                    // way a button does, which `ui::Button` gave for free.
                    .role(gpui::accesskit::Role::Button)
                    .aria_label(match is_sending {
                        true => "Sending the request",
                        false => "Send the request",
                    })
                    .h(px(36.))
                    .w(px(140.))
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .gap_1p5()
                    .border_1()
                    .border_color(accent.border().opacity(0.55))
                    .bg(accent.border().opacity(0.12))
                    .when(is_sending, |button| button.opacity(0.6))
                    .when(!is_sending, |button| {
                        button
                            .cursor_pointer()
                            .hover(|style| style.bg(accent.border().opacity(0.28)))
                            .on_click(cx.listener(|this, _, window, cx| this.send(window, cx)))
                    })
                    .child(
                        Icon::new(IconName::PlayFilled)
                            .size(IconSize::Small)
                            .color(Color::Accent),
                    )
                    .child(
                        div()
                            .cyberpunk_monospace(cx)
                            .font_weight(gpui::FontWeight::EXTRA_BOLD)
                            .text_size(ui::HeadlineSize::XSmall.rems())
                            .text_color(Color::Accent.color(cx))
                            .child(match is_sending {
                                true => "SENDING",
                                false => "SEND",
                            }),
                    )
            });

        let url_warning = self.url_looks_malformed.then(|| {
            div()
                .id("request-url-warning")
                .debug_selector(|| "request-url-warning".to_string())
                .child(
                    Label::new("This URL has no scheme (e.g. https://) and doesn't start with a {{variable}} -- Send may fail.")
                        .size(LabelSize::XSmall)
                        .color(Color::Warning),
                )
        });

        let active_tab = self.active_tab;
        // Params, then Authorization: the address and what it takes to be let in
        // are the two things a reader sets first.
        let mut tab_strip = h_flex().gap_2();
        for (label, tab) in [
            ("Params", RequestTab::Params),
            ("Authorization", RequestTab::Auth),
            ("Headers", RequestTab::Headers),
            ("Body", RequestTab::Body),
            ("Scripts", RequestTab::Scripts),
            ("Examples", RequestTab::Examples),
        ] {
            tab_strip = tab_strip.child(Self::render_chip(
                label,
                active_tab == tab,
                cx,
                move |this, _, _, cx| {
                    this.active_tab = tab;
                    cx.notify();
                },
            ));
        }

        let response_section = if self.response_shown_in_dock(cx) {
            self.render_response_dock_redirect(cx)
        } else {
            self.render_response_section(window, cx)
        };

        if self.response_fullscreen {
            return v_flex()
                .id("api-client-request-view")
                .key_context("ApiClientRequestView")
                .track_focus(&self.focus_handle)
                .on_action(cx.listener(|this, _: &NextCell, window, cx| {
                    this.step_through_the_cells(true, window, cx)
                }))
                .on_action(cx.listener(|this, _: &PreviousCell, window, cx| {
                    this.step_through_the_cells(false, window, cx)
                }))
                .on_action(cx.listener(
                    |this, _: &zed_actions::api_client_panel::SendRequest, window, cx| {
                        this.send(window, cx);
                    },
                ))
                .size_full()
                .p_4()
                .gap_3()
                .bg(editor_background)
                .overflow_scroll()
                .track_scroll(&self.scroll_handle)
                .child(response_section)
                .custom_scrollbars(
                    Scrollbars::always_visible(ScrollAxes::Vertical)
                        .tracked_scroll_handle(&self.scroll_handle),
                    window,
                    cx,
                )
                .into_any_element();
        }

        let tab_body = self.render_tab_body(window, cx);

        // The form (URL/env/tabs) always keeps its natural height
        // (`flex_shrink_0`) -- only the response below it gives up space.
        let top_section = v_flex()
            .id("api-client-request-top-section")
            .debug_selector(|| "api-client-request-top-section".to_string())
            .flex_shrink_0()
            .gap_3()
            .child(url_row)
            .when_some(url_warning, |this, warning| this.child(warning))
            .child(tab_strip)
            .child(div().child(tab_body));

        // `flex_initial` (grow: 0, shrink: 1, basis: auto) sizes this to the
        // response's own content height when there's room, and lets it shrink
        // below that -- via `min_h(0)`, overriding Taffy's default
        // min-height-equals-content -- once the window doesn't have enough
        // remaining space, at which point `overflow_scroll` reveals the rest.
        let response_region = div()
            .id("api-client-response-scroll-region")
            .debug_selector(|| "api-client-response-scroll-region".to_string())
            .flex_initial()
            .min_h(px(0.))
            .overflow_scroll()
            .track_scroll(&self.scroll_handle)
            .child(response_section)
            .custom_scrollbars(
                Scrollbars::always_visible(ScrollAxes::Vertical)
                    .tracked_scroll_handle(&self.scroll_handle),
                window,
                cx,
            );

        v_flex()
            .id("api-client-request-view")
            .debug_selector(|| "api-client-request-view".to_string())
            .key_context("ApiClientRequestView")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &NextCell, window, cx| {
                this.step_through_the_cells(true, window, cx)
            }))
            .on_action(cx.listener(|this, _: &PreviousCell, window, cx| {
                this.step_through_the_cells(false, window, cx)
            }))
            .on_action(cx.listener(
                |this, _: &zed_actions::api_client_panel::SendRequest, window, cx| {
                    this.send(window, cx);
                },
            ))
            .size_full()
            .p_4()
            .gap_3()
            .bg(editor_background)
            .child(top_section)
            .child(response_region)
            .into_any_element()
    }
}

/// A read-only-in-spirit preview of a generated `curl` command, shown with
/// shell syntax highlighting so the author can look it over before deciding
/// whether to copy it -- the "Copy" button copies whatever text is currently
/// in the editor, so an author who tweaks the command before copying gets
/// their edited version, not the original.
/// The request as code, in whichever shape the reader asks for.
///
/// The request itself is held rather than the generated text, so switching shape
/// generates again from the same request -- what is shown is always this request,
/// not whatever it looked like when the window opened.
pub(crate) struct CodeSnippetModal {
    focus_handle: FocusHandle,
    pub(crate) code_editor: Entity<Editor>,
    request: api_client::Request,
    store: Entity<ApiClientStore>,
    languages: Arc<language::LanguageRegistry>,
    shown: Snippet,
    /// Whether a line too long for the window is folded onto the next one rather
    /// than left to run off the right of it.
    wrapped: bool,
    /// Where its top-left corner sits in the window, and how big it is. The reader
    /// moves and resizes it, so neither is fixed.
    place: Point<Pixels>,
    size: Size<Pixels>,
    held: Option<Held>,
    remembering_place: Option<gpui::Task<()>>,
    /// Set for a moment after the code is copied, so the button can say so.
    copied: bool,
    forgetting_the_copy: Option<gpui::Task<()>>,
}

/// What the pointer is doing to the window while a button is down.
#[derive(Clone, Copy)]
enum Held {
    /// Moving it: the pointer holds the window at this offset from its corner, so
    /// the corner keeps its distance from the pointer however far it travels.
    Moving { at: Point<Pixels> },
    /// Resizing from the bottom-right corner: where the pointer started, and how
    /// big the window was then.
    Resizing {
        from: Point<Pixels>,
        was: Size<Pixels>,
    },
}

/// Small enough to still be usable, large enough to still be a window.
const NARROWEST_SNIPPET_WINDOW: Pixels = px(420.);
const SHORTEST_SNIPPET_WINDOW: Pixels = px(220.);
/// What it opens as the first time, before the reader has resized it.
const SNIPPET_WINDOW_SIZE: Size<Pixels> = Size {
    width: px(760.),
    height: px(520.),
};

/// Where the window was left, so reopening it does not undo the reader's dragging
/// and sizing. Kept for the whole editor rather than on the view that opened it:
/// the next one is usually opened from another request, and to the reader it is
/// the same window. Held for the run of the editor rather than written down --
/// the placement matters within a sitting, and a stored one can name room the
/// editor's window no longer has.
struct WhereItWasLeft {
    place: Point<Pixels>,
    size: Size<Pixels>,
}

impl gpui::Global for WhereItWasLeft {}

impl CodeSnippetModal {
    pub(crate) fn new(
        request: api_client::Request,
        store: Entity<ApiClientStore>,
        languages: Arc<language::LanguageRegistry>,
        shown: Snippet,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let code_editor = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.set_read_only(true);
            // A command is read here rather than written, and a URL disappearing
            // off the right of the window is the one thing this window cannot
            // afford: long lines are folded until the reader says otherwise.
            editor.set_soft_wrap_mode(language::language_settings::SoftWrap::EditorWidth, cx);
            editor
        });
        let (place, size) = Self::where_to_open(window, cx);
        let mut modal = Self {
            focus_handle: cx.focus_handle(),
            code_editor,
            request,
            store,
            languages,
            shown,
            wrapped: true,
            place,
            size,
            held: None,
            remembering_place: None,
            copied: false,
            forgetting_the_copy: None,
        };
        modal.keep_inside_the_window(window);
        modal.show(shown, window, cx);
        modal
    }

    /// The shape last asked for, so the window opens on it next time rather than
    /// on cURL again.
    pub(crate) fn shown(&self) -> Snippet {
        self.shown
    }

    /// Where it was left, if the editor's window still has room for it there;
    /// otherwise across the window at the size it opens at.
    fn where_to_open(window: &Window, cx: &App) -> (Point<Pixels>, Size<Pixels>) {
        let viewport = window.viewport_size();
        let left = cx.try_global::<WhereItWasLeft>().filter(|left| {
            left.place.x + left.size.width <= viewport.width
                && left.place.y + left.size.height <= viewport.height
        });
        match left {
            Some(left) => (left.place, left.size),
            None => {
                let size = SNIPPET_WINDOW_SIZE;
                (Self::first_place_of(size, window), size)
            }
        }
    }

    /// Across the window and a little down from its top: near the request it came
    /// from, and clear of the tab bar.
    fn first_place_of(size: Size<Pixels>, window: &Window) -> Point<Pixels> {
        let viewport = window.viewport_size();
        point(
            ((viewport.width - size.width) / 2.).max(px(0.)),
            (viewport.height * 0.08).max(px(0.)),
        )
    }

    /// Nothing may hang past the edge of the window: a title bar dragged out of
    /// reach cannot be dragged back.
    fn keep_inside_the_window(&mut self, window: &Window) {
        let viewport = window.viewport_size();
        let widest = viewport.width.max(NARROWEST_SNIPPET_WINDOW);
        let tallest = viewport.height.max(SHORTEST_SNIPPET_WINDOW);
        self.size.width = self
            .size
            .width
            .clamp(NARROWEST_SNIPPET_WINDOW.min(widest), widest);
        self.size.height = self
            .size
            .height
            .clamp(SHORTEST_SNIPPET_WINDOW.min(tallest), tallest);
        self.place.x = self
            .place
            .x
            .clamp(px(0.), (viewport.width - self.size.width).max(px(0.)));
        self.place.y = self
            .place
            .y
            .clamp(px(0.), (viewport.height - self.size.height).max(px(0.)));
    }

    fn take_hold(&mut self, held: Held, cx: &mut Context<Self>) {
        self.held = Some(held);
        cx.notify();
    }

    /// The pointer moved while the window was held.
    fn dragged_to(&mut self, pointer: Point<Pixels>, window: &Window, cx: &mut Context<Self>) {
        match self.held {
            Some(Held::Moving { at }) => {
                self.place = point(pointer.x - at.x, pointer.y - at.y);
            }
            Some(Held::Resizing { from, was }) => {
                self.size = Size {
                    width: was.width + (pointer.x - from.x),
                    height: was.height + (pointer.y - from.y),
                };
            }
            None => return,
        }
        self.keep_inside_the_window(window);
        self.remember_where_it_was_left(cx);
        cx.notify();
    }

    fn let_go(&mut self, cx: &mut Context<Self>) {
        if self.held.take().is_some() {
            cx.notify();
        }
    }

    /// A drag delivers a move a frame, and writing every one of them down is work
    /// nobody asked for: only where the window is a tenth of a second later is
    /// kept.
    fn remember_where_it_was_left(&mut self, cx: &mut Context<Self>) {
        if self.remembering_place.is_some() {
            return;
        }
        self.remembering_place = Some(cx.spawn(async move |modal, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(100))
                .await;
            modal
                .update(cx, |modal, cx| {
                    modal.remembering_place.take();
                    cx.set_global(WhereItWasLeft {
                        place: modal.place,
                        size: modal.size,
                    });
                })
                .log_err();
        }));
    }

    /// While the window is held, the pointer is followed wherever it goes --
    /// including outside the window's own bounds, which an element's own mouse
    /// handlers never see.
    fn follow_the_pointer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity().downgrade();
        gpui::canvas(
            |_, _, _| (),
            move |_, _, window, _cx| {
                let moved = view.clone();
                window.on_mouse_event(move |event: &gpui::MouseMoveEvent, phase, window, cx| {
                    if phase != gpui::DispatchPhase::Bubble {
                        return;
                    }
                    // The window is closed while it is being dragged often enough
                    // that a gone entity is ordinary here, not a fault.
                    moved
                        .update(cx, |modal, cx| match event.pressed_button {
                            Some(MouseButton::Left) => modal.dragged_to(event.position, window, cx),
                            // Released where these listeners cannot see it -- outside
                            // the editor's own window, or while the editor was not
                            // the window in front. Without this the window would
                            // stay stuck to the pointer afterwards.
                            _ => modal.let_go(cx),
                        })
                        .ok();
                });
                let released = view;
                window.on_mouse_event(move |_: &gpui::MouseUpEvent, phase, _window, cx| {
                    if phase == gpui::DispatchPhase::Bubble {
                        released.update(cx, |modal, cx| modal.let_go(cx)).ok();
                    }
                });
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
    }

    /// The corner that resizes it.
    fn render_grip(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let was = self.size;
        div()
            .id("code-snippet-grip")
            .debug_selector(|| "code-snippet-grip".to_string())
            .absolute()
            .bottom_0()
            .right_0()
            .w(px(16.))
            .h(px(16.))
            .cursor(gpui::CursorStyle::ResizeUpLeftDownRight)
            .child(
                Icon::new(IconName::ArrowDownRight)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |modal, event: &MouseDownEvent, _window, cx| {
                    modal.take_hold(
                        Held::Resizing {
                            from: event.position,
                            was,
                        },
                        cx,
                    );
                    cx.stop_propagation();
                }),
            )
    }

    fn show(&mut self, snippet: Snippet, window: &mut Window, cx: &mut Context<Self>) {
        self.shown = snippet;
        let code = {
            let store = self.store.read(cx);
            let context = store.variable_context_for(&self.request);
            crate::code_generator::generate(snippet, &self.request, &context)
        };
        self.code_editor.update(cx, |editor, cx| {
            editor.set_read_only(false);
            editor.set_text(code, window, cx);
            editor.set_read_only(true);
        });

        // The colouring follows the shape, and the language has to be fetched, so
        // it arrives a moment after the text.
        let wanted = language_of(snippet);
        let language = self.languages.language_for_name(wanted);
        let editor = self.code_editor.clone();
        cx.spawn(async move |_, cx| {
            let Some(language) = language.await.log_err() else {
                return;
            };
            editor.update(cx, |editor, cx| {
                if let Some(buffer) = editor.buffer().read(cx).as_singleton() {
                    buffer.update(cx, |buffer, cx| buffer.set_language(Some(language), cx));
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn copy(&mut self, cx: &mut Context<Self>) {
        let text = self.code_editor.read(cx).text(cx);
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        // Said on the button rather than in a message somewhere else: a press
        // that silently succeeds reads as a press that did nothing, and the
        // clipboard cannot be looked at to check.
        self.copied = true;
        self.forgetting_the_copy = Some(cx.spawn(async move |modal, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(2))
                .await;
            modal
                .update(cx, |modal, cx| {
                    modal.copied = false;
                    cx.notify();
                })
                .ok();
        }));
        cx.notify();
    }

    /// Folds long lines into the window, or lets them run off the right of it for a
    /// reader who wants one command on one line.
    fn wrap_the_lines(&mut self, wrapped: bool, cx: &mut Context<Self>) {
        self.wrapped = wrapped;
        let mode = match wrapped {
            true => language::language_settings::SoftWrap::EditorWidth,
            false => language::language_settings::SoftWrap::None,
        };
        self.code_editor
            .update(cx, |editor, cx| editor.set_soft_wrap_mode(mode, cx));
        cx.notify();
    }

    fn render_wrap_toggle(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .debug_selector(|| "code-snippet-wrap".to_string())
            .child(
                Checkbox::new(
                    "code-snippet-wrap",
                    match self.wrapped {
                        true => ToggleState::Selected,
                        false => ToggleState::Unselected,
                    },
                )
                .label("Wrap")
                .label_size(LabelSize::Small)
                .tooltip(Tooltip::text("Fold long lines into the window"))
                .on_click(cx.listener(
                    |modal, wanted: &ToggleState, _window, cx| {
                        modal.wrap_the_lines(matches!(wanted, ToggleState::Selected), cx)
                    },
                )),
            )
    }

    fn render_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let shown = self.shown;
        ui::PopoverMenu::new("code-snippet-shapes")
            .trigger(
                Button::new("code-snippet-shape", shown.label())
                    .style(ButtonStyle::OutlinedCustom(
                        cyberpunk::Accent::Cyan.border(),
                    ))
                    .end_icon(Icon::new(IconName::ChevronDown).size(IconSize::XSmall)),
            )
            .menu({
                let modal = cx.entity();
                move |window, cx| {
                    let modal = modal.clone();
                    Some(ui::ContextMenu::build(window, cx, move |mut menu, _, _| {
                        for snippet in Snippet::ALL {
                            let modal = modal.clone();
                            menu = menu.toggleable_entry(
                                snippet.label(),
                                snippet == shown,
                                ui::IconPosition::Start,
                                None,
                                move |window, cx| {
                                    modal.update(cx, |modal, cx| modal.show(snippet, window, cx));
                                },
                            );
                        }
                        menu
                    }))
                }
            })
    }
}

/// What each shape is written in, so the editor colours it.
fn language_of(snippet: Snippet) -> &'static str {
    match snippet {
        Snippet::Curl | Snippet::Wget => "Shell Script",
        Snippet::HttpText => "Plain Text",
        Snippet::Go => "Go",
        Snippet::Python => "Python",
        Snippet::JavaScript | Snippet::NodeAxios => "JavaScript",
        Snippet::Rust => "Rust",
        Snippet::Php => "PHP",
        Snippet::CSharp => "C#",
        Snippet::Java => "Java",
        Snippet::Ruby => "Ruby",
    }
}

impl EventEmitter<gpui::DismissEvent> for CodeSnippetModal {}

impl workspace::ModalView for CodeSnippetModal {
    /// Rendered on its own rather than inside the modal layer's centred backdrop:
    /// the backdrop is what closes a modal on a click anywhere else, and this
    /// window has to stay open while the reader works beside it. Escape and its
    /// own close button are the ways out.
    fn render_bare(&self) -> bool {
        true
    }
}

impl Focusable for CodeSnippetModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for CodeSnippetModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("CodeSnippetModal")
            .track_focus(&self.focus_handle)
            .id("code-snippet-window")
            .debug_selector(|| "code-snippet-window".to_string())
            .occlude()
            .absolute()
            .left(self.place.x)
            .top(self.place.y)
            .w(self.size.width)
            .h(self.size.height)
            .p_3()
            .gap_3()
            .cyberpunk_surface()
            .rounded(ElevationIndex::ModalSurface.radius())
            .shadow(ElevationIndex::ModalSurface.shadow(cx))
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(gpui::DismissEvent)))
            .child(
                h_flex()
                    .id("code-snippet-title")
                    .debug_selector(|| "code-snippet-title".to_string())
                    .w_full()
                    .flex_none()
                    .justify_between()
                    .items_center()
                    .cursor(gpui::CursorStyle::OpenHand)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|modal, event: &MouseDownEvent, window, cx| {
                            // Taking hold of the title also brings the window forward,
                            // so Escape closes the window the reader just touched
                            // rather than whatever had the focus before it.
                            window.focus(&modal.focus_handle, cx);
                            // Where the pointer took hold of the window, so it keeps
                            // that distance from the corner for the whole drag.
                            modal.take_hold(
                                Held::Moving {
                                    at: point(
                                        event.position.x - modal.place.x,
                                        event.position.y - modal.place.y,
                                    ),
                                },
                                cx,
                            );
                        }),
                    )
                    .child(
                        div()
                            .cyberpunk_monospace(cx)
                            .font_weight(gpui::FontWeight::EXTRA_BOLD)
                            .text_size(ui::HeadlineSize::Small.rems())
                            .text_color(cyberpunk::text_primary())
                            .child("CODE SNIPPET"),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(self.render_wrap_toggle(cx))
                            .child(self.render_picker(cx))
                            .child(
                                IconButton::new("code-snippet-close", IconName::Close)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("Close"))
                                    .on_click(
                                        cx.listener(|_, _, _window, cx| {
                                            cx.emit(gpui::DismissEvent)
                                        }),
                                    ),
                            ),
                    ),
            )
            .child(
                div()
                    .id("code-snippet-editor-hitbox")
                    .debug_selector(|| "code-snippet-editor".to_string())
                    .flex_1()
                    .min_h_0()
                    .py_2()
                    // A window inside a window: the code is what this window is
                    // for, so it sits on the window's own surface with a rule
                    // marking where the heading ends, and no frame of its own.
                    .border_t_1()
                    .border_color(cyberpunk::border_dim())
                    .child(self.code_editor.clone()),
            )
            .child(
                h_flex()
                    .w_full()
                    .flex_none()
                    .justify_between()
                    .items_center()
                    .child(
                        Label::new("Exactly what Send would do: your variables, auth and headers.")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        // One copy, not two. The heading used to carry an icon
                        // that copied without closing while this button copied
                        // and closed, which is two behaviours nothing on screen
                        // told apart. Copying now leaves the window open, so a
                        // reader can switch the format and copy again.
                        div()
                            .debug_selector(|| "code-snippet-copy".to_string())
                            .child(
                                Button::new(
                                    "code-snippet-copy",
                                    match self.copied {
                                        true => "Copied",
                                        false => "Copy",
                                    },
                                )
                                .style(ButtonStyle::OutlinedCustom(
                                    cyberpunk::Accent::Cyan.border(),
                                ))
                                .on_click(cx.listener(|this, _, _window, cx| this.copy(cx))),
                            ),
                    ),
            )
            .child(self.render_grip(cx))
            .when(self.held.is_some(), |window| {
                window.child(self.follow_the_pointer(cx))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::ApiClientStore;
    use gpui::{TestAppContext, VisualTestContext};
    use project::Project;
    use terminal_view::terminal_panel::TerminalPanel;
    use workspace::ItemHandle as _;
    use workspace::dock::Panel as _;

    #[test]
    fn every_raw_body_content_type_maps_to_the_matching_language_and_header_value() {
        assert_eq!(
            language_name_for_content_type(RawBodyContentType::Json),
            Some("JSON")
        );
        assert_eq!(
            content_type_header_value(RawBodyContentType::Json),
            "application/json"
        );
        assert_eq!(
            language_name_for_content_type(RawBodyContentType::Xml),
            Some("XML")
        );
        assert_eq!(
            content_type_header_value(RawBodyContentType::Xml),
            "application/xml"
        );
        assert_eq!(
            language_name_for_content_type(RawBodyContentType::Html),
            Some("HTML")
        );
        assert_eq!(
            content_type_header_value(RawBodyContentType::Html),
            "text/html"
        );
        assert_eq!(
            language_name_for_content_type(RawBodyContentType::JavaScript),
            Some("JavaScript")
        );
        assert_eq!(
            content_type_header_value(RawBodyContentType::JavaScript),
            "application/javascript"
        );
        assert_eq!(
            language_name_for_content_type(RawBodyContentType::Text),
            None,
            "plain text has no language to attach"
        );
        assert_eq!(
            content_type_header_value(RawBodyContentType::Text),
            "text/plain"
        );
    }

    #[test]
    fn each_http_method_has_a_fixed_semantic_color_matching_its_string_label_mapping() {
        assert_eq!(
            RequestView::method_color(&HttpMethod::Get),
            RequestView::method_color_for_label("GET")
        );
        assert_eq!(
            RequestView::method_color(&HttpMethod::Post),
            RequestView::method_color_for_label("POST")
        );
        assert_eq!(
            RequestView::method_color(&HttpMethod::Put),
            RequestView::method_color_for_label("PUT")
        );
        assert_eq!(
            RequestView::method_color(&HttpMethod::Patch),
            RequestView::method_color_for_label("PATCH")
        );
        assert_eq!(
            RequestView::method_color(&HttpMethod::Delete),
            RequestView::method_color_for_label("DELETE")
        );
        assert_eq!(RequestView::method_color(&HttpMethod::Get), Color::Info);
        assert_eq!(RequestView::method_color(&HttpMethod::Post), Color::Success);
        assert_eq!(RequestView::method_color(&HttpMethod::Delete), Color::Error);
        assert_eq!(
            RequestView::method_color(&HttpMethod::Put),
            RequestView::method_color(&HttpMethod::Patch)
        );
    }

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            // The same two bindings `assets/keymaps/default-linux.json` ships, in
            // the same context: what has to hold is that they reach a cell's editor
            // at all, which a shorter context would fake.
            cx.bind_keys([
                gpui::KeyBinding::new("tab", NextCell, Some("ApiClientKeyValueTable > Editor")),
                gpui::KeyBinding::new(
                    "shift-tab",
                    PreviousCell,
                    Some("ApiClientKeyValueTable > Editor"),
                ),
            ]);
        });
    }

    /// A row's editors, borrowed for reading outside the view's own lease.
    fn clone_row(row: &KeyValueRow) -> KeyValueRow {
        KeyValueRow {
            key_editor: row.key_editor.clone(),
            value_editor: row.value_editor.clone(),
            description_editor: row.description_editor.clone(),
            enabled: row.enabled,
        }
    }

    /// The rows that stand for something, which is every row but the blank one the
    /// next line is typed into.
    fn rows_that_say_something(
        rows: &[KeyValueRow],
        cx: &mut VisualTestContext,
    ) -> Vec<(String, String, bool)> {
        cx.update(|_, cx| {
            rows.iter()
                .filter(|row| !row.is_blank(cx))
                .map(|row| {
                    (
                        row.key_editor.read(cx).text(cx),
                        row.value_editor.read(cx).text(cx),
                        row.enabled,
                    )
                })
                .collect()
        })
    }

    async fn build_request_view(
        cx: &mut TestAppContext,
    ) -> (
        Entity<ApiClientStore>,
        RequestId,
        Entity<RequestView>,
        VisualTestContext,
    ) {
        init_test(cx);
        let store = cx.new(|cx| ApiClientStore::new(cx));
        let collection_id = store.update(cx, |store, cx| store.create_collection("A".into(), cx));
        let request_id = store.update(cx, |store, cx| {
            store.create_request(collection_id, "Get users".into(), None, cx)
        });
        let request = store.read_with(cx, |store, _| {
            store
                .requests
                .iter()
                .find(|r| r.id == request_id)
                .unwrap()
                .clone()
        });

        // A throwaway `Workspace`, just to hand `RequestView` a real
        // `WeakEntity<Workspace>` for its (optional) language-registry
        // lookup -- `RequestView` itself is the window root here, matching
        // `ConnectionView`'s test pattern, since it doesn't need pane/tab
        // chrome to be interaction-testable.
        let fs = project::FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let workspace_window =
            cx.add_window(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let workspace_handle = workspace_window
            .read_with(cx, |workspace, _| workspace.weak_handle())
            .unwrap();

        let window = cx.add_window(|window, cx| {
            RequestView::new(&request, store.clone(), workspace_handle, window, cx)
        });
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        let view = window.root(&mut cx).unwrap();
        (store, request_id, view, cx)
    }

    /// Unlike `build_request_view`, this mounts `RequestView` as a real pane
    /// item inside the workspace's own window (matching how
    /// `ApiClientPanel::open_request` wires it up in production) -- needed
    /// for the dock-routing tests below, since revealing a panel calls
    /// `workspace.reveal_panel`/`add_panel` against that same window.
    async fn build_request_view_in_workspace(
        cx: &mut TestAppContext,
    ) -> (
        Entity<ApiClientStore>,
        Entity<Workspace>,
        Entity<RequestView>,
        VisualTestContext,
    ) {
        init_test(cx);
        let store = cx.new(|cx| ApiClientStore::new(cx));
        let collection_id = store.update(cx, |store, cx| store.create_collection("A".into(), cx));
        let request_id = store.update(cx, |store, cx| {
            store.create_request(collection_id, "Get users".into(), None, cx)
        });
        let request = store.read_with(cx, |store, _| {
            store
                .requests
                .iter()
                .find(|r| r.id == request_id)
                .unwrap()
                .clone()
        });

        let fs = project::FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let window = cx.add_window(|window, cx| Workspace::test_new(project.clone(), window, cx));

        let view = window
            .update(cx, |workspace, window, cx| {
                let workspace_handle = workspace.weak_handle();
                let view = cx.new(|cx| {
                    RequestView::new(&request, store.clone(), workspace_handle, window, cx)
                });
                workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx);
                view
            })
            .unwrap();

        let mut cx = VisualTestContext::from_window(window.into(), cx);
        let workspace = window.root(&mut cx).unwrap();
        (store, workspace, view, cx)
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

    /// Clicking an environment sends the request there. It must not pin it:
    /// pinning is the button on the row, and the two are different things.
    #[gpui::test]
    async fn clicking_an_environment_sends_the_request_there_without_pinning_it(
        cx: &mut TestAppContext,
    ) {
        let (store, request_id, _view, mut cx) = build_request_view(cx).await;
        let staging_id = store.update(&mut cx, |store, cx| {
            let staging = store.create_environment("Staging".into(), cx);
            store.create_environment("Production".into(), cx);
            staging
        });
        draw(&mut cx);

        open_the_environment_picker(&mut cx);
        click_in_the_picker(&mut cx, "environment-row:Staging");

        store.read_with(&cx, |store, _| {
            let request = store.requests.iter().find(|r| r.id == request_id).unwrap();
            assert_eq!(
                request.chosen_environment(),
                Some(staging_id),
                "the click has to send the request to the environment it landed on"
            );
            assert!(
                request.pinned_environments().is_empty(),
                "and it must not pin it: {:?}",
                request.pinned_environments()
            );
        });
        assert!(
            cx.debug_bounds("request-environment-pin:Staging").is_some(),
            "the chip names the environment the request is sent to"
        );
    }

    /// Pinning is the button on the row, and pinning does not send anything
    /// anywhere: it only keeps the environment at the top of the list.
    #[gpui::test]
    async fn the_pin_button_pins_without_sending_the_request_there(cx: &mut TestAppContext) {
        let (store, request_id, _view, mut cx) = build_request_view(cx).await;
        let production_id = store.update(&mut cx, |store, cx| {
            store.create_environment("Staging".into(), cx);
            store.create_environment("Production".into(), cx)
        });
        draw(&mut cx);

        open_the_environment_picker(&mut cx);
        // The button is only there while the row is under the pointer, the way
        // the reader meets it.
        let row = debug_center(&mut cx, "environment-row:Production");
        cx.simulate_mouse_move(row, None, gpui::Modifiers::none());
        cx.run_until_parked();
        draw(&mut cx);
        let pin = debug_center(&mut cx, "environment-pin:Production");
        cx.simulate_click(pin, gpui::Modifiers::none());
        cx.run_until_parked();
        draw(&mut cx);

        store.read_with(&cx, |store, _| {
            let request = store.requests.iter().find(|r| r.id == request_id).unwrap();
            assert_eq!(
                request.pinned_environments(),
                vec![production_id],
                "the button has to pin the row it belongs to"
            );
            assert_eq!(
                request.chosen_environment(),
                None,
                "and pinning must not send the request anywhere"
            );
        });
        assert!(
            cx.debug_bounds("request-environment-pin:Active Environment")
                .is_some(),
            "so the chip still says the request follows the active environment"
        );
        assert!(
            cx.debug_bounds("environment-picker-pinned-header")
                .is_some(),
            "and the pinned group appears, with its heading"
        );
    }

    /// What Send does when a comparison has been asked for -- and what it does
    /// not do when one has not.
    #[gpui::test]
    async fn send_compares_only_what_was_asked_for(cx: &mut TestAppContext) {
        let (store, request_id, _view, mut cx) = build_request_view(cx).await;
        let (staging, production, elsewhere) = store.update(&mut cx, |store, cx| {
            (
                store.create_environment("Staging".into(), cx),
                store.create_environment("Production".into(), cx),
                store.create_environment("Elsewhere".into(), cx),
            )
        });
        let request_of = |cx: &mut VisualTestContext| {
            store.read_with(cx, |store, _| {
                store
                    .requests
                    .iter()
                    .find(|r| r.id == request_id)
                    .cloned()
                    .unwrap()
            })
        };

        // Nothing asked for: a plain send.
        let request = request_of(&mut cx);
        store.read_with(&cx, |store, _| {
            assert_eq!(what_send_compares(&request, store), None);
        });

        // Asked for, with an environment of its own: that one against the other.
        store.update(&mut cx, |store, cx| {
            store.choose_request_environment(request_id, Some(staging), cx);
            store.set_request_comparison_environment(request_id, Some(production), cx);
        });
        let request = request_of(&mut cx);
        store.read_with(&cx, |store, _| {
            assert_eq!(
                what_send_compares(&request, store),
                Some((staging, production))
            );
        });

        // Asked for, following the active environment: that one against the other.
        store.update(&mut cx, |store, cx| {
            store.choose_request_environment(request_id, None, cx);
            store.set_active_environment(Some(elsewhere), cx);
        });
        let request = request_of(&mut cx);
        store.read_with(&cx, |store, _| {
            assert_eq!(
                what_send_compares(&request, store),
                Some((elsewhere, production))
            );
        });

        // Asked for with nothing else at all: the same environment on both
        // sides, which is a fair question to ask.
        store.update(&mut cx, |store, cx| {
            store.set_active_environment(None, cx);
        });
        let request = request_of(&mut cx);
        store.read_with(&cx, |store, _| {
            assert_eq!(
                what_send_compares(&request, store),
                Some((production, production)),
                "comparing one environment with itself is allowed: two sends to it \
                 can answer differently"
            );
        });

        // Asked for an environment that has since been deleted: a plain send.
        store.update(&mut cx, |store, cx| {
            store
                .environments
                .retain(|environment| environment.id != production);
            cx.notify();
        });
        let request = request_of(&mut cx);
        store.read_with(&cx, |store, _| {
            assert_eq!(what_send_compares(&request, store), None);
        });
    }

    /// The compare list is the same list: every environment in it, the pinned
    /// ones at the top rather than gone from it, and the one the request is
    /// already sent to among them -- two sends to one environment can answer
    /// differently, and asking that is the reader's business.
    #[gpui::test]
    async fn the_compare_picker_lists_every_environment(cx: &mut TestAppContext) {
        let (store, request_id, view, mut cx) = build_request_view(cx).await;
        let staging = store.update(&mut cx, |store, cx| {
            let staging = store.create_environment("Staging".into(), cx);
            store.create_environment("Production".into(), cx);
            store.create_environment("Local".into(), cx);
            staging
        });
        store.update(&mut cx, |store, cx| {
            store.toggle_request_pinned_environment(request_id, staging, cx);
            store.choose_request_environment(request_id, Some(staging), cx);
        });
        draw(&mut cx);

        open_the_comparison_picker(&mut cx);

        for environment in ["Staging", "Production", "Local"] {
            assert!(
                cx.debug_bounds(match environment {
                    "Staging" => "environment-row:Staging",
                    "Production" => "environment-row:Production",
                    _ => "environment-row:Local",
                })
                .is_some(),
                "{environment} has to be in the compare list: pinned and chosen \
                 environments belong in it too"
            );
        }
        assert!(
            cx.debug_bounds("environment-picker-pinned-header")
                .is_some(),
            "and the pinned ones are the group at the top of it, not missing from it"
        );
        assert!(
            cx.debug_bounds("environment-row:nothing").is_some(),
            "with a row that asks for no comparison at all"
        );
        let _ = view;
    }

    /// One list of environments and one set of pins: pinning from the compare
    /// list is pinning, full stop, and the other list shows it.
    #[gpui::test]
    async fn pinning_from_the_compare_picker_pins_everywhere(cx: &mut TestAppContext) {
        let (store, request_id, _view, mut cx) = build_request_view(cx).await;
        let production = store.update(&mut cx, |store, cx| {
            store.create_environment("Staging".into(), cx);
            store.create_environment("Production".into(), cx)
        });
        draw(&mut cx);

        open_the_comparison_picker(&mut cx);
        let row = debug_center(&mut cx, "environment-row:Production");
        cx.simulate_mouse_move(row, None, gpui::Modifiers::none());
        cx.run_until_parked();
        draw(&mut cx);
        let pin = debug_center(&mut cx, "environment-pin:Production");
        cx.simulate_click(pin, gpui::Modifiers::none());
        cx.run_until_parked();
        draw(&mut cx);

        store.read_with(&cx, |store, _| {
            let request = store.requests.iter().find(|r| r.id == request_id).unwrap();
            assert_eq!(
                request.pinned_environments(),
                vec![production],
                "the pin belongs to the request, whichever list it was made in"
            );
            assert_eq!(
                request.compared_with(),
                None,
                "and pinning still asks for no comparison"
            );
        });

        // The other list is the same list: it shows the pin as well.
        cx.dispatch_action(menu::Cancel);
        cx.run_until_parked();
        draw(&mut cx);
        open_the_environment_picker(&mut cx);
        assert!(
            cx.debug_bounds("environment-picker-pinned-header")
                .is_some(),
            "the environment list has to show the pin made in the compare list"
        );
    }

    /// Choosing what to compare against sends nothing. Send is what sends.
    #[gpui::test]
    async fn choosing_what_to_compare_with_sends_nothing(cx: &mut TestAppContext) {
        let (store, request_id, view, mut cx) = build_request_view(cx).await;
        let production = store.update(&mut cx, |store, cx| {
            store.create_environment("Staging".into(), cx);
            store.create_environment("Production".into(), cx)
        });
        draw(&mut cx);

        open_the_comparison_picker(&mut cx);
        click_in_the_picker(&mut cx, "environment-row:Production");

        store.read_with(&cx, |store, _| {
            let request = store.requests.iter().find(|r| r.id == request_id).unwrap();
            assert_eq!(
                request.compared_with(),
                Some(production),
                "the choice has to be written down"
            );
        });
        assert!(
            cx.debug_bounds("request-environments-comparison:vs Production")
                .is_some(),
            "and the chip has to say what the next send will be compared against"
        );
        view.read_with(&cx, |view, _| {
            assert!(
                matches!(view.send_state, SendState::Idle),
                "but nothing may have been sent yet"
            );
            assert!(
                !view.comparing_environments,
                "and no comparison may be in flight"
            );
        });

        // And the way back out of it.
        open_the_comparison_picker(&mut cx);
        click_in_the_picker(&mut cx, "environment-row:nothing");
        store.read_with(&cx, |store, _| {
            let request = store.requests.iter().find(|r| r.id == request_id).unwrap();
            assert_eq!(request.compared_with(), None);
        });
        assert!(
            cx.debug_bounds("request-environments-comparison:Compare")
                .is_some(),
            "the chip says there is nothing to compare against again"
        );
    }

    /// The pinned ones go to the top of the list and stay there, separated from
    /// the rest by a line, and both groups read alphabetically -- a list in the
    /// order the environments happened to be created is one the reader has to
    /// search through every time.
    #[gpui::test]
    async fn the_picker_puts_the_pinned_ones_on_top(cx: &mut TestAppContext) {
        let (store, request_id, view, mut cx) = build_request_view(cx).await;
        // Created out of order on purpose, and mixed in case: what is painted has
        // to come from the sorting rather than from the order they arrived in.
        let (zeta, beta) = store.update(&mut cx, |store, cx| {
            let zeta = store.create_environment("zeta".into(), cx);
            store.create_environment("Alpha".into(), cx);
            let beta = store.create_environment("beta".into(), cx);
            store.create_environment("omega".into(), cx);
            (zeta, beta)
        });
        store.update(&mut cx, |store, cx| {
            store.toggle_request_pinned_environment(request_id, zeta, cx);
            store.toggle_request_pinned_environment(request_id, beta, cx);
        });
        draw(&mut cx);

        open_the_environment_picker(&mut cx);

        let heading = debug_center(&mut cx, "environment-picker-pinned-header");
        let beta_at = debug_center(&mut cx, "environment-row:beta");
        let zeta_at = debug_center(&mut cx, "environment-row:zeta");
        let active_at = debug_center(&mut cx, "environment-row:nothing");
        let alpha_at = debug_center(&mut cx, "environment-row:Alpha");
        let omega_at = debug_center(&mut cx, "environment-row:omega");

        assert!(
            heading.y < beta_at.y,
            "the heading names the group under it: heading at {heading:?}, beta at {beta_at:?}"
        );
        assert!(
            beta_at.y < zeta_at.y,
            "the pinned ones read alphabetically: beta at {beta_at:?}, zeta at {zeta_at:?}"
        );
        assert!(
            zeta_at.y < active_at.y && active_at.y < alpha_at.y,
            "both sit above everything that is not pinned, whatever its name: \
             zeta at {zeta_at:?}, Alpha at {alpha_at:?}"
        );
        assert!(
            alpha_at.y < omega_at.y,
            "the rest read alphabetically too: Alpha at {alpha_at:?}, omega at {omega_at:?}"
        );

        // The line itself cannot be measured by spacing: the list draws it as a
        // border on the row's own edge, with a negative padding that takes the
        // space back again. What is ours to get right is which row it falls
        // under, and that is asserted where it is asked for.
        let picker = view.read_with(&cx, |view, _| view.environment_picker.clone());
        let (under, rows_above_it) = picker.read_with(&cx, |picker, cx| {
            let under = picker::PickerDelegate::separators_after_indices(&picker.delegate);
            let pinned = picker.delegate.rows_for_test(cx);
            (under, pinned)
        });
        assert_eq!(
            under.len(),
            1,
            "one line, between the two groups: {under:?}"
        );
        assert_eq!(
            rows_above_it.get(under[0]).map(|name| name.as_ref()),
            Some("zeta"),
            "and it falls under the last pinned row: {rows_above_it:?}"
        );
    }

    /// A line under the pinned ones only when something follows them: a search
    /// that turns up nothing but pinned environments must not leave a line
    /// hanging off the bottom of the list.
    #[gpui::test]
    async fn a_search_that_finds_only_pinned_ones_draws_no_line(cx: &mut TestAppContext) {
        let (store, request_id, view, mut cx) = build_request_view(cx).await;
        store.update(&mut cx, |store, cx| {
            let staging = store.create_environment("qagke-stage".into(), cx);
            store.create_environment("localhost".into(), cx);
            store.toggle_request_pinned_environment(request_id, staging, cx);
        });
        draw(&mut cx);

        open_the_environment_picker(&mut cx);
        let picker = view.read_with(&cx, |view, _| view.environment_picker.clone());
        cx.update(|window, cx| {
            window.focus(&gpui::Focusable::focus_handle(&picker, cx), cx);
        });
        cx.run_until_parked();
        cx.simulate_input("qagke");
        cx.run_until_parked();
        draw(&mut cx);

        let (under, rows) = picker.read_with(&cx, |picker, cx| {
            (
                picker::PickerDelegate::separators_after_indices(&picker.delegate),
                picker.delegate.rows_for_test(cx),
            )
        });
        assert!(
            under.is_empty(),
            "nothing follows the pinned ones, so there is nothing to separate \
             them from: rows {rows:?}, line under {under:?}"
        );
    }

    /// The way back to following the active environment is searchable like any
    /// other row, so a reader with something typed is not locked out of it.
    #[gpui::test]
    async fn the_active_environment_row_can_be_searched_for(cx: &mut TestAppContext) {
        let (store, _request_id, view, mut cx) = build_request_view(cx).await;
        store.update(&mut cx, |store, cx| {
            store.create_environment("qagke-stage".into(), cx);
        });
        draw(&mut cx);

        open_the_environment_picker(&mut cx);
        let picker = view.read_with(&cx, |view, _| view.environment_picker.clone());
        cx.update(|window, cx| {
            window.focus(&gpui::Focusable::focus_handle(&picker, cx), cx);
        });
        cx.run_until_parked();
        cx.simulate_input("active");
        cx.run_until_parked();
        draw(&mut cx);

        assert!(
            cx.debug_bounds("environment-row:nothing").is_some(),
            "typing what it is called has to find it"
        );
        assert!(
            cx.debug_bounds("environment-row:qagke-stage").is_none(),
            "and leave out what does not match"
        );
    }

    /// The search box is what makes a dozen environments usable: typing narrows
    /// the list to what was typed and leaves the rest out.
    #[gpui::test]
    async fn typing_in_the_picker_narrows_the_list(cx: &mut TestAppContext) {
        let (store, _request_id, view, mut cx) = build_request_view(cx).await;
        store.update(&mut cx, |store, cx| {
            store.create_environment("ams-prod-gcp".into(), cx);
            store.create_environment("qagke-stage".into(), cx);
            store.create_environment("localhost".into(), cx);
        });
        draw(&mut cx);

        open_the_environment_picker(&mut cx);
        assert!(cx.debug_bounds("environment-row:localhost").is_some());

        // Focus is given to the picker by hand here. In the app the popover
        // takes it two frames after it is deployed, from a callback the platform
        // runs at the start of a frame -- and the frames a test paints itself do
        // not run those. Everything after this is the real path: real keystrokes
        // into the picker's own search box, and the list it rebuilds from them.
        let picker = view.read_with(&cx, |view, _| view.environment_picker.clone());
        cx.update(|window, cx| {
            window.focus(&gpui::Focusable::focus_handle(&picker, cx), cx);
        });
        cx.run_until_parked();
        cx.simulate_input("stage");
        cx.run_until_parked();
        draw(&mut cx);
        assert_eq!(
            picker.read_with(&cx, |picker, cx| picker.query(cx)),
            "stage",
            "the keystrokes have to reach the picker's own search box"
        );

        assert!(
            cx.debug_bounds("environment-row:qagke-stage").is_some(),
            "what was typed has to be found"
        );
        assert!(
            cx.debug_bounds("environment-row:localhost").is_none()
                && cx.debug_bounds("environment-row:ams-prod-gcp").is_none(),
            "and what was not typed has to be out of the way"
        );
    }

    /// Opens the compare picker from its chip, the way the reader does.
    fn open_the_comparison_picker(cx: &mut VisualTestContext) {
        let chip = debug_center(cx, "request-environments-comparison");
        cx.simulate_click(chip, gpui::Modifiers::none());
        cx.run_until_parked();
        draw(cx);
        draw(cx);
        draw(cx);
    }

    /// Opens the environment picker from the chip, the way the reader does.
    ///
    /// Painted three times over: focus reaches a popover two frames after it is
    /// deployed -- the list is linked into the dispatch tree first -- and until
    /// it does, typing goes to whatever had the focus before.
    fn open_the_environment_picker(cx: &mut VisualTestContext) {
        let chip = debug_center(cx, "request-environment-pin");
        cx.simulate_click(chip, gpui::Modifiers::none());
        cx.run_until_parked();
        draw(cx);
        draw(cx);
        draw(cx);
    }

    /// Clicks one row of the open picker.
    fn click_in_the_picker(cx: &mut VisualTestContext, row: &'static str) {
        let row = debug_center(cx, row);
        cx.simulate_click(row, gpui::Modifiers::none());
        cx.run_until_parked();
        draw(cx);
    }

    /// The table of headers has room around it. Rows flush against the edges of
    /// the pane read as one block of text, which is what he saw.
    #[gpui::test]
    async fn the_table_of_headers_is_not_flush_against_the_pane(cx: &mut TestAppContext) {
        let (_store, _request_id, view, mut cx) = build_request_view(cx).await;
        view.update_in(&mut cx, |view, window, cx| {
            view.apply_response(fake_response(200, "{}"), window, cx);
            view.response_tab = crate::response_view::ResponseTab::Headers;
        });
        draw(&mut cx);

        let row = cx
            .debug_bounds("response-header-row-0")
            .expect("the first header row was painted");
        let tabs = cx
            .debug_bounds("response-tabs")
            .expect("the tabs were painted, which is where the pane's own edge is");

        let indented = f32::from(row.origin.x) - f32::from(tabs.origin.x);
        assert!(
            indented >= 6.,
            "the row starts {indented}px in from where the tabs start: a table needs \
             room of its own, not the pane's edge"
        );
        assert!(
            f32::from(row.size.height) >= 20.,
            "a row {}px tall is text on text, not a row",
            f32::from(row.size.height)
        );
    }

    /// What the response was -- the status, how long it took, how big it is -- sits
    /// on the same row as the tabs. A row of its own for three short words is a row
    /// of the response the reader does not see.
    #[gpui::test]
    async fn what_the_response_was_shares_a_row_with_the_tabs(cx: &mut TestAppContext) {
        let (_store, _request_id, view, mut cx) = build_request_view(cx).await;
        view.update_in(&mut cx, |view, window, cx| {
            view.apply_response(fake_response(200, "{}"), window, cx);
        });
        draw(&mut cx);

        let tabs = cx
            .debug_bounds("response-tabs")
            .expect("the tabs were painted");
        let summary = cx
            .debug_bounds("response-summary")
            .expect("what the response was got painted");

        let apart = (f32::from(tabs.center().y) - f32::from(summary.center().y)).abs();
        assert!(
            apart < 4.,
            "the tabs were painted at {:?} and the summary at {:?}: {apart}px apart, \
             which is two rows rather than one",
            tabs.center(),
            summary.center()
        );
        assert!(
            f32::from(summary.origin.x) > f32::from(tabs.origin.x + tabs.size.width),
            "the summary belongs at the far end of the row, past the tabs"
        );
    }

    #[gpui::test]
    async fn typing_in_the_url_editor_persists_to_the_store(cx: &mut TestAppContext) {
        let (store, request_id, view, mut cx) = build_request_view(cx).await;

        view.update_in(&mut cx, |view, window, cx| {
            view.url_editor.update(cx, |editor, cx| {
                editor.focus_handle(cx).focus(window, cx);
            });
        });
        cx.simulate_input("https://api.example.com/users");
        cx.run_until_parked();

        store.read_with(&cx, |store, _| {
            let request = store.requests.iter().find(|r| r.id == request_id).unwrap();
            assert_eq!(request.url, "https://api.example.com/users");
        });
    }

    #[test]
    fn url_looks_malformed_detects_missing_scheme_and_variable() {
        assert!(!url_looks_malformed(""));
        assert!(!url_looks_malformed("   "));
        assert!(!url_looks_malformed("https://api.example.com/users"));
        assert!(!url_looks_malformed("{{base_url}}/users"));
        assert!(url_looks_malformed("api.example.com/users"));
        assert!(url_looks_malformed("not a url"));
    }

    #[test]
    fn json_body_is_invalid_detects_malformed_json() {
        assert!(!json_body_is_invalid(""));
        assert!(!json_body_is_invalid("   "));
        assert!(!json_body_is_invalid(r#"{"a": 1}"#));
        assert!(!json_body_is_invalid("[1, 2, 3]"));
        assert!(json_body_is_invalid(r#"{"a": 1"#));
        assert!(json_body_is_invalid("not json"));
    }

    #[gpui::test]
    async fn typing_a_malformed_url_shows_an_inline_warning(cx: &mut TestAppContext) {
        let (_store, _request_id, view, mut cx) = build_request_view(cx).await;

        view.update_in(&mut cx, |view, window, cx| {
            view.url_editor.update(cx, |editor, cx| {
                editor.focus_handle(cx).focus(window, cx);
            });
        });
        cx.simulate_input("not a url");
        cx.run_until_parked();
        draw(&mut cx);

        view.read_with(&cx, |view, _| {
            assert!(
                view.url_looks_malformed,
                "a schemeless, non-variable URL must be flagged as malformed"
            );
        });
        debug_center(&mut cx, "request-url-warning");

        view.update_in(&mut cx, |view, window, cx| {
            view.url_editor.update(cx, |editor, cx| {
                editor.set_text("https://api.example.com/users", window, cx);
            });
        });
        cx.run_until_parked();
        draw(&mut cx);

        view.read_with(&cx, |view, _| {
            assert!(
                !view.url_looks_malformed,
                "a well-formed URL must clear the warning"
            );
        });
    }

    #[gpui::test]
    async fn typing_invalid_json_in_the_body_shows_an_inline_warning(cx: &mut TestAppContext) {
        let (_store, _request_id, view, mut cx) = build_request_view(cx).await;

        view.update_in(&mut cx, |view, window, cx| {
            view.set_body_kind(BodyKind::Raw, cx);
            view.set_body_content_type(RawBodyContentType::Json, window, cx);
            view.active_tab = RequestTab::Body;
            view.body_editor.update(cx, |editor, cx| {
                editor.focus_handle(cx).focus(window, cx);
            });
        });
        cx.simulate_input("{ not json");
        cx.run_until_parked();
        draw(&mut cx);

        view.read_with(&cx, |view, _| {
            assert!(
                view.body_json_invalid,
                "malformed JSON in a JSON-typed raw body must be flagged"
            );
        });
        debug_center(&mut cx, "request-body-json-warning");

        view.update_in(&mut cx, |view, window, cx| {
            view.body_editor.update(cx, |editor, cx| {
                editor.set_text(r#"{"ok": true}"#, window, cx);
            });
        });
        cx.run_until_parked();
        draw(&mut cx);

        view.read_with(&cx, |view, _| {
            assert!(!view.body_json_invalid, "valid JSON must clear the warning");
        });
    }

    #[gpui::test]
    async fn clicking_the_method_selector_trigger_opens_its_picker(cx: &mut TestAppContext) {
        let (_store, _request_id, view, mut cx) = build_request_view(cx).await;
        draw(&mut cx);

        let handle = view.read_with(&cx, |view, _| view.method_selector_handle.clone());
        assert!(
            !handle.is_deployed(),
            "the picker must start closed before any interaction"
        );

        let trigger = debug_center(&mut cx, "request-method-selector");
        cx.simulate_click(trigger, gpui::Modifiers::none());
        cx.run_until_parked();

        assert!(
            handle.is_deployed(),
            "clicking the method selector must open its picker"
        );
    }

    #[gpui::test]
    async fn setting_the_method_directly_updates_the_stored_request(cx: &mut TestAppContext) {
        let (store, request_id, view, mut cx) = build_request_view(cx).await;
        draw(&mut cx);

        view.update(&mut cx, |view, cx| view.set_method(HttpMethod::Post, cx));
        cx.run_until_parked();

        view.read_with(&cx, |view, _| assert_eq!(view.method, HttpMethod::Post));
        store.read_with(&cx, |store, _| {
            let request = store.requests.iter().find(|r| r.id == request_id).unwrap();
            assert_eq!(request.method, HttpMethod::Post);
        });
    }

    #[gpui::test]
    async fn setting_a_custom_method_through_the_real_modal_persists_it(cx: &mut TestAppContext) {
        let (store, request_id, view, mut cx) = build_request_view(cx).await;
        draw(&mut cx);

        view.update_in(&mut cx, |view, window, cx| {
            view.start_custom_method(window, cx)
        });
        cx.run_until_parked();

        let workspace = view
            .read_with(&cx, |view, _| view.workspace.clone())
            .upgrade()
            .expect("the workspace should still be alive");
        let modal = workspace
            .read_with(&cx, |workspace, cx| {
                workspace.active_modal::<TextPromptModal>(cx)
            })
            .expect("the custom-method chip should open the method-prompt modal");
        modal.update_in(&mut cx, |modal, window, cx| {
            modal.editor.update(cx, |editor, cx| {
                editor.set_text("purge", window, cx);
            });
        });
        modal.update_in(&mut cx, |modal, window, cx| modal.confirm(window, cx));
        cx.run_until_parked();

        view.read_with(&cx, |view, _| {
            assert_eq!(view.method, HttpMethod::Custom("PURGE".to_string()));
        });
        store.read_with(&cx, |store, _| {
            let request = store.requests.iter().find(|r| r.id == request_id).unwrap();
            assert_eq!(request.method, HttpMethod::Custom("PURGE".to_string()));
        });
    }

    #[gpui::test]
    async fn the_send_button_disables_itself_while_a_request_is_in_flight(cx: &mut TestAppContext) {
        let (_store, _request_id, view, mut cx) = build_request_view(cx).await;
        view.update(&mut cx, |view, _| {
            assert!(
                matches!(view.send_state, SendState::Idle),
                "must start idle"
            );
        });
        draw(&mut cx);

        assert!(
            cx.debug_bounds("request-response-idle-hint").is_some(),
            "the idle response pane must show a hint, not render nothing"
        );

        view.update(&mut cx, |view, cx| {
            view.send_state = SendState::Sending;
            cx.notify();
        });
        draw(&mut cx);

        // The button element itself keeps a stable id/debug selector across
        // the idle/sending label change -- clicking it while disabled must
        // not re-enter `send` (which would fire a second, duplicate request).
        let send_button = debug_center(&mut cx, "request-send");
        cx.simulate_click(send_button, gpui::Modifiers::none());
        cx.run_until_parked();
        view.read_with(&cx, |view, _| {
            assert!(
                matches!(view.send_state, SendState::Sending),
                "clicking Send while already sending must be a no-op, not restart the request"
            );
        });
    }

    #[gpui::test]
    async fn switching_to_oauth2_and_typing_credentials_persists_the_full_config(
        cx: &mut TestAppContext,
    ) {
        let (store, request_id, view, mut cx) = build_request_view(cx).await;
        draw(&mut cx);

        let auth_tab = debug_center(&mut cx, "request-chip-Authorization");
        cx.simulate_click(auth_tab, gpui::Modifiers::none());
        cx.run_until_parked();
        draw(&mut cx);

        let oauth2_chip = debug_center(&mut cx, "auth-kind-chip-OAuth 2.0");
        cx.simulate_click(oauth2_chip, gpui::Modifiers::none());
        cx.run_until_parked();

        view.read_with(&cx, |view, _| assert_eq!(view.auth_kind, AuthKind::OAuth2));
        draw(&mut cx);

        let client_credentials_chip =
            debug_center(&mut cx, "oauth2-grant-type-chip-Client Credentials");
        cx.simulate_click(client_credentials_chip, gpui::Modifiers::none());
        cx.run_until_parked();

        let token_url_editor = view.read_with(&cx, |view, _| view.oauth2_token_url_editor.clone());
        let client_id_editor = view.read_with(&cx, |view, _| view.oauth2_client_id_editor.clone());
        view.update_in(&mut cx, |_, window, cx| {
            token_url_editor.update(cx, |editor, cx| editor.focus_handle(cx).focus(window, cx));
        });
        cx.simulate_input("https://auth.example.com/token");
        cx.run_until_parked();
        view.update_in(&mut cx, |_, window, cx| {
            client_id_editor.update(cx, |editor, cx| editor.focus_handle(cx).focus(window, cx));
        });
        cx.simulate_input("my-client-id");
        cx.run_until_parked();

        store.read_with(&cx, |store, _| {
            let request = store.requests.iter().find(|r| r.id == request_id).unwrap();
            match &request.auth {
                AuthConfig::OAuth2(config) => {
                    assert_eq!(
                        config.grant_type,
                        api_client::OAuth2GrantType::ClientCredentials
                    );
                    assert_eq!(config.token_url, "https://auth.example.com/token");
                    assert_eq!(config.client_id, "my-client-id");
                }
                other => panic!("expected AuthConfig::OAuth2, got {other:?}"),
            }
        });
    }

    #[gpui::test]
    async fn switching_to_aws_sigv4_and_typing_credentials_persists_the_full_config(
        cx: &mut TestAppContext,
    ) {
        let (store, request_id, view, mut cx) = build_request_view(cx).await;
        draw(&mut cx);

        let auth_tab = debug_center(&mut cx, "request-chip-Authorization");
        cx.simulate_click(auth_tab, gpui::Modifiers::none());
        cx.run_until_parked();
        draw(&mut cx);

        let aws_chip = debug_center(&mut cx, "auth-kind-chip-AWS Signature v4");
        cx.simulate_click(aws_chip, gpui::Modifiers::none());
        cx.run_until_parked();

        view.read_with(&cx, |view, _| {
            assert_eq!(view.auth_kind, AuthKind::AwsSigV4)
        });

        let access_key_editor = view.read_with(&cx, |view, _| view.aws_access_key_editor.clone());
        let secret_key_editor = view.read_with(&cx, |view, _| view.aws_secret_key_editor.clone());
        let region_editor = view.read_with(&cx, |view, _| view.aws_region_editor.clone());
        let service_editor = view.read_with(&cx, |view, _| view.aws_service_editor.clone());
        view.update_in(&mut cx, |_, window, cx| {
            access_key_editor.update(cx, |editor, cx| editor.focus_handle(cx).focus(window, cx));
        });
        cx.simulate_input("AKIDEXAMPLE");
        cx.run_until_parked();
        view.update_in(&mut cx, |_, window, cx| {
            secret_key_editor.update(cx, |editor, cx| editor.focus_handle(cx).focus(window, cx));
        });
        cx.simulate_input("wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY");
        cx.run_until_parked();
        view.update_in(&mut cx, |_, window, cx| {
            region_editor.update(cx, |editor, cx| editor.focus_handle(cx).focus(window, cx));
        });
        cx.simulate_input("us-east-1");
        cx.run_until_parked();
        view.update_in(&mut cx, |_, window, cx| {
            service_editor.update(cx, |editor, cx| editor.focus_handle(cx).focus(window, cx));
        });
        cx.simulate_input("execute-api");
        cx.run_until_parked();

        store.read_with(&cx, |store, _| {
            let request = store.requests.iter().find(|r| r.id == request_id).unwrap();
            match &request.auth {
                AuthConfig::AwsSigV4(config) => {
                    assert_eq!(config.access_key, "AKIDEXAMPLE");
                    assert_eq!(
                        config.secret_key,
                        "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
                    );
                    assert_eq!(config.region, "us-east-1");
                    assert_eq!(config.service, "execute-api");
                }
                other => panic!("expected AuthConfig::AwsSigV4, got {other:?}"),
            }
        });
    }

    #[gpui::test]
    async fn clicking_the_environment_pin_trigger_opens_its_picker(cx: &mut TestAppContext) {
        let (store, _request_id, view, mut cx) = build_request_view(cx).await;
        store.update(&mut cx, |store, cx| {
            store.create_environment("Staging".into(), cx);
        });
        draw(&mut cx);

        let handle = view.read_with(&cx, |view, _| view.environment_pin_handle.clone());
        assert!(
            !handle.is_deployed(),
            "the picker must start closed before any interaction"
        );

        let pin_trigger = debug_center(&mut cx, "request-environment-pin");
        cx.simulate_click(pin_trigger, gpui::Modifiers::none());
        cx.run_until_parked();

        assert!(
            handle.is_deployed(),
            "clicking the pin trigger must open its environment picker"
        );
    }

    #[gpui::test]
    async fn clicking_the_diff_comparison_selector_trigger_opens_its_picker(
        cx: &mut TestAppContext,
    ) {
        // Picking an actual environment entry here (which calls
        // `run_environment_comparison`) is deliberately NOT exercised via a
        // real click: that fires a request on the api-client Tokio runtime
        // (see `network_runtime::on_network_runtime`), a genuine OS thread
        // outside GPUI's virtual clock. Letting that thread wake the
        // pending task back into this test's deterministic scheduler trips
        // `TestScheduler::assert_correct_thread` ("test is not
        // deterministic") -- a hard panic, not a flake, and it fires from
        // the mere act of dispatching the click, before any explicit
        // `run_until_parked`/`draw` call. No test in this crate exercises a
        // live `api_client::execute` round trip for the same reason (see
        // `http_send.rs`). So this test covers only the picker opening and
        // listing every environment; `comparison_diff_text` (the pure
        // function that turns the request's eventual `Result` into the
        // diff text) is covered separately below, without a network round
        // trip.
        let (store, _request_id, view, mut cx) = build_request_view(cx).await;
        store.update(&mut cx, |store, cx| {
            store.create_environment("Staging".into(), cx);
        });

        view.update_in(&mut cx, |view, window, cx| {
            view.apply_response(fake_response(200, r#"{"a":1}"#), window, cx);
        });
        view.update_in(&mut cx, |view, window, cx| {
            view.apply_response(fake_response(200, r#"{"a":2}"#), window, cx);
        });
        draw(&mut cx);

        let diff_chip = debug_center(&mut cx, "response-tab-chip-Diff");
        cx.simulate_click(diff_chip, gpui::Modifiers::none());
        draw(&mut cx);

        let handle = view.read_with(&cx, |view, _| view.comparison_environment_handle.clone());
        assert!(
            !handle.is_deployed(),
            "the picker must start closed before any interaction"
        );

        let selector_trigger = debug_center(&mut cx, "diff-comparison-selector");
        cx.simulate_click(selector_trigger, gpui::Modifiers::none());
        cx.run_until_parked();

        assert!(
            handle.is_deployed(),
            "clicking the selector trigger must open its environment picker"
        );
        assert!(
            cx.debug_bounds("MENU_ITEM-vs Staging").is_some(),
            "every store environment must appear as a comparison option"
        );
    }

    #[gpui::test]
    async fn reselecting_vs_previous_response_recomputes_the_diff(cx: &mut TestAppContext) {
        let (_store, _request_id, view, mut cx) = build_request_view(cx).await;
        view.update_in(&mut cx, |view, window, cx| {
            view.apply_response(fake_response(200, r#"{"a":1}"#), window, cx);
        });
        view.update_in(&mut cx, |view, window, cx| {
            view.apply_response(fake_response(200, r#"{"a":2}"#), window, cx);
        });
        draw(&mut cx);

        let diff_chip = debug_center(&mut cx, "response-tab-chip-Diff");
        cx.simulate_click(diff_chip, gpui::Modifiers::none());
        draw(&mut cx);

        let selector_trigger = debug_center(&mut cx, "diff-comparison-selector");
        cx.simulate_click(selector_trigger, gpui::Modifiers::none());
        draw(&mut cx);

        let previous_entry = debug_center(&mut cx, "MENU_ITEM-vs Previous Response");
        cx.simulate_click(previous_entry, gpui::Modifiers::none());
        cx.run_until_parked();

        view.read_with(&cx, |view, cx| {
            assert_eq!(view.diff_comparison_environment, None);
            let diff_text = view.diff_body_editor.read(cx).text(cx);
            assert!(diff_text.contains('1'));
            assert!(diff_text.contains('2'));
        });
    }

    #[test]
    fn comparison_diff_text_diffs_against_the_baseline_on_success() {
        let baseline = fake_response(200, r#"{"a":1}"#);
        let summary = api_client::HttpResponseSummary {
            status: 200,
            status_text: "OK".to_string(),
            headers: Vec::new(),
            body: br#"{"a":2}"#.to_vec(),
            elapsed_ms: 0,
            timings: api_client::Timings::default(),
        };
        let diff_text = comparison_diff_text(&baseline, Ok(summary));
        assert!(
            diff_text.contains('-') && diff_text.contains('+'),
            "a changed body must produce a real diff, not a placeholder: {diff_text}"
        );
    }

    #[test]
    fn comparison_diff_text_reports_the_error_on_failure() {
        let baseline = fake_response(200, r#"{"a":1}"#);
        let diff_text = comparison_diff_text(&baseline, Err(anyhow::anyhow!("connection refused")));
        assert_eq!(diff_text, "Comparison request failed: connection refused");
    }

    #[gpui::test]
    async fn clicking_the_variable_picker_trigger_opens_its_picker(cx: &mut TestAppContext) {
        let (_store, _request_id, view, mut cx) = build_request_view(cx).await;
        view.update(&mut cx, |view, cx| {
            view.set_body_kind(BodyKind::Raw, cx);
            view.active_tab = RequestTab::Body;
        });
        draw(&mut cx);

        let handle = view.read_with(&cx, |view, _| view.variable_picker_handle.clone());
        assert!(
            !handle.is_deployed(),
            "the picker must start closed before any interaction"
        );

        let trigger = debug_center(&mut cx, "request-variable-picker");
        cx.simulate_click(trigger, gpui::Modifiers::none());
        cx.run_until_parked();

        assert!(
            handle.is_deployed(),
            "clicking the variable-picker trigger must open its picker"
        );
    }

    #[gpui::test]
    async fn inserting_a_dynamic_variable_token_appends_it_to_the_body(cx: &mut TestAppContext) {
        let (_store, _request_id, view, mut cx) = build_request_view(cx).await;
        let body_editor = view.update_in(&mut cx, |view, window, cx| {
            view.set_body_kind(BodyKind::Raw, cx);
            view.body_editor.update(cx, |editor, cx| {
                editor.set_text("", window, cx);
            });
            view.body_editor.clone()
        });
        view.update_in(&mut cx, |_, window, cx| {
            RequestView::insert_text_at_cursor(&body_editor, "{{$guid}}".to_string(), window, cx);
        });

        body_editor.read_with(&cx, |editor, cx| {
            assert!(
                editor.text(cx).contains("{{$guid}}"),
                "the dynamic-variable token must be inserted into the body, got: {}",
                editor.text(cx)
            );
        });
    }

    #[gpui::test]
    async fn switching_to_jwt_and_typing_a_secret_and_payload_persists_the_full_config(
        cx: &mut TestAppContext,
    ) {
        let (store, request_id, view, mut cx) = build_request_view(cx).await;
        draw(&mut cx);

        let auth_tab = debug_center(&mut cx, "request-chip-Authorization");
        cx.simulate_click(auth_tab, gpui::Modifiers::none());
        cx.run_until_parked();
        draw(&mut cx);

        let jwt_chip = debug_center(&mut cx, "auth-kind-chip-JWT Bearer");
        cx.simulate_click(jwt_chip, gpui::Modifiers::none());
        cx.run_until_parked();

        view.read_with(&cx, |view, _| assert_eq!(view.auth_kind, AuthKind::Jwt));
        draw(&mut cx);

        let hs384_chip = debug_center(&mut cx, "jwt-algorithm-chip-HS384");
        cx.simulate_click(hs384_chip, gpui::Modifiers::none());
        cx.run_until_parked();

        let secret_editor = view.read_with(&cx, |view, _| view.jwt_secret_editor.clone());
        let payload_editor = view.read_with(&cx, |view, _| view.jwt_payload_editor.clone());
        view.update_in(&mut cx, |_, window, cx| {
            secret_editor.update(cx, |editor, cx| editor.focus_handle(cx).focus(window, cx));
        });
        cx.simulate_input("test-secret");
        cx.run_until_parked();
        view.update_in(&mut cx, |_, window, cx| {
            payload_editor.update(cx, |editor, cx| editor.focus_handle(cx).focus(window, cx));
        });
        cx.simulate_input(r#"{"sub":"user-1"}"#);
        cx.run_until_parked();

        store.read_with(&cx, |store, _| {
            let request = store.requests.iter().find(|r| r.id == request_id).unwrap();
            match &request.auth {
                AuthConfig::Jwt(config) => {
                    assert_eq!(config.algorithm, JwtAlgorithm::HS384);
                    assert_eq!(config.secret, "test-secret");
                    assert_eq!(config.payload, r#"{"sub":"user-1"}"#);
                    assert!(!config.add_to_query_param);
                }
                other => panic!("expected AuthConfig::Jwt, got {other:?}"),
            }
        });
    }

    /// The Code button opens a window rather than copying at once, the window shows
    /// the request in the shape asked for, and Copy puts that shape on the
    /// clipboard.
    #[gpui::test]
    async fn the_code_button_opens_a_window_and_its_copy_button_copies_that_shape(
        cx: &mut TestAppContext,
    ) {
        let (store, request_id, view, mut cx) = build_request_view(cx).await;
        store.update(&mut cx, |store, cx| {
            store.update_request(request_id, cx, |request| {
                request.url = "https://api.example.com/ping".to_string();
            });
        });
        draw(&mut cx);

        let code_button = debug_center(&mut cx, "request-copy-curl");
        cx.simulate_click(code_button, gpui::Modifiers::none());
        cx.run_until_parked();

        assert!(
            cx.read_from_clipboard().is_none(),
            "the button opens the window; copying is what the window's own button does"
        );

        let workspace = view
            .read_with(&cx, |view, _| view.workspace.clone())
            .upgrade()
            .expect("the workspace should still be alive");
        let modal = workspace
            .read_with(&cx, |workspace, cx| {
                workspace.active_modal::<CodeSnippetModal>(cx)
            })
            .expect("the Code button should open the code window");

        let shown = modal.read_with(&cx, |modal, cx| modal.code_editor.read(cx).text(cx));
        assert!(shown.contains("curl --location"), "{shown}");
        assert!(shown.contains("https://api.example.com/ping"), "{shown}");

        // Another shape: the same request, written differently.
        modal.update_in(&mut cx, |modal, window, cx| {
            modal.show(Snippet::Go, window, cx)
        });
        cx.run_until_parked();
        let as_go = modal.read_with(&cx, |modal, cx| modal.code_editor.read(cx).text(cx));
        assert!(
            as_go.contains("net/http") && as_go.contains("https://api.example.com/ping"),
            "picking another shape has to write the same request in it:\n{as_go}"
        );
        assert!(
            !as_go.contains("curl --location"),
            "and it must not leave the shape before it behind:\n{as_go}"
        );

        modal.update_in(&mut cx, |modal, _window, cx| modal.copy(cx));
        cx.run_until_parked();

        let copied = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .expect("the window's Copy button puts the code on the clipboard");
        assert!(
            copied.contains("net/http"),
            "what is copied is the shape being shown:\n{copied}"
        );
    }

    /// Opens the code window the way the reader does, and hands back the window
    /// itself.
    ///
    /// Through a whole editor window rather than a bare `Workspace`, because the
    /// modal layer -- where this window is painted -- only exists there.
    async fn a_code_window(
        cx: &mut TestAppContext,
    ) -> (
        Entity<Workspace>,
        Entity<CodeSnippetModal>,
        VisualTestContext,
    ) {
        let (workspace, _store, _request, modal, cx) =
            a_code_window_showing("https://api.example.com/ping", cx).await;
        (workspace, modal, cx)
    }

    /// The same window, for a request whose URL the test chooses, and with the
    /// store and the request as well -- enough to open the window a second time
    /// from a request view that has never opened one.
    async fn a_code_window_showing(
        url: &str,
        cx: &mut TestAppContext,
    ) -> (
        Entity<Workspace>,
        Entity<ApiClientStore>,
        Request,
        Entity<CodeSnippetModal>,
        VisualTestContext,
    ) {
        init_test(cx);
        let store = cx.new(|cx| ApiClientStore::new(cx));
        let collection_id = store.update(cx, |store, cx| store.create_collection("A".into(), cx));
        let request_id = store.update(cx, |store, cx| {
            store.create_request(collection_id, "Get users".into(), None, cx)
        });
        store.update(cx, |store, cx| {
            store.update_request(request_id, cx, |request| {
                request.url = url.to_string();
            });
        });
        let request = store.read_with(cx, |store, _| {
            store
                .requests
                .iter()
                .find(|r| r.id == request_id)
                .unwrap()
                .clone()
        });

        let fs = project::FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let window = cx.add_window(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        let multi_workspace = window.root(&mut cx).unwrap();
        let workspace = multi_workspace.read_with(&cx, |multi, _| multi.workspace().clone());
        workspace.update_in(&mut cx, |workspace, window, cx| {
            let workspace_handle = workspace.weak_handle();
            let view = cx
                .new(|cx| RequestView::new(&request, store.clone(), workspace_handle, window, cx));
            workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
        });
        cx.run_until_parked();
        draw(&mut cx);

        let code_button = debug_center(&mut cx, "request-copy-curl");
        cx.simulate_click(code_button, gpui::Modifiers::none());
        cx.run_until_parked();
        draw(&mut cx);

        let modal = workspace
            .read_with(&cx, |workspace, cx| {
                workspace.active_modal::<CodeSnippetModal>(cx)
            })
            .expect("the Code button should open the code window");
        (workspace, store, request, modal, cx)
    }

    /// A URL whose `curl` line is far wider than the window opens, so what the
    /// window does with a line too long to show is there to be measured.
    const A_URL_TOO_LONG_FOR_THE_WINDOW: &str = "https://api.example.com/v1/reports/quarterly/consolidated\
         ?fields=identifier,display_name,description,created_at,updated_at,owner_email\
         &filter=everything-that-happened-in-the-last-quarter\
         &sort=-created_at&page=1&per_page=100";

    /// How the code is laid out in the window as it is painted.
    struct AsLaidOut {
        /// The rows the text is laid out over: one a line, until a line too long
        /// for the window is folded onto more of them.
        rows: u32,
        /// How tall one of those rows is painted.
        line_height: Pixels,
        /// The longest row the editor laid out, in characters, against the whole
        /// command's length. Folding keeps every row shorter than the command;
        /// not folding leaves one row as long as the whole of it.
        longest_row: u32,
        whole_command: u32,
    }

    fn how_the_code_is_laid_out(
        modal: &Entity<CodeSnippetModal>,
        cx: &mut VisualTestContext,
    ) -> AsLaidOut {
        let rem_size = cx.update(|window, _| window.rem_size());
        modal.update_in(cx, |modal, _window, cx| {
            modal.code_editor.update(cx, |editor, cx| {
                // Display rows, not buffer rows: a line folded at the width the
                // editor painted takes several of them, and an unfolded one takes
                // exactly one however long it is.
                let laid_out = editor.display_snapshot(cx);
                let rows = laid_out.max_point().row().0 + 1;
                let longest_row = laid_out.line_len(laid_out.longest_row());
                AsLaidOut {
                    rows,
                    line_height: editor.style(cx).text.line_height_in_pixels(rem_size),
                    longest_row,
                    whole_command: laid_out.buffer_snapshot().len().0 as u32,
                }
            })
        })
    }

    /// Copying used to close the window, while an icon in the heading copied and
    /// left it open -- two behaviours nothing on screen told apart. There is one
    /// copy now, it says it worked, and the window stays open so the reader can
    /// change the format and copy again.
    #[gpui::test]
    async fn copying_the_code_says_so_and_leaves_the_window_open(cx: &mut TestAppContext) {
        let (_workspace, modal, mut cx) = a_code_window(cx).await;

        assert!(
            cx.debug_bounds("ICON-Copy").is_none(),
            "the heading must not carry a second way to copy"
        );

        let copy = debug_center(&mut cx, "code-snippet-copy");
        cx.simulate_click(copy, gpui::Modifiers::none());
        cx.run_until_parked();
        draw(&mut cx);

        assert!(
            cx.debug_bounds("code-snippet-window").is_some(),
            "copying must not take the window away"
        );
        assert!(
            modal.read_with(&cx, |modal, _| modal.copied),
            "the button has to say the code was copied"
        );

        // And it stops saying so on its own, rather than reading as copied for
        // the rest of the window's life.
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(3));
        cx.run_until_parked();
        assert!(!modal.read_with(&cx, |modal, _| modal.copied));
    }

    /// The window is dragged by its title, and what moves is the window on screen,
    /// not merely a number in the struct.
    #[gpui::test]
    async fn the_code_window_is_dragged_by_its_title(cx: &mut TestAppContext) {
        let (_workspace, _modal, mut cx) = a_code_window(cx).await;

        let before = cx
            .debug_bounds("code-snippet-window")
            .expect("the window is painted");
        let title = debug_center(&mut cx, "code-snippet-title");

        cx.simulate_mouse_down(title, MouseButton::Left, gpui::Modifiers::none());
        cx.run_until_parked();
        draw(&mut cx);
        let moved_to = point(title.x - px(120.), title.y + px(60.));
        cx.simulate_mouse_move(moved_to, Some(MouseButton::Left), gpui::Modifiers::none());
        cx.run_until_parked();
        draw(&mut cx);
        cx.simulate_mouse_up(moved_to, MouseButton::Left, gpui::Modifiers::none());
        cx.run_until_parked();
        draw(&mut cx);

        let after = cx
            .debug_bounds("code-snippet-window")
            .expect("the window is still painted");
        assert_eq!(
            (
                after.origin.x - before.origin.x,
                after.origin.y - before.origin.y
            ),
            (px(-120.), px(60.)),
            "the window follows the pointer exactly, from {:?} to {:?}",
            before.origin,
            after.origin
        );
        assert_eq!(
            after.size, before.size,
            "dragging it must not resize it as well"
        );
    }

    /// A window dragged towards the edge stops at it: a title bar out of reach
    /// cannot be dragged back.
    #[gpui::test]
    async fn the_code_window_cannot_be_dragged_off_the_screen(cx: &mut TestAppContext) {
        let (_workspace, _modal, mut cx) = a_code_window(cx).await;

        let title = debug_center(&mut cx, "code-snippet-title");
        cx.simulate_mouse_down(title, MouseButton::Left, gpui::Modifiers::none());
        cx.run_until_parked();
        draw(&mut cx);
        cx.simulate_mouse_move(
            point(px(-4000.), px(-4000.)),
            Some(MouseButton::Left),
            gpui::Modifiers::none(),
        );
        cx.run_until_parked();
        draw(&mut cx);
        cx.simulate_mouse_up(
            point(px(-4000.), px(-4000.)),
            MouseButton::Left,
            gpui::Modifiers::none(),
        );
        cx.run_until_parked();
        draw(&mut cx);

        let painted = cx
            .debug_bounds("code-snippet-window")
            .expect("the window is still painted");
        assert!(
            painted.origin.x >= px(0.) && painted.origin.y >= px(0.),
            "the window has to stay inside the editor's own window, not {:?}",
            painted.origin
        );
    }

    /// The corner resizes it, and the smallest it goes is still a window.
    #[gpui::test]
    async fn the_code_window_is_resized_by_its_corner(cx: &mut TestAppContext) {
        let (_workspace, _modal, mut cx) = a_code_window(cx).await;

        let before = cx
            .debug_bounds("code-snippet-window")
            .expect("the window is painted");
        let grip = debug_center(&mut cx, "code-snippet-grip");

        cx.simulate_mouse_down(grip, MouseButton::Left, gpui::Modifiers::none());
        cx.run_until_parked();
        draw(&mut cx);
        let pulled_to = point(grip.x - px(200.), grip.y - px(150.));
        cx.simulate_mouse_move(pulled_to, Some(MouseButton::Left), gpui::Modifiers::none());
        cx.run_until_parked();
        draw(&mut cx);
        cx.simulate_mouse_up(pulled_to, MouseButton::Left, gpui::Modifiers::none());
        cx.run_until_parked();
        draw(&mut cx);

        let after = cx
            .debug_bounds("code-snippet-window")
            .expect("the window is still painted");
        assert_eq!(
            (after.size.width, after.size.height),
            (before.size.width - px(200.), before.size.height - px(150.)),
            "the corner resizes the window it belongs to"
        );
        assert_eq!(
            after.origin, before.origin,
            "resizing from the corner must leave the other corner where it was"
        );

        // Pulled far past the smallest it may be.
        let grip = debug_center(&mut cx, "code-snippet-grip");
        cx.simulate_mouse_down(grip, MouseButton::Left, gpui::Modifiers::none());
        cx.run_until_parked();
        draw(&mut cx);
        cx.simulate_mouse_move(
            point(px(0.), px(0.)),
            Some(MouseButton::Left),
            gpui::Modifiers::none(),
        );
        cx.run_until_parked();
        draw(&mut cx);
        cx.simulate_mouse_up(
            point(px(0.), px(0.)),
            MouseButton::Left,
            gpui::Modifiers::none(),
        );
        cx.run_until_parked();
        draw(&mut cx);

        let smallest = cx
            .debug_bounds("code-snippet-window")
            .expect("the window is still painted");
        assert!(
            smallest.size.width >= NARROWEST_SNIPPET_WINDOW
                && smallest.size.height >= SHORTEST_SNIPPET_WINDOW,
            "it may not be squeezed into nothing: {:?}",
            smallest.size
        );
    }

    /// A button let go where the window cannot see it -- outside the editor, or
    /// while another window was in front -- must not leave the window stuck to the
    /// pointer for every move afterwards.
    #[gpui::test]
    async fn the_code_window_lets_go_when_the_button_is_no_longer_down(cx: &mut TestAppContext) {
        let (_workspace, modal, mut cx) = a_code_window(cx).await;

        let title = debug_center(&mut cx, "code-snippet-title");
        cx.simulate_mouse_down(title, MouseButton::Left, gpui::Modifiers::none());
        cx.run_until_parked();
        draw(&mut cx);
        let held_at = cx
            .debug_bounds("code-snippet-window")
            .expect("the window is painted")
            .origin;

        // The pointer moves on with nothing held down: that is a button released
        // somewhere these listeners never saw.
        cx.simulate_mouse_move(
            point(title.x + px(90.), title.y + px(90.)),
            None,
            gpui::Modifiers::none(),
        );
        cx.run_until_parked();
        draw(&mut cx);
        cx.simulate_mouse_move(
            point(title.x + px(300.), title.y + px(240.)),
            None,
            gpui::Modifiers::none(),
        );
        cx.run_until_parked();
        draw(&mut cx);

        let after = cx
            .debug_bounds("code-snippet-window")
            .expect("the window is still painted")
            .origin;
        assert_eq!(
            after, held_at,
            "the window must stay where it was once nothing is held down"
        );
        assert!(
            modal.read_with(&cx, |modal, _| modal.held.is_none()),
            "and it must not still think it is being dragged"
        );
    }

    /// A line too long for the window is folded into it as the window opens:
    /// reading the command must not mean scrolling sideways for the end of it.
    #[gpui::test]
    async fn the_code_window_wraps_a_long_line_by_default(cx: &mut TestAppContext) {
        let (_workspace, _store, _request, modal, mut cx) =
            a_code_window_showing(A_URL_TOO_LONG_FOR_THE_WINDOW, cx).await;

        let editor = cx
            .debug_bounds("code-snippet-editor")
            .expect("the editor is painted");
        let laid_out = how_the_code_is_laid_out(&modal, &mut cx);

        assert!(
            laid_out.rows > 1,
            "one line of code, too long for an editor {:?} wide, has to be folded \
             onto more rows than the one it is in the buffer",
            editor.size.width
        );
        assert!(
            laid_out.line_height * laid_out.rows as f32 <= editor.size.height,
            "which the reader sees all of at once: {} rows of {:?} in {:?} of editor",
            laid_out.rows,
            laid_out.line_height,
            editor.size.height
        );
        assert!(
            laid_out.longest_row < laid_out.whole_command,
            "and no row is the whole command, which is what hanging past the right \
             edge would mean: {} of {} characters in an editor {:?} wide",
            laid_out.longest_row,
            laid_out.whole_command,
            editor.size.width
        );
    }

    /// Wrapping is the reader's to turn off, from the window itself: unchecked, the
    /// long line runs off the right of it again, and checked, it comes back.
    #[gpui::test]
    async fn the_wrap_checkbox_turns_the_folding_off_and_on(cx: &mut TestAppContext) {
        let (_workspace, _store, _request, modal, mut cx) =
            a_code_window_showing(A_URL_TOO_LONG_FOR_THE_WINDOW, cx).await;

        let checkbox = debug_center(&mut cx, "code-snippet-wrap");
        cx.simulate_click(checkbox, gpui::Modifiers::none());
        cx.run_until_parked();
        draw(&mut cx);

        let unwrapped = how_the_code_is_laid_out(&modal, &mut cx);
        assert_eq!(
            unwrapped.rows, 1,
            "with the box unchecked the command is one line again"
        );
        assert_eq!(
            unwrapped.longest_row, unwrapped.whole_command,
            "which puts the whole command on that one row, hanging past the right \
             edge of the window"
        );

        let checkbox = debug_center(&mut cx, "code-snippet-wrap");
        cx.simulate_click(checkbox, gpui::Modifiers::none());
        cx.run_until_parked();
        draw(&mut cx);

        let wrapped_again = how_the_code_is_laid_out(&modal, &mut cx);
        assert!(
            wrapped_again.rows > 1,
            "checking it again folds the line back into the window"
        );
        assert!(
            wrapped_again.longest_row < wrapped_again.whole_command,
            "and no row is the whole command any more: {} of {}",
            wrapped_again.longest_row,
            wrapped_again.whole_command
        );
    }

    /// The window comes back where and at the size it was left, even for a request
    /// view that has never opened one: the reader's dragging belongs to the window,
    /// not to the tab that happened to open it.
    #[gpui::test]
    async fn the_code_window_opens_where_it_was_left(cx: &mut TestAppContext) {
        let (workspace, store, request, _modal, mut cx) =
            a_code_window_showing("https://api.example.com/ping", cx).await;

        let grip = debug_center(&mut cx, "code-snippet-grip");
        cx.simulate_mouse_down(grip, MouseButton::Left, gpui::Modifiers::none());
        cx.run_until_parked();
        draw(&mut cx);
        let pulled_to = point(grip.x - px(180.), grip.y - px(120.));
        cx.simulate_mouse_move(pulled_to, Some(MouseButton::Left), gpui::Modifiers::none());
        cx.run_until_parked();
        draw(&mut cx);
        cx.simulate_mouse_up(pulled_to, MouseButton::Left, gpui::Modifiers::none());
        cx.run_until_parked();
        draw(&mut cx);

        let left_at = cx
            .debug_bounds("code-snippet-window")
            .expect("the window is painted");
        assert!(
            left_at.size.width < SNIPPET_WINDOW_SIZE.width
                && left_at.size.height < SNIPPET_WINDOW_SIZE.height,
            "the drag has to have resized it, or there is nothing to remember: {:?}",
            left_at.size
        );
        // Where it was left is written down a tenth of a second after the dragging
        // stops.
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(200));
        cx.run_until_parked();

        // Touching the title brings the window forward, which is what makes Escape
        // reach it.
        let title = debug_center(&mut cx, "code-snippet-title");
        cx.simulate_click(title, gpui::Modifiers::none());
        cx.run_until_parked();
        cx.dispatch_action(menu::Cancel);
        cx.run_until_parked();
        draw(&mut cx);
        assert!(
            workspace
                .read_with(&cx, |workspace, cx| workspace
                    .active_modal::<CodeSnippetModal>(cx))
                .is_none(),
            "the window has to be closed before opening it again means anything"
        );

        let another_view = workspace.update_in(&mut cx, |workspace, window, cx| {
            let workspace_handle = workspace.weak_handle();
            cx.new(|cx| RequestView::new(&request, store.clone(), workspace_handle, window, cx))
        });
        another_view.update_in(&mut cx, |view, window, cx| view.show_as_code(window, cx));
        cx.run_until_parked();
        draw(&mut cx);

        let opened_at = cx
            .debug_bounds("code-snippet-window")
            .expect("the window opens again");
        assert_eq!(
            (opened_at.origin, opened_at.size),
            (left_at.origin, left_at.size),
            "it has to come back where it was left, at the size it was left"
        );
    }

    /// A click anywhere else is somebody working beside the window, not asking for
    /// it to close. Escape is what closes it.
    #[gpui::test]
    async fn a_click_beside_the_code_window_leaves_it_open(cx: &mut TestAppContext) {
        let (workspace, _modal, mut cx) = a_code_window(cx).await;

        let painted = cx
            .debug_bounds("code-snippet-window")
            .expect("the window is painted");
        let beside = point(
            painted.origin.x / 2.,
            painted.origin.y + painted.size.height + px(80.),
        );
        cx.simulate_click(beside, gpui::Modifiers::none());
        cx.run_until_parked();
        draw(&mut cx);

        assert!(
            workspace
                .read_with(&cx, |workspace, cx| workspace
                    .active_modal::<CodeSnippetModal>(cx))
                .is_some(),
            "a click beside the window must leave it open"
        );

        // Touching its title brings the window forward again, which is what makes
        // Escape reach it.
        let title = debug_center(&mut cx, "code-snippet-title");
        cx.simulate_click(title, gpui::Modifiers::none());
        cx.run_until_parked();
        draw(&mut cx);
        cx.dispatch_action(menu::Cancel);
        cx.run_until_parked();
        assert!(
            workspace
                .read_with(&cx, |workspace, cx| workspace
                    .active_modal::<CodeSnippetModal>(cx))
                .is_none(),
            "and Escape is what closes it"
        );
    }

    #[gpui::test]
    async fn a_variable_token_in_the_url_is_highlighted(cx: &mut TestAppContext) {
        let (_store, _request_id, view, mut cx) = build_request_view(cx).await;

        view.update_in(&mut cx, |view, window, cx| {
            view.url_editor.update(cx, |editor, cx| {
                editor.focus_handle(cx).focus(window, cx);
            });
        });
        cx.simulate_input("{{base_url}}/users");
        cx.run_until_parked();

        view.update(&mut cx, |view, cx| {
            view.url_editor.update(cx, |editor, cx| {
                let (_, ranges) = editor
                    .text_highlights(HighlightKey::ApiClientVariableToken, cx)
                    .expect("the {{base_url}} token should be highlighted");
                assert_eq!(ranges.len(), 1);
                let snapshot = editor.buffer().read(cx).snapshot(cx);
                let range = ranges[0].clone();
                let highlighted_text = snapshot
                    .text_for_range(range.start..range.end)
                    .collect::<String>();
                assert_eq!(highlighted_text, "{{base_url}}");
            });
        });
    }

    /// A row is added the way a row is added in a spreadsheet: by typing into the
    /// blank one at the end, which then grows a new blank one under it. There is no
    /// button to press first.
    #[gpui::test]
    async fn typing_into_the_last_row_adds_it_and_leaves_another_blank_one(
        cx: &mut TestAppContext,
    ) {
        let (store, request_id, view, mut cx) = build_request_view(cx).await;
        draw(&mut cx);

        assert_eq!(
            view.read_with(&cx, |view, _| view.param_rows.len()),
            1,
            "a table opens with one row to type into"
        );

        // Straight into the cells of that row, the way a click into them would.
        let cell =
            |view: &Entity<RequestView>, cx: &mut VisualTestContext, at: usize, column: Column| {
                view.read_with(cx, |view, _| view.param_rows[at].cell(column).clone())
            };
        let key = cell(&view, &mut cx, 0, Column::Key);
        view.update_in(&mut cx, |_, window, cx| {
            key.update(cx, |editor, cx| editor.focus_handle(cx).focus(window, cx));
        });
        cx.simulate_input("page");
        cx.run_until_parked();
        let value = cell(&view, &mut cx, 0, Column::Value);
        view.update_in(&mut cx, |_, window, cx| {
            value.update(cx, |editor, cx| editor.focus_handle(cx).focus(window, cx));
        });
        cx.simulate_input("1");
        cx.run_until_parked();
        let description = cell(&view, &mut cx, 0, Column::Description);
        view.update_in(&mut cx, |_, window, cx| {
            description.update(cx, |editor, cx| editor.focus_handle(cx).focus(window, cx));
        });
        cx.simulate_input("which page to read");
        cx.run_until_parked();
        draw(&mut cx);

        store.read_with(&cx, |store, _| {
            let request = store.requests.iter().find(|r| r.id == request_id).unwrap();
            assert_eq!(request.params.len(), 1, "only the row that says something");
            assert_eq!(request.params[0].key, "page");
            assert_eq!(request.params[0].value, "1");
            assert_eq!(
                request.params[0].description.as_deref(),
                Some("which page to read"),
                "the third column is kept with the request, like the other two"
            );
        });
        assert_eq!(
            view.read_with(&cx, |view, _| view.param_rows.len()),
            2,
            "and another blank row is waiting under it"
        );
    }

    /// The text form of a table has two columns and the table has three, so a trip
    /// through Bulk Edit must not take the notes away with it.
    #[gpui::test]
    async fn a_trip_through_bulk_edit_keeps_the_descriptions(cx: &mut TestAppContext) {
        let (store, request_id, view, mut cx) = build_request_view(cx).await;
        draw(&mut cx);

        for (column, text) in [
            (Column::Key, "page"),
            (Column::Value, "1"),
            (Column::Description, "which page to read"),
        ] {
            let cell = view.read_with(&cx, |view, _| view.param_rows[0].cell(column).clone());
            view.update_in(&mut cx, |_, window, cx| {
                cell.update(cx, |editor, cx| editor.focus_handle(cx).focus(window, cx));
            });
            cx.simulate_input(text);
            cx.run_until_parked();
        }
        draw(&mut cx);

        // In to fix the value, and back out again.
        view.update_in(&mut cx, |view, window, cx| {
            view.toggle_params_bulk_edit(window, cx);
        });
        cx.run_until_parked();
        view.update_in(&mut cx, |view, window, cx| {
            view.param_bulk_editor.update(cx, |editor, cx| {
                editor.set_text("page: 2", window, cx);
            });
        });
        cx.run_until_parked();
        view.update_in(&mut cx, |view, window, cx| {
            view.toggle_params_bulk_edit(window, cx);
        });
        cx.run_until_parked();
        draw(&mut cx);

        store.read_with(&cx, |store, _| {
            let request = store.requests.iter().find(|r| r.id == request_id).unwrap();
            assert_eq!(request.params.len(), 1);
            assert_eq!(
                request.params[0].value, "2",
                "the value the reader went in for"
            );
            assert_eq!(
                request.params[0].description.as_deref(),
                Some("which page to read"),
                "and the note they never touched is still there"
            );
        });
        assert_eq!(
            view.read_with(&cx, |view, cx| view.param_rows[0]
                .description_editor
                .read(cx)
                .text(cx)),
            "which page to read",
            "the table shows it again too"
        );
    }

    /// A cell holding a space is a cell somebody typed in. Rows are dropped for
    /// being untouched, never for looking empty.
    #[gpui::test]
    async fn a_row_holding_only_a_space_is_still_a_row(cx: &mut TestAppContext) {
        let (store, request_id, view, mut cx) = build_request_view(cx).await;
        draw(&mut cx);

        let key = view.read_with(&cx, |view, _| view.param_rows[0].key_editor.clone());
        view.update_in(&mut cx, |_, window, cx| {
            key.update(cx, |editor, cx| editor.focus_handle(cx).focus(window, cx));
        });
        cx.simulate_input(" ");
        cx.run_until_parked();
        draw(&mut cx);

        store.read_with(&cx, |store, _| {
            let request = store.requests.iter().find(|r| r.id == request_id).unwrap();
            assert_eq!(
                request.params.len(),
                1,
                "what was typed is kept, whatever it looks like"
            );
            assert_eq!(request.params[0].key, " ");
        });
    }

    /// The three columns are a table: each cell sits under its own heading, and the
    /// rows line up with each other rather than each being its own little box.
    #[gpui::test]
    async fn the_columns_line_up_under_their_headings(cx: &mut TestAppContext) {
        let (_store, _request_id, view, mut cx) = build_request_view(cx).await;
        draw(&mut cx);

        // Two rows written the way a reader writes them: into the blank row at the
        // end, which grows another one under it.
        for (at, key, value) in [(0usize, "page", "1"), (1, "per_page", "50")] {
            for (column, text) in [(Column::Key, key), (Column::Value, value)] {
                let cell = view.read_with(&cx, |view, _| view.param_rows[at].cell(column).clone());
                view.update_in(&mut cx, |_, window, cx| {
                    cell.update(cx, |editor, cx| editor.focus_handle(cx).focus(window, cx));
                });
                cx.simulate_input(text);
                cx.run_until_parked();
            }
            draw(&mut cx);
        }

        for column in Column::ALL {
            let heading = cx
                .debug_bounds(format!("params-heading-{}", column.name()).leak())
                .unwrap_or_else(|| panic!("the {} column has to have a heading", column.name()));
            for row in 0..2 {
                let cell = cx
                    .debug_bounds(format!("params-cell-{row}-{}", column.name()).leak())
                    .unwrap_or_else(|| panic!("row {row} is missing its {} cell", column.name()));
                assert_eq!(
                    cell.origin.x,
                    heading.origin.x,
                    "row {row}'s {} cell has to start where its heading does",
                    column.name()
                );
                assert_eq!(cell.size.width, heading.size.width, "and be as wide as it");
                assert!(
                    cell.size.height > px(1.),
                    "and occupy real screen area: {:?}",
                    cell.size
                );
            }
        }

        let first = cx.debug_bounds("params-row-0").expect("the first row");
        let second = cx.debug_bounds("params-row-1").expect("the second row");
        assert_eq!(
            second.origin.y,
            first.origin.y + first.size.height,
            "rows sit directly on top of one another, sharing their line"
        );
    }

    /// A query typed into the address bar shows up as rows below it, the way it
    /// does in every other client: the two are one query string.
    #[gpui::test]
    async fn a_query_typed_into_the_address_bar_becomes_rows(cx: &mut TestAppContext) {
        let (store, request_id, view, mut cx) = build_request_view(cx).await;
        draw(&mut cx);

        view.update_in(&mut cx, |view, window, cx| {
            view.url_editor.update(cx, |editor, cx| {
                editor.focus_handle(cx).focus(window, cx);
            });
        });
        cx.simulate_input("{{financials-api}}/v1/instruments/:instrument_id/ratios?hello=world");
        cx.run_until_parked();
        draw(&mut cx);

        let rows = view.read_with(&cx, |view, cx| {
            view.param_rows
                .iter()
                .filter(|row| !row.is_blank(cx))
                .map(|row| {
                    (
                        row.key_editor.read(cx).text(cx),
                        row.value_editor.read(cx).text(cx),
                    )
                })
                .collect::<Vec<_>>()
        });
        assert_eq!(
            rows,
            vec![
                (":instrument_id".to_string(), String::new()),
                ("hello".to_string(), "world".to_string()),
            ],
            "the place in the path is a row waiting for a value, then what was typed \
             after the `?`"
        );
        store.read_with(&cx, |store, _| {
            let request = store.requests.iter().find(|r| r.id == request_id).unwrap();
            assert_eq!(request.params.len(), 2);
            assert_eq!(request.params[1].key, "hello");
            assert_eq!(request.params[1].value, "world");
        });
        // And it is painted, not merely stored.
        assert!(
            cx.debug_bounds("params-cell-0-key").is_some(),
            "the row has to be in the table on screen"
        );

        // A second one, typed on the end.
        view.update_in(&mut cx, |view, window, cx| {
            view.url_editor.update(cx, |editor, cx| {
                editor.focus_handle(cx).focus(window, cx);
            });
        });
        cx.simulate_input("&page=2");
        cx.run_until_parked();
        draw(&mut cx);
        let rows = view.read_with(&cx, |view, cx| {
            view.param_rows
                .iter()
                .filter(|row| !row.is_blank(cx))
                .map(|row| row.key_editor.read(cx).text(cx))
                .collect::<Vec<_>>()
        });
        assert_eq!(
            rows,
            vec![
                ":instrument_id".to_string(),
                "hello".to_string(),
                "page".to_string()
            ]
        );
    }

    /// The other way round: a row written into the table shows up after the `?`,
    /// and switching a row off takes it out of the address bar. A row that names a
    /// place in the path is not a query parameter and stays out of it.
    #[gpui::test]
    async fn a_row_written_into_the_table_shows_up_in_the_address_bar(cx: &mut TestAppContext) {
        let (_store, _request_id, view, mut cx) = build_request_view(cx).await;
        view.update_in(&mut cx, |view, window, cx| {
            view.url_editor.update(cx, |editor, cx| {
                editor.set_text(
                    "https://example.com/v1/instruments/:instrument_id/ratios",
                    window,
                    cx,
                );
            });
        });
        cx.run_until_parked();
        draw(&mut cx);

        let type_into = |cx: &mut VisualTestContext,
                         view: &Entity<RequestView>,
                         at: usize,
                         column: Column,
                         text: &str| {
            let cell = view.read_with(cx, |view, _| view.param_rows[at].cell(column).clone());
            view.update_in(cx, |_, window, cx| {
                cell.update(cx, |editor, cx| editor.focus_handle(cx).focus(window, cx));
            });
            cx.simulate_input(text);
            cx.run_until_parked();
        };

        type_into(&mut cx, &view, 0, Column::Key, ":instrument_id");
        type_into(&mut cx, &view, 0, Column::Value, "6408");
        draw(&mut cx);
        assert_eq!(
            view.read_with(&cx, |view, cx| view.url_editor.read(cx).text(cx)),
            "https://example.com/v1/instruments/:instrument_id/ratios",
            "a place in the path is filled at send time, not hung on the end as a query"
        );

        type_into(&mut cx, &view, 1, Column::Key, "hello");
        type_into(&mut cx, &view, 1, Column::Value, "world");
        draw(&mut cx);
        assert_eq!(
            view.read_with(&cx, |view, cx| view.url_editor.read(cx).text(cx)),
            "https://example.com/v1/instruments/:instrument_id/ratios?hello=world",
            "what is written in the table shows up after the question mark"
        );

        // Switched off, it leaves the address bar -- that is what switching it off
        // means.
        let switch = debug_center(&mut cx, "params-toggle-1");
        cx.simulate_click(switch, gpui::Modifiers::none());
        cx.run_until_parked();
        draw(&mut cx);
        assert_eq!(
            view.read_with(&cx, |view, cx| view.url_editor.read(cx).text(cx)),
            "https://example.com/v1/instruments/:instrument_id/ratios",
            "a row that is switched off is not part of the request"
        );
        assert_eq!(
            view.read_with(&cx, |view, cx| view
                .param_rows
                .iter()
                .filter(|row| !row.is_blank(cx))
                .count()),
            2,
            "and it is still in the table, waiting to be switched back on"
        );
    }

    /// A `:name` written into the path is a place waiting for a value, so it shows
    /// up as a row of the table -- and the value written beside it survives the name
    /// being typed out letter by letter.
    #[gpui::test]
    async fn a_place_in_the_path_becomes_a_row(cx: &mut TestAppContext) {
        let (store, request_id, view, mut cx) = build_request_view(cx).await;
        view.update_in(&mut cx, |view, window, cx| {
            view.url_editor.update(cx, |editor, cx| {
                editor.set_text(
                    "{{financials-api}}/v1/instruments/instrument_id/ratios",
                    window,
                    cx,
                );
            });
        });
        cx.run_until_parked();
        draw(&mut cx);
        assert_eq!(
            view.read_with(&cx, |view, cx| view
                .param_rows
                .iter()
                .filter(|row| !row.is_blank(cx))
                .count()),
            0,
            "a plain path holds no places"
        );

        // The colon put in front of it, the way the reader writes a place.
        view.update_in(&mut cx, |view, window, cx| {
            view.url_editor.update(cx, |editor, cx| {
                editor.set_text(
                    "{{financials-api}}/v1/instruments/:instrument_id/ratios",
                    window,
                    cx,
                );
            });
        });
        cx.run_until_parked();
        draw(&mut cx);

        assert_eq!(
            view.read_with(&cx, |view, cx| view
                .param_rows
                .iter()
                .filter(|row| !row.is_blank(cx))
                .map(|row| (
                    row.key_editor.read(cx).text(cx),
                    row.value_editor.read(cx).text(cx)
                ))
                .collect::<Vec<_>>()),
            vec![(":instrument_id".to_string(), String::new())],
            "the place is a row of its own, waiting for a value"
        );
        assert!(
            cx.debug_bounds("params-cell-0-key").is_some(),
            "and it is in the table on screen"
        );

        // The value written beside it.
        let value = view.read_with(&cx, |view, _| view.param_rows[0].value_editor.clone());
        view.update_in(&mut cx, |_, window, cx| {
            value.update(cx, |editor, cx| editor.focus_handle(cx).focus(window, cx));
        });
        cx.simulate_input("6408");
        cx.run_until_parked();
        draw(&mut cx);
        store.read_with(&cx, |store, _| {
            let request = store.requests.iter().find(|r| r.id == request_id).unwrap();
            assert_eq!(request.params[0].key, ":instrument_id");
            assert_eq!(request.params[0].value, "6408");
        });
        assert_eq!(
            view.read_with(&cx, |view, cx| view.url_editor.read(cx).text(cx)),
            "{{financials-api}}/v1/instruments/:instrument_id/ratios",
            "a place is filled at send time and never hung on the end as a query"
        );

        // The name typed out further: the value stays beside it rather than being
        // thrown away and typed again.
        view.update_in(&mut cx, |view, window, cx| {
            view.url_editor.update(cx, |editor, cx| {
                editor.set_text(
                    "{{financials-api}}/v1/instruments/:instrument/ratios",
                    window,
                    cx,
                );
            });
        });
        cx.run_until_parked();
        draw(&mut cx);
        assert_eq!(
            view.read_with(&cx, |view, cx| view
                .param_rows
                .iter()
                .filter(|row| !row.is_blank(cx))
                .map(|row| (
                    row.key_editor.read(cx).text(cx),
                    row.value_editor.read(cx).text(cx)
                ))
                .collect::<Vec<_>>()),
            vec![(":instrument".to_string(), "6408".to_string())],
            "renaming the place keeps what was written beside it"
        );

        // And taken out of the path, its row goes with it.
        view.update_in(&mut cx, |view, window, cx| {
            view.url_editor.update(cx, |editor, cx| {
                editor.set_text("{{financials-api}}/v1/instruments/6408/ratios", window, cx);
            });
        });
        cx.run_until_parked();
        draw(&mut cx);
        assert_eq!(
            view.read_with(&cx, |view, cx| view
                .param_rows
                .iter()
                .filter(|row| !row.is_blank(cx))
                .count()),
            0,
            "no place in the path, no row for one"
        );
    }

    /// `?=1` is a query somebody wrote on purpose. Writing another row must not
    /// quietly take it out of the address bar.
    #[gpui::test]
    async fn a_parameter_with_no_name_is_left_where_it_was_written(cx: &mut TestAppContext) {
        let (_store, _request_id, view, mut cx) = build_request_view(cx).await;
        view.update_in(&mut cx, |view, window, cx| {
            view.url_editor.update(cx, |editor, cx| {
                editor.set_text("https://example.com/things?=1", window, cx);
            });
        });
        cx.run_until_parked();
        draw(&mut cx);

        assert_eq!(
            view.read_with(&cx, |view, cx| view
                .param_rows
                .iter()
                .filter(|row| !row.is_blank(cx))
                .map(|row| (
                    row.key_editor.read(cx).text(cx),
                    row.value_editor.read(cx).text(cx)
                ))
                .collect::<Vec<_>>()),
            vec![(String::new(), "1".to_string())],
            "a nameless parameter is still a row"
        );

        // Another row written under it, which is what makes the address bar be
        // written again.
        for (column, text) in [(Column::Key, "b"), (Column::Value, "2")] {
            let cell = view.read_with(&cx, |view, _| view.param_rows[1].cell(column).clone());
            view.update_in(&mut cx, |_, window, cx| {
                cell.update(cx, |editor, cx| editor.focus_handle(cx).focus(window, cx));
            });
            cx.simulate_input(text);
            cx.run_until_parked();
        }
        draw(&mut cx);

        assert_eq!(
            view.read_with(&cx, |view, cx| view.url_editor.read(cx).text(cx)),
            "https://example.com/things?=1&b=2",
            "the nameless one stays where it was written"
        );
    }

    /// Send is the one thing the view exists for, so it is the biggest, brightest
    /// thing in its row -- and easy to hit without aiming.
    #[gpui::test]
    async fn send_is_the_largest_thing_in_the_row(cx: &mut TestAppContext) {
        let (_store, _request_id, _view, mut cx) = build_request_view(cx).await;
        draw(&mut cx);

        let send = cx
            .debug_bounds("request-send")
            .expect("the Send button is painted");
        assert!(
            send.size.width >= px(136.) && send.size.height >= px(34.),
            "Send has to be a block worth aiming at, not a chip: {:?}",
            send.size
        );

        // Bigger than the quiet control beside it, which is the point of it being
        // the bright one.
        let code = cx
            .debug_bounds("request-copy-curl")
            .expect("the Code button is painted");
        assert!(
            send.size.width > code.size.width && send.size.height > code.size.height,
            "Send {:?} has to stand out against Code {:?}",
            send.size,
            code.size
        );
    }

    /// What a request is sent with is read right after where it is sent: the
    /// Authorization tab follows Params.
    #[gpui::test]
    async fn authorization_is_the_second_tab(cx: &mut TestAppContext) {
        let (_store, _request_id, _view, mut cx) = build_request_view(cx).await;
        draw(&mut cx);

        let places: Vec<(&str, gpui::Point<Pixels>)> = [
            "Params",
            "Authorization",
            "Headers",
            "Body",
            "Scripts",
            "Examples",
        ]
        .into_iter()
        .map(|label| {
            let chip = cx
                .debug_bounds(format!("request-chip-{label}").leak())
                .unwrap_or_else(|| panic!("the {label} tab is painted"));
            (label, chip.origin)
        })
        .collect();

        for pair in places.windows(2) {
            let (before, at) = pair[0];
            let (after, next) = pair[1];
            assert!(
                at.x < next.x,
                "{before} has to be painted left of {after}: {at:?} against {next:?}"
            );
        }
    }

    /// The headers a request sends are read in one place: the ones this client adds
    /// sit in the same table as the ones the reader wrote, under the same headings.
    #[gpui::test]
    async fn the_automatic_headers_are_rows_of_the_same_table(cx: &mut TestAppContext) {
        let (_store, _request_id, view, mut cx) = build_request_view(cx).await;
        view.update_in(&mut cx, |view, _window, cx| {
            view.active_tab = RequestTab::Headers;
            cx.notify();
        });
        draw(&mut cx);

        let heading = cx
            .debug_bounds("headers-heading-key")
            .expect("the table has its headings");
        let table = cx
            .debug_bounds("headers-table")
            .expect("the table itself is painted");

        for key in [
            "Cache-Control",
            "User-Agent",
            "Accept",
            "Content-Length",
            "Host",
        ] {
            let row = cx
                .debug_bounds(format!("headers-fixed-{key}").leak())
                .unwrap_or_else(|| panic!("{key} has to be a row of the table"));
            assert!(
                row.origin.x >= table.origin.x
                    && row.origin.x + row.size.width <= table.origin.x + table.size.width + px(1.),
                "{key} has to sit inside the table, not beside it: {:?} against {:?}",
                row,
                table
            );
            assert!(
                row.origin.y > heading.origin.y,
                "{key} belongs under the headings"
            );
            let switch = cx
                .debug_bounds(format!("auto-header-toggle-{key}").leak())
                .unwrap_or_else(|| panic!("{key} keeps its own switch"));
            assert!(
                switch.origin.x < heading.origin.x,
                "the switch stays in the column left of Key"
            );
        }

        // The row the reader types into is still the last thing in the table.
        let blank = cx
            .debug_bounds("headers-row-0")
            .expect("the blank row is there too");
        let last_automatic = cx
            .debug_bounds("headers-fixed-Host")
            .expect("Host is painted");
        assert!(
            blank.origin.y > last_automatic.origin.y,
            "what the reader writes goes under what the client adds"
        );

        // Hidden away again, the table holds only the reader's own rows.
        let switch = debug_center(&mut cx, "hide-auto-headers-toggle");
        cx.simulate_click(switch, gpui::Modifiers::none());
        cx.run_until_parked();
        draw(&mut cx);
        assert!(
            cx.debug_bounds("headers-fixed-Accept").is_none(),
            "hiding them takes them out of the table"
        );
        assert!(
            cx.debug_bounds("headers-row-0").is_some(),
            "and leaves the rows the reader writes alone"
        );
    }

    /// Tab walks the cells, which is how a table is filled in without reaching for
    /// the mouse. The cell editor would otherwise take the tab for itself.
    #[gpui::test]
    async fn tab_walks_from_cell_to_cell(cx: &mut TestAppContext) {
        let (_store, _request_id, view, mut cx) = build_request_view(cx).await;
        draw(&mut cx);

        let cells: Vec<Entity<Editor>> = view.read_with(&cx, |view, _| {
            Column::ALL
                .iter()
                .map(|column| view.param_rows[0].cell(*column).clone())
                .collect()
        });
        view.update_in(&mut cx, |_, window, cx| {
            cells[0].update(cx, |editor, cx| editor.focus_handle(cx).focus(window, cx));
        });
        cx.run_until_parked();
        draw(&mut cx);

        for expected in [1usize, 2] {
            cx.simulate_keystrokes("tab");
            cx.run_until_parked();
            let focused = view.update_in(&mut cx, |_, window, cx| {
                cells
                    .iter()
                    .position(|editor| editor.read(cx).focus_handle(cx).is_focused(window))
            });
            assert_eq!(
                focused,
                Some(expected),
                "tab has to move to the next cell, not indent inside the one it is in"
            );
        }

        cx.simulate_keystrokes("shift-tab");
        cx.run_until_parked();
        let focused = view.update_in(&mut cx, |_, window, cx| {
            cells
                .iter()
                .position(|editor| editor.read(cx).focus_handle(cx).is_focused(window))
        });
        assert_eq!(focused, Some(1), "and shift-tab back again");
    }

    /// `apply_response` is the seam between the network call (real I/O,
    /// untestable without a live server -- see `api_client::http_send`'s own
    /// unit tests for the request-building/response-parsing logic that seam
    /// wraps) and the UI: everything from here down is pure GPUI state, so
    /// it is exercised directly instead of driving a real HTTP round trip.
    fn fake_response(status: u16, body: &str) -> ResponseData {
        ResponseData::from_summary(api_client::HttpResponseSummary {
            status,
            status_text: "OK".to_string(),
            headers: vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                ("Set-Cookie".to_string(), "session=abc123".to_string()),
            ],
            body: body.as_bytes().to_vec(),
            elapsed_ms: 42,
            timings: api_client::Timings::default(),
        })
    }

    #[gpui::test]
    async fn a_successful_response_renders_status_time_size_and_pretty_prints_json(
        cx: &mut TestAppContext,
    ) {
        let (_store, _request_id, view, mut cx) = build_request_view(cx).await;

        view.update_in(&mut cx, |view, window, cx| {
            view.apply_response(fake_response(201, r#"{"id":1}"#), window, cx);
        });
        cx.run_until_parked();

        view.read_with(&cx, |view, cx| {
            assert!(
                matches!(&view.send_state, SendState::Success(response) if response.status == 201)
            );
            let pretty_text = view.pretty_body_editor.read(cx).text(cx);
            assert!(
                pretty_text.contains("\"id\": 1"),
                "expected pretty-printed JSON, got: {pretty_text}"
            );
            let raw_text = view.raw_body_editor.read(cx).text(cx);
            assert_eq!(raw_text, r#"{"id":1}"#);
        });
    }

    // Reproduces the reported bug: the response region used to be a regular
    // child of one page-wide scroll container, so it never had a bounded
    // height of its own -- a short response left dead space or stretched to
    // fill the window, and a long one just kept pushing the whole page
    // (including the request form above it) taller instead of scrolling on
    // its own. It must now size to its own content when that fits, and cap
    // at the remaining window space (scrolling internally) when it doesn't.
    #[gpui::test]
    async fn response_region_sizes_to_content_and_scrolls_when_it_overflows(
        cx: &mut TestAppContext,
    ) {
        let (_store, _request_id, view, mut cx) = build_request_view(cx).await;

        view.update_in(&mut cx, |view, window, cx| {
            view.apply_response(fake_response(200, r#"{"id":1}"#), window, cx);
        });
        cx.run_until_parked();
        draw(&mut cx);

        let window_bounds = cx
            .debug_bounds("api-client-request-view")
            .expect("expected debug bounds for the request view");
        let short_response_bounds = cx
            .debug_bounds("api-client-response-scroll-region")
            .expect("expected debug bounds for the response region");
        assert!(
            short_response_bounds.size.height < window_bounds.size.height / 2.,
            "a short response must size to its own content, not stretch to fill the window: \
             response height {:?}, window height {:?}",
            short_response_bounds.size.height,
            window_bounds.size.height,
        );
        let max_offset_short = view.read_with(&cx, |view, _| view.scroll_handle.max_offset());
        assert_eq!(
            max_offset_short.y,
            px(0.),
            "a short response must fit without needing to scroll"
        );

        let long_body = format!(
            "{{{}}}",
            (0..500)
                .map(|i| format!(r#""field_{i}": {i}"#))
                .collect::<Vec<_>>()
                .join(",")
        );
        view.update_in(&mut cx, |view, window, cx| {
            view.apply_response(fake_response(200, &long_body), window, cx);
        });
        cx.run_until_parked();
        draw(&mut cx);

        let long_response_bounds = cx
            .debug_bounds("api-client-response-scroll-region")
            .expect("expected debug bounds for the response region");
        assert!(
            long_response_bounds.size.height <= window_bounds.size.height,
            "the response region must be capped at the window's height, not grow past it: \
             response height {:?}, window height {:?}",
            long_response_bounds.size.height,
            window_bounds.size.height,
        );
        let max_offset_long = view.read_with(&cx, |view, _| view.scroll_handle.max_offset());
        assert!(
            max_offset_long.y > px(0.),
            "a response too tall for the remaining window space must become scrollable, got max_offset {max_offset_long:?}"
        );
    }

    #[gpui::test]
    async fn clicking_the_fullscreen_toggle_hides_the_request_editor_and_a_second_click_restores_it(
        cx: &mut TestAppContext,
    ) {
        let (_store, _request_id, view, mut cx) = build_request_view(cx).await;
        view.update_in(&mut cx, |view, window, cx| {
            view.apply_response(fake_response(200, "{}"), window, cx);
        });
        cx.run_until_parked();
        draw(&mut cx);

        assert!(
            cx.debug_bounds("request-send").is_some(),
            "the request editor (Send button) must be visible before entering fullscreen"
        );

        let fullscreen_button = debug_center(&mut cx, "request-response-fullscreen");
        cx.simulate_click(fullscreen_button, gpui::Modifiers::none());
        cx.run_until_parked();
        draw(&mut cx);

        view.read_with(&cx, |view, _| {
            assert!(
                view.response_fullscreen,
                "clicking the fullscreen toggle must set response_fullscreen"
            );
        });
        assert!(
            cx.debug_bounds("request-send").is_none(),
            "the request editor must be hidden while the response is fullscreen"
        );

        let fullscreen_button = debug_center(&mut cx, "request-response-fullscreen");
        cx.simulate_click(fullscreen_button, gpui::Modifiers::none());
        cx.run_until_parked();
        draw(&mut cx);

        view.read_with(&cx, |view, _| {
            assert!(
                !view.response_fullscreen,
                "a second click must exit fullscreen"
            );
        });
        assert!(
            cx.debug_bounds("request-send").is_some(),
            "the request editor must reappear after exiting fullscreen"
        );
    }

    #[gpui::test]
    async fn picking_a_raw_body_content_type_auto_sets_the_content_type_header(
        cx: &mut TestAppContext,
    ) {
        let (_store, _request_id, view, mut cx) = build_request_view(cx).await;
        view.update(&mut cx, |view, cx| {
            view.set_body_kind(BodyKind::Raw, cx);
            view.active_tab = RequestTab::Body;
        });
        draw(&mut cx);

        let json_chip = debug_center(&mut cx, "content-type-chip-JSON");
        cx.simulate_click(json_chip, gpui::Modifiers::none());
        cx.run_until_parked();

        view.read_with(&cx, |view, cx| {
            let header = view
                .header_rows
                .iter()
                .find(|row| row.key_editor.read(cx).text(cx) == "Content-Type")
                .expect("the JSON chip must add a Content-Type header");
            assert_eq!(header.value_editor.read(cx).text(cx), "application/json");
        });

        let xml_chip = debug_center(&mut cx, "content-type-chip-XML");
        cx.simulate_click(xml_chip, gpui::Modifiers::none());
        cx.run_until_parked();

        view.read_with(&cx, |view, cx| {
            let content_type_rows: Vec<_> = view
                .header_rows
                .iter()
                .filter(|row| row.key_editor.read(cx).text(cx) == "Content-Type")
                .collect();
            assert_eq!(
                content_type_rows.len(),
                1,
                "switching content type must update the existing header, not add a duplicate"
            );
            assert_eq!(
                content_type_rows[0].value_editor.read(cx).text(cx),
                "application/xml"
            );
        });
    }

    #[gpui::test]
    async fn clicking_format_pretty_prints_a_minified_json_body(cx: &mut TestAppContext) {
        let (_store, _request_id, view, mut cx) = build_request_view(cx).await;
        view.update_in(&mut cx, |view, window, cx| {
            view.set_body_kind(BodyKind::Raw, cx);
            view.set_body_content_type(RawBodyContentType::Json, window, cx);
            view.body_editor.update(cx, |editor, cx| {
                editor.set_text(r#"{"a":1,"b":[2,3]}"#, window, cx);
            });
            view.active_tab = RequestTab::Body;
        });
        draw(&mut cx);

        let format_button = debug_center(&mut cx, "request-format-body");
        cx.simulate_click(format_button, gpui::Modifiers::none());
        cx.run_until_parked();

        view.read_with(&cx, |view, cx| {
            let text = view.body_editor.read(cx).text(cx);
            assert_eq!(text, "{\n  \"a\": 1,\n  \"b\": [\n    2,\n    3\n  ]\n}");
        });
    }

    #[gpui::test]
    async fn unchecking_an_auto_header_removes_it_from_enabled_auto_headers_and_persists(
        cx: &mut TestAppContext,
    ) {
        let (store, request_id, view, mut cx) = build_request_view(cx).await;
        view.update_in(&mut cx, |view, _window, cx| {
            view.active_tab = RequestTab::Headers;
            cx.notify();
        });
        draw(&mut cx);

        view.read_with(&cx, |view, _| {
            assert!(
                view.enabled_auto_headers()
                    .iter()
                    .any(|(key, _)| key == "User-Agent"),
                "User-Agent is enabled by default"
            );
        });

        let toggle = debug_center(&mut cx, "auto-header-toggle-User-Agent");
        cx.simulate_click(toggle, gpui::Modifiers::none());
        cx.run_until_parked();

        view.read_with(&cx, |view, _| {
            assert!(
                !view
                    .enabled_auto_headers()
                    .iter()
                    .any(|(key, _)| key == "User-Agent"),
                "unchecking the row must drop it from the set `build_resolved_request` layers onto the request"
            );
        });
        store.read_with(&cx, |store, _| {
            let request = store.requests.iter().find(|r| r.id == request_id).unwrap();
            assert_eq!(
                request.settings.disabled_auto_headers,
                vec!["User-Agent".to_string()],
                "the toggle must persist to the store so it survives across app restarts, \
                 the same way header_rows does"
            );
        });
    }

    #[test]
    fn parse_bulk_key_value_text_parses_enabled_and_disabled_rows_and_drops_pure_comments() {
        let rows = parse_bulk_key_value_text(
            "Accept: application/json\n\
             //Authorization: Bearer old-token\n\
             // just a note to self, not a header\n\
             \n\
             X-Custom:no-space-around-colon",
        );
        assert_eq!(
            rows,
            vec![
                ("Accept".to_string(), "application/json".to_string(), true),
                (
                    "Authorization".to_string(),
                    "Bearer old-token".to_string(),
                    false
                ),
                (
                    "X-Custom".to_string(),
                    "no-space-around-colon".to_string(),
                    true
                ),
            ],
            "the pure-comment line and the blank line must both be dropped"
        );
    }

    #[test]
    fn a_key_that_literally_starts_with_a_disable_marker_is_escaped_and_round_trips() {
        // Without the `\`-escape a key of `//page` would render as
        // `//page: 1`, which then parses back as a *disabled* `page` row --
        // silently corrupting the key. The escape must prevent that.
        let text = "\\//page: 1";
        assert_eq!(
            parse_bulk_key_value_text(text),
            vec![("//page".to_string(), "1".to_string(), true)]
        );
    }

    #[gpui::test]
    async fn params_round_trip_through_bulk_edit_via_the_real_rows_and_editors(
        cx: &mut TestAppContext,
    ) {
        let (store, request_id, view, mut cx) = build_request_view(cx).await;
        view.update_in(&mut cx, |view, window, cx| {
            view.add_param_row(window, cx);
            view.add_param_row(window, cx);
            view.add_param_row(window, cx);
        });
        view.update_in(&mut cx, |view, window, cx| {
            let seed = [
                ("Accept", "application/json", true),
                ("Authorization", "Bearer old-token", false),
                ("//page", "1", true),
            ];
            for (row, (key, value, enabled)) in view.param_rows.iter_mut().zip(seed) {
                row.key_editor
                    .update(cx, |editor, cx| editor.set_text(key, window, cx));
                row.value_editor
                    .update(cx, |editor, cx| editor.set_text(value, window, cx));
                row.enabled = enabled;
            }
            view.persist_params(cx);
        });
        draw(&mut cx);

        // Round-trip through the real toggle handlers -- into bulk mode
        // (seeding the textarea from the rows) and back out (re-parsing the
        // textarea into fresh rows) -- rather than calling the parse/render
        // free functions directly, so a mismatch between the two is caught.
        view.update_in(&mut cx, |view, window, cx| {
            view.toggle_params_bulk_edit(window, cx);
        });
        view.update_in(&mut cx, |view, window, cx| {
            view.toggle_params_bulk_edit(window, cx);
        });
        cx.run_until_parked();

        let rows = {
            let param_rows = view.read_with(&cx, |view, _| {
                view.param_rows.iter().map(clone_row).collect::<Vec<_>>()
            });
            rows_that_say_something(&param_rows, &mut cx)
        };
        {
            assert_eq!(
                rows,
                vec![
                    ("Accept".to_string(), "application/json".to_string(), true),
                    (
                        "Authorization".to_string(),
                        "Bearer old-token".to_string(),
                        false
                    ),
                    ("//page".to_string(), "1".to_string(), true),
                ],
                "key, value, and enabled state must all survive a round trip through Bulk Edit, \
                 including a key that itself starts with the disabled-row marker"
            );
        }
        store.read_with(&cx, |store, _| {
            let request = store.requests.iter().find(|r| r.id == request_id).unwrap();
            assert_eq!(request.params.len(), 3);
        });
    }

    #[gpui::test]
    async fn entering_bulk_edit_on_headers_seeds_the_textarea_from_the_current_rows(
        cx: &mut TestAppContext,
    ) {
        let (_store, _request_id, view, mut cx) = build_request_view(cx).await;
        view.update_in(&mut cx, |view, window, cx| {
            view.active_tab = RequestTab::Headers;
            view.add_header_row(window, cx);
        });
        view.update_in(&mut cx, |view, window, cx| {
            let key_editor = view.header_rows[0].key_editor.clone();
            let value_editor = view.header_rows[0].value_editor.clone();
            key_editor.update(cx, |editor, cx| editor.set_text("Accept", window, cx));
            value_editor.update(cx, |editor, cx| {
                editor.set_text("application/json", window, cx)
            });
        });
        draw(&mut cx);

        let toggle = debug_center(&mut cx, "headers-bulk-edit-toggle");
        cx.simulate_click(toggle, gpui::Modifiers::none());
        cx.run_until_parked();

        view.read_with(&cx, |view, cx| {
            assert!(view.headers_bulk_edit);
            assert_eq!(
                view.header_bulk_editor.read(cx).text(cx),
                "Accept: application/json"
            );
        });
    }

    #[gpui::test]
    async fn typing_a_disabled_row_in_headers_bulk_edit_and_leaving_it_persists_a_disabled_header(
        cx: &mut TestAppContext,
    ) {
        let (store, request_id, view, mut cx) = build_request_view(cx).await;
        view.update_in(&mut cx, |view, window, cx| {
            view.active_tab = RequestTab::Headers;
            view.toggle_headers_bulk_edit(window, cx);
        });
        draw(&mut cx);

        view.update_in(&mut cx, |view, window, cx| {
            view.header_bulk_editor.update(cx, |editor, cx| {
                editor.set_text(
                    "Accept: application/json\n//Authorization: Bearer old-token\n// just a note",
                    window,
                    cx,
                );
            });
        });
        cx.run_until_parked();

        // The bulk text already commits to the store on every keystroke,
        // before the user ever switches back to the row view.
        store.read_with(&cx, |store, _| {
            let request = store.requests.iter().find(|r| r.id == request_id).unwrap();
            let headers: Vec<(String, String, bool)> = request
                .headers
                .iter()
                .map(|header| (header.key.clone(), header.value.clone(), header.enabled))
                .collect();
            assert_eq!(
                headers,
                vec![
                    ("Accept".to_string(), "application/json".to_string(), true),
                    (
                        "Authorization".to_string(),
                        "Bearer old-token".to_string(),
                        false
                    ),
                ],
                "the pure-comment line must not become a header, and the `//`-prefixed row must persist disabled"
            );
        });

        view.update_in(&mut cx, |view, window, cx| {
            view.toggle_headers_bulk_edit(window, cx);
        });
        draw(&mut cx);

        view.read_with(&cx, |view, cx| {
            assert_eq!(
                view.header_rows.len(),
                3,
                "the two rows the text described, and the blank one to type the next into"
            );
            assert!(
                view.header_rows[2].is_blank(cx),
                "the last one is the blank"
            );
            assert!(view.header_rows[0].enabled);
            assert_eq!(view.header_rows[0].key_editor.read(cx).text(cx), "Accept");
            assert!(
                !view.header_rows[1].enabled,
                "switching back to the row view must restore the disabled row's checkbox state"
            );
            assert_eq!(
                view.header_rows[1].key_editor.read(cx).text(cx),
                "Authorization"
            );
        });
    }

    #[gpui::test]
    async fn clicking_the_response_headers_tab_switches_away_from_the_body_editor(
        cx: &mut TestAppContext,
    ) {
        let (_store, _request_id, view, mut cx) = build_request_view(cx).await;
        view.update_in(&mut cx, |view, window, cx| {
            view.apply_response(fake_response(200, r#"{"ok":true}"#), window, cx);
        });
        draw(&mut cx);

        let headers_chip = debug_center(&mut cx, "response-tab-chip-Headers");
        cx.simulate_click(headers_chip, gpui::Modifiers::none());
        cx.run_until_parked();

        view.read_with(&cx, |view, _| {
            assert_eq!(view.response_tab, ResponseTab::Headers);
        });
    }

    fn fake_html_response(body: &str) -> ResponseData {
        ResponseData::from_summary(api_client::HttpResponseSummary {
            status: 200,
            status_text: "OK".to_string(),
            headers: vec![(
                "Content-Type".to_string(),
                "text/html; charset=utf-8".to_string(),
            )],
            body: body.as_bytes().to_vec(),
            elapsed_ms: 10,
            timings: api_client::Timings::default(),
        })
    }

    #[gpui::test]
    async fn an_html_response_offers_a_preview_tab_that_strips_tags_to_readable_text(
        cx: &mut TestAppContext,
    ) {
        let (_store, _request_id, view, mut cx) = build_request_view(cx).await;
        view.update_in(&mut cx, |view, window, cx| {
            view.apply_response(
                fake_html_response("<html><body><script>evil()</script><p>Hello</p></body></html>"),
                window,
                cx,
            );
        });
        draw(&mut cx);

        let preview_chip = debug_center(&mut cx, "response-tab-chip-Preview");
        cx.simulate_click(preview_chip, gpui::Modifiers::none());
        cx.run_until_parked();

        view.read_with(&cx, |view, cx| {
            assert_eq!(view.response_tab, ResponseTab::Preview);
            let preview_text = view.preview_body_editor.read(cx).text(cx);
            assert_eq!(preview_text, "Hello");
        });
    }

    #[gpui::test]
    async fn a_second_response_offers_a_diff_tab_showing_the_body_change(cx: &mut TestAppContext) {
        let (_store, _request_id, view, mut cx) = build_request_view(cx).await;
        view.update_in(&mut cx, |view, window, cx| {
            view.apply_response(fake_response(200, r#"{"a":1}"#), window, cx);
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("response-tab-chip-Diff").is_none(),
            "no Diff tab before a second response exists"
        );

        view.update_in(&mut cx, |view, window, cx| {
            view.apply_response(fake_response(200, r#"{"a":2}"#), window, cx);
        });
        draw(&mut cx);

        let diff_chip = debug_center(&mut cx, "response-tab-chip-Diff");
        cx.simulate_click(diff_chip, gpui::Modifiers::none());
        cx.run_until_parked();

        view.read_with(&cx, |view, cx| {
            assert_eq!(view.response_tab, ResponseTab::Diff);
            let diff_text = view.diff_body_editor.read(cx).text(cx);
            assert!(diff_text.contains('1'));
            assert!(diff_text.contains('2'));
        });
    }

    #[gpui::test]
    async fn a_json_response_does_not_offer_a_preview_tab(cx: &mut TestAppContext) {
        let (_store, _request_id, view, mut cx) = build_request_view(cx).await;
        view.update_in(&mut cx, |view, window, cx| {
            view.apply_response(fake_response(200, "{}"), window, cx);
        });
        draw(&mut cx);

        assert!(cx.debug_bounds("response-tab-chip-Preview").is_none());
    }

    #[gpui::test]
    async fn cookies_from_set_cookie_headers_are_available_to_the_cookies_tab(
        cx: &mut TestAppContext,
    ) {
        let (_store, _request_id, view, mut cx) = build_request_view(cx).await;
        view.update_in(&mut cx, |view, window, cx| {
            view.apply_response(fake_response(200, "{}"), window, cx);
        });

        view.read_with(&cx, |view, _| match &view.send_state {
            SendState::Success(response) => {
                assert_eq!(response.cookies.len(), 1);
                assert_eq!(response.cookies[0].name, "session");
                assert_eq!(response.cookies[0].value, "abc123");
            }
            other => panic!(
                "expected Success, got a different send state ({other:?})",
                other = std::mem::discriminant(other)
            ),
        });
    }

    #[gpui::test]
    fn sending_a_request_records_a_history_entry_with_status_and_evicts_the_oldest_past_the_cap(
        cx: &mut TestAppContext,
    ) {
        let store = cx.new(|cx| ApiClientStore::new(cx));
        let collection_id = store.update(cx, |store, cx| store.create_collection("A".into(), cx));
        let request_id = store.update(cx, |store, cx| {
            store.create_request(collection_id, "Ping".into(), None, cx)
        });

        store.update(cx, |store, cx| {
            store.record_history_entry(
                HistoryEntry::new(
                    request_id,
                    "GET".into(),
                    "https://api.example.com/ping".into(),
                    Some(204),
                    1_700_000_000_000,
                ),
                cx,
            );
        });
        store.read_with(cx, |store, _| {
            assert_eq!(store.history.len(), 1);
            assert_eq!(store.history[0].status, Some(204));
            assert_eq!(store.history[0].method, "GET");
        });

        store.update(cx, |store, cx| {
            for _ in 0..600 {
                store.record_history_entry(
                    HistoryEntry::new(
                        request_id,
                        "GET".into(),
                        "https://api.example.com/ping".into(),
                        Some(200),
                        1_700_000_000_001,
                    ),
                    cx,
                );
            }
        });
        store.read_with(cx, |store, _| {
            assert_eq!(
                store.history.len(),
                500,
                "history must be capped at MAX_HISTORY_ENTRIES"
            );
        });
    }

    #[gpui::test]
    async fn typing_into_the_scripts_tab_editors_persists_both_scripts(cx: &mut TestAppContext) {
        let (store, request_id, view, mut cx) = build_request_view(cx).await;
        draw(&mut cx);

        let scripts_tab = debug_center(&mut cx, "request-chip-Scripts");
        cx.simulate_click(scripts_tab, gpui::Modifiers::none());
        cx.run_until_parked();

        let pre_request_editor =
            view.read_with(&cx, |view, _| view.pre_request_script_editor.clone());
        let test_editor = view.read_with(&cx, |view, _| view.test_script_editor.clone());
        view.update_in(&mut cx, |_, window, cx| {
            pre_request_editor.update(cx, |editor, cx| editor.focus_handle(cx).focus(window, cx));
        });
        cx.simulate_input("pm.environment.set('a', '1');");
        cx.run_until_parked();
        view.update_in(&mut cx, |_, window, cx| {
            test_editor.update(cx, |editor, cx| editor.focus_handle(cx).focus(window, cx));
        });
        cx.simulate_input("pm.test('ok', () => {});");
        cx.run_until_parked();

        store.read_with(&cx, |store, _| {
            let request = store.requests.iter().find(|r| r.id == request_id).unwrap();
            assert_eq!(request.pre_request_script, "pm.environment.set('a', '1');");
            assert_eq!(request.test_script, "pm.test('ok', () => {});");
        });
    }

    #[gpui::test]
    async fn test_results_render_pass_and_fail_rows_and_offer_a_tab_only_when_present(
        cx: &mut TestAppContext,
    ) {
        let (_store, _request_id, view, mut cx) = build_request_view(cx).await;
        view.update_in(&mut cx, |view, window, cx| {
            view.apply_response(fake_response(200, "{}"), window, cx);
        });
        draw(&mut cx);
        assert!(
            cx.debug_bounds("response-tab-chip-Test Results").is_none(),
            "no Test Results tab before any test has run"
        );

        view.update(&mut cx, |view, cx| {
            view.test_results = vec![
                api_client::TestResult {
                    name: "passes".to_string(),
                    passed: true,
                    error: None,
                },
                api_client::TestResult {
                    name: "fails".to_string(),
                    passed: false,
                    error: Some("expected 1 to equal 2".to_string()),
                },
            ];
            cx.notify();
        });
        draw(&mut cx);

        let tab = debug_center(&mut cx, "response-tab-chip-Test Results");
        cx.simulate_click(tab, gpui::Modifiers::none());
        cx.run_until_parked();

        view.read_with(&cx, |view, _| {
            assert_eq!(view.response_tab, ResponseTab::TestResults)
        });
    }

    #[gpui::test]
    async fn visualize_data_renders_as_formatted_json_and_offers_a_tab_only_when_present(
        cx: &mut TestAppContext,
    ) {
        let (_store, _request_id, view, mut cx) = build_request_view(cx).await;
        view.update_in(&mut cx, |view, window, cx| {
            view.apply_response(fake_response(200, "{}"), window, cx);
        });
        draw(&mut cx);
        assert!(
            cx.debug_bounds("response-tab-chip-Visualize").is_none(),
            "no Visualize tab before pm.visualize() has run"
        );

        view.update(&mut cx, |view, cx| {
            view.visualize_data = Some(serde_json::json!({ "total": 3 }));
            cx.notify();
        });
        draw(&mut cx);

        let tab = debug_center(&mut cx, "response-tab-chip-Visualize");
        cx.simulate_click(tab, gpui::Modifiers::none());
        cx.run_until_parked();

        view.read_with(&cx, |view, _| {
            assert_eq!(view.response_tab, ResponseTab::Visualize)
        });
    }

    #[gpui::test]
    async fn a_fresh_response_with_no_test_results_switches_away_from_the_test_results_tab(
        cx: &mut TestAppContext,
    ) {
        let (_store, _request_id, view, mut cx) = build_request_view(cx).await;
        view.update_in(&mut cx, |view, window, cx| {
            view.apply_response(fake_response(200, "{}"), window, cx);
        });
        view.update(&mut cx, |view, cx| {
            view.test_results = vec![api_client::TestResult {
                name: "passes".to_string(),
                passed: true,
                error: None,
            }];
            view.response_tab = ResponseTab::TestResults;
            cx.notify();
        });

        view.update_in(&mut cx, |view, window, cx| {
            view.test_results.clear();
            view.apply_response(fake_response(200, "{}"), window, cx);
        });

        view.read_with(&cx, |view, _| {
            assert_eq!(view.response_tab, ResponseTab::Pretty)
        });
    }

    #[gpui::test]
    async fn clicking_save_as_example_through_the_real_modal_persists_a_saved_example(
        cx: &mut TestAppContext,
    ) {
        let (store, request_id, view, mut cx) = build_request_view(cx).await;
        store.update(&mut cx, |store, cx| {
            store.update_request(request_id, cx, |request| {
                request.url = "https://api.example.com/ping".to_string();
            });
        });
        view.update_in(&mut cx, |view, window, cx| {
            view.apply_response(fake_response(200, r#"{"ok":true}"#), window, cx);
        });
        draw(&mut cx);

        let save_button = debug_center(&mut cx, "request-save-example");
        cx.simulate_click(save_button, gpui::Modifiers::none());
        cx.run_until_parked();

        let workspace = view
            .read_with(&cx, |view, _| view.workspace.clone())
            .upgrade()
            .expect("the workspace should still be alive");
        let modal = workspace
            .read_with(&cx, |workspace, cx| {
                workspace.active_modal::<TextPromptModal>(cx)
            })
            .expect("Save as Example should open the name-prompt modal");
        modal.update_in(&mut cx, |modal, window, cx| {
            modal.editor.update(cx, |editor, cx| {
                editor.set_text("Ping OK", window, cx);
            });
        });
        modal.update_in(&mut cx, |modal, window, cx| modal.confirm(window, cx));
        cx.run_until_parked();

        store.read_with(&cx, |store, _| {
            let request = store.requests.iter().find(|r| r.id == request_id).unwrap();
            assert_eq!(request.examples.len(), 1);
            let example = &request.examples[0];
            assert_eq!(example.name, "Ping OK");
            assert_eq!(example.response_status, 200);
            assert_eq!(example.response_body, r#"{"ok":true}"#);
            assert_eq!(example.request_url, "https://api.example.com/ping");
        });
    }

    #[gpui::test]
    async fn the_examples_tab_lists_saved_examples_and_deleting_one_removes_it_from_the_store(
        cx: &mut TestAppContext,
    ) {
        let (store, request_id, _view, mut cx) = build_request_view(cx).await;
        store.update(&mut cx, |store, cx| {
            store.update_request(request_id, cx, |request| {
                request.examples.push(api_client::SavedExample::new(
                    "200 OK".to_string(),
                    HttpMethod::Get,
                    "https://api.example.com/ping".to_string(),
                    Vec::new(),
                    String::new(),
                    200,
                    Vec::new(),
                    "{}".to_string(),
                ));
            });
        });
        draw(&mut cx);

        let examples_tab = debug_center(&mut cx, "request-chip-Examples");
        cx.simulate_click(examples_tab, gpui::Modifiers::none());
        cx.run_until_parked();
        draw(&mut cx);

        let example_id = store.read_with(&cx, |store, _| {
            store
                .requests
                .iter()
                .find(|r| r.id == request_id)
                .unwrap()
                .examples[0]
                .id
        });
        let selector: &'static str =
            Box::leak(format!("example-delete-{example_id}").into_boxed_str());
        let delete_button = debug_center(&mut cx, selector);
        cx.simulate_click(delete_button, gpui::Modifiers::none());
        cx.run_until_parked();

        store.read_with(&cx, |store, _| {
            let request = store.requests.iter().find(|r| r.id == request_id).unwrap();
            assert!(request.examples.is_empty());
        });
    }

    fn sample_response_data(status: u16) -> ResponseData {
        ResponseData {
            status,
            status_text: "status".into(),
            elapsed_ms: 3,
            size_bytes: 2,
            headers: Vec::new(),
            body: b"{}".to_vec(),
            cookies: Vec::new(),
            timings: api_client::Timings::default(),
        }
    }

    /// A response must never vanish just because no dock happens to be
    /// registered on the workspace -- `RequestView` falls back to keeping
    /// (and rendering) its own copy, exactly as it always has.
    #[gpui::test]
    async fn a_response_falls_back_to_the_request_view_when_no_dock_is_registered(
        cx: &mut TestAppContext,
    ) {
        let (_store, _workspace, view, mut cx) = build_request_view_in_workspace(cx).await;

        view.update_in(&mut cx, |view, window, cx| {
            view.apply_response(sample_response_data(200), window, cx);
        });
        cx.run_until_parked();

        view.read_with(&cx, |view, cx| {
            assert!(
                !view.response_shown_in_dock(cx),
                "no dock is registered, so the response must stay local"
            );
            assert!(
                matches!(&view.send_state, SendState::Success(response) if response.status == 200)
            );
        });
    }

    /// A reply opens a tab in the terminal panel's pane -- where the terminals
    /// and the database results already are -- and shows itself there rather than
    /// in the request view's own inline section.
    #[gpui::test]
    async fn a_response_opens_a_tab_next_to_the_terminals(cx: &mut TestAppContext) {
        let (_store, workspace, view, mut cx) = build_request_view_in_workspace(cx).await;

        let terminal_panel = workspace.update_in(&mut cx, |workspace, window, cx| {
            let panel = cx.new(|cx| TerminalPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });
        cx.run_until_parked();

        view.update_in(&mut cx, |view, window, cx| {
            view.apply_response(sample_response_data(201), window, cx);
        });
        cx.run_until_parked();

        let response_tabs = terminal_panel.read_with(&cx, |panel, cx| {
            panel
                .pane()
                .map(|pane| pane.read(cx).items_of_type::<ResponseDockPanel>().count())
                .unwrap_or(0)
        });
        assert_eq!(
            response_tabs, 1,
            "the reply has to open exactly one tab in the terminal panel's pane"
        );
        view.read_with(&cx, |view, cx| {
            assert!(
                view.response_shown_in_dock(cx),
                "that tab is what shows the response, so the inline copy steps aside"
            );
        });
        workspace.read_with(&cx, |workspace, cx| {
            assert!(
                workspace.bottom_dock().read(cx).is_open(),
                "the dock has to be revealed so the reply is actually visible"
            );
        });

        // A second reply belongs in the same tab: one tab for every request.
        view.update_in(&mut cx, |view, window, cx| {
            view.apply_response(sample_response_data(500), window, cx);
        });
        cx.run_until_parked();
        let response_tabs_after = terminal_panel.read_with(&cx, |panel, cx| {
            panel
                .pane()
                .map(|pane| pane.read(cx).items_of_type::<ResponseDockPanel>().count())
                .unwrap_or(0)
        });
        assert_eq!(
            response_tabs_after, 1,
            "a later reply replaces what the tab shows instead of stacking another tab"
        );
    }

    /// The reader can close the response tab while a request is still in flight.
    /// The reply then has to open a tab and show itself there, rather than being
    /// dismissed as belonging to a send the fresh tab knows nothing about.
    #[gpui::test]
    async fn a_reply_after_the_tab_was_closed_shows_in_a_fresh_tab(cx: &mut TestAppContext) {
        let (_store, _workspace_handle, view, mut cx) = {
            let (store, workspace, view, mut cx) = build_request_view_in_workspace(cx).await;
            workspace.update_in(&mut cx, |workspace, window, cx| {
                let panel = cx.new(|cx| TerminalPanel::new(workspace, window, cx));
                workspace.add_panel(panel, window, cx);
            });
            cx.run_until_parked();
            (store, workspace, view, cx)
        };

        view.update_in(&mut cx, |view, window, cx| {
            view.apply_response(sample_response_data(200), window, cx);
        });
        cx.run_until_parked();

        let pane = view
            .read_with(&cx, |view, cx| {
                view.workspace
                    .upgrade()
                    .and_then(|workspace| workspace.read(cx).panel::<TerminalPanel>(cx))
                    .and_then(|panel| panel.read(cx).pane())
            })
            .expect("the terminal panel has a pane");
        let tab_id = pane
            .read_with(&cx, |pane, _| {
                pane.items_of_type::<ResponseDockPanel>()
                    .next()
                    .map(|tab| tab.item_id())
            })
            .expect("the first reply opened a tab");
        pane.update_in(&mut cx, |pane, window, cx| {
            pane.remove_item(tab_id, false, false, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            pane.read_with(&cx, |pane, _| pane
                .items_of_type::<ResponseDockPanel>()
                .count()),
            0,
            "the tab has to be gone before the next reply arrives"
        );

        view.update_in(&mut cx, |view, window, cx| {
            view.apply_response(sample_response_data(204), window, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            pane.read_with(&cx, |pane, _| pane
                .items_of_type::<ResponseDockPanel>()
                .count()),
            1,
            "the reply has to open a tab again"
        );
        view.read_with(&cx, |view, cx| {
            assert!(
                view.response_shown_in_dock(cx),
                "the fresh tab has to be the one showing the reply"
            );
        });
    }

    /// With no terminal panel there is no pane to put the tab in, so the reply
    /// stays in the request view rather than disappearing.
    #[gpui::test]
    async fn without_a_terminal_panel_the_reply_stays_in_the_request_view(cx: &mut TestAppContext) {
        let (_store, _workspace, view, mut cx) = build_request_view_in_workspace(cx).await;

        view.update_in(&mut cx, |view, window, cx| {
            view.apply_response(sample_response_data(201), window, cx);
        });
        cx.run_until_parked();

        view.read_with(&cx, |view, cx| {
            assert!(
                !view.response_shown_in_dock(cx),
                "with nowhere to open a tab, the response has to stay on screen here"
            );
            assert!(
                matches!(&view.send_state, SendState::Success(response) if response.status == 201),
                "the reply itself must not be lost"
            );
        });
    }
}
