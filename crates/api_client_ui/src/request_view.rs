use crate::response_view::{ResponseData, ResponseTab, SendState};
use crate::store::ApiClientStore;
use crate::text_prompt_modal::TextPromptModal;
use api_client::{
    ApiKeyPlacement, AuthConfig, AwsSigV4Config, EnvironmentId, Header, HistoryEntry, HttpMethod,
    JwtAlgorithm, JwtAuthConfig, OAuth2Config, OAuth2GrantType, QueryParam, RawBodyContentType,
    Request, RequestBody, RequestId, ResolveMode, SavedExample, SystemDynamicVariableSource,
};
use editor::{Editor, EditorEvent, HighlightKey};
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, HighlightStyle, Render,
    ScrollHandle, Subscription, WeakEntity, Window,
};
use std::sync::Arc;
use ui::{
    ContextMenu, Icon, IconName, IconSize, Label, LabelSize, ScrollAxes, Scrollbars, Tooltip,
    WithScrollbar, prelude::*,
};
use util::ResultExt;
use workspace::{Item, Workspace, item::ItemEvent};

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

fn content_type_header_value(content_type: RawBodyContentType) -> &'static str {
    match content_type {
        RawBodyContentType::Json => "application/json",
        RawBodyContentType::Xml => "application/xml",
        RawBodyContentType::Html => "text/html",
        RawBodyContentType::JavaScript => "application/javascript",
        RawBodyContentType::Text => "text/plain",
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
    enabled: bool,
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
    response_tab: ResponseTab,
    pretty_body_editor: Entity<Editor>,
    raw_body_editor: Entity<Editor>,
    preview_body_editor: Entity<Editor>,
    response_is_html: bool,
    previous_response: Option<ResponseData>,
    diff_body_editor: Entity<Editor>,
    pre_request_script_editor: Entity<Editor>,
    test_script_editor: Entity<Editor>,
    test_results: Vec<api_client::TestResult>,
    visualize_data: Option<serde_json::Value>,
    scroll_handle: ScrollHandle,
    environment_pin_handle: ui::PopoverMenuHandle<ContextMenu>,
    response_fullscreen: bool,
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
            response_tab: ResponseTab::Pretty,
            pretty_body_editor,
            raw_body_editor,
            preview_body_editor,
            response_is_html: false,
            previous_response: None,
            diff_body_editor,
            pre_request_script_editor,
            test_script_editor,
            test_results: Vec::new(),
            visualize_data: None,
            scroll_handle: ScrollHandle::new(),
            environment_pin_handle: ui::PopoverMenuHandle::default(),
            response_fullscreen: false,
            _subscriptions: Vec::new(),
        };

        for param in &request.params {
            this.push_param_row(
                param.key.clone(),
                param.value.clone(),
                param.enabled,
                window,
                cx,
            );
        }
        for header in &request.headers {
            this.push_header_row(
                header.key.clone(),
                header.value.clone(),
                header.enabled,
                window,
                cx,
            );
        }

        this.watch_editor(this.url_editor.clone(), window, cx, |this, editor, cx| {
            let url = editor.read(cx).text(cx);
            let request_id = this.request_id;
            this.store.update(cx, |store, cx| {
                store.update_request(request_id, cx, |request| request.url = url);
            });
        });
        this.watch_editor(this.body_editor.clone(), window, cx, |this, _, cx| {
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
        enabled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key_editor = new_single_line_editor("Key", &key, window, cx);
        let value_editor = new_single_line_editor("Value", &value, window, cx);
        self.watch_editor(key_editor.clone(), window, cx, |this, _, cx| {
            this.persist_params(cx);
        });
        self.watch_editor(value_editor.clone(), window, cx, |this, _, cx| {
            this.persist_params(cx);
        });
        self.param_rows.push(KeyValueRow {
            key_editor,
            value_editor,
            enabled,
        });
    }

    fn push_header_row(
        &mut self,
        key: String,
        value: String,
        enabled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key_editor = new_single_line_editor("Header", &key, window, cx);
        let value_editor = new_single_line_editor("Value", &value, window, cx);
        self.watch_editor(key_editor.clone(), window, cx, |this, _, cx| {
            this.persist_headers(cx);
        });
        self.watch_editor(value_editor.clone(), window, cx, |this, _, cx| {
            this.persist_headers(cx);
        });
        self.header_rows.push(KeyValueRow {
            key_editor,
            value_editor,
            enabled,
        });
    }

    fn add_param_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.push_param_row(String::new(), String::new(), true, window, cx);
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

    fn add_header_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.push_header_row(String::new(), String::new(), true, window, cx);
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

    fn persist_params(&self, cx: &mut Context<Self>) {
        let params: Vec<QueryParam> = self
            .param_rows
            .iter()
            .map(|row| QueryParam {
                key: row.key_editor.read(cx).text(cx),
                value: row.value_editor.read(cx).text(cx),
                enabled: row.enabled,
                description: None,
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
            .map(|row| Header {
                key: row.key_editor.read(cx).text(cx),
                value: row.value_editor.read(cx).text(cx),
                enabled: row.enabled,
                description: None,
            })
            .collect();
        let request_id = self.request_id;
        self.store.update(cx, |store, cx| {
            store.update_request(request_id, cx, |request| request.headers = headers);
        });
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
        cx.notify();
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
                self.push_header_row("Content-Type".into(), value.into(), true, window, cx);
            }
        }
        self.persist_headers(cx);
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

    fn copy_as_curl(&self, cx: &mut Context<Self>) {
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
        let store = self.store.read(cx);
        let context = store.variable_context_for(&request);
        let curl = crate::code_generator::generate_curl(&request, &context);
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(curl));
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
        let client = self.store.read(cx).http_client.clone();

        self.send_state = SendState::Sending;
        self.test_results.clear();
        self.visualize_data = None;
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
                        this.update(cx, |this, cx| {
                            this.send_state = SendState::Error(message);
                            cx.notify();
                        })
                        .ok();
                        return;
                    }
                }
            }

            let resolved = store.update(cx, |store, _| {
                let context = store.variable_context_for(&request);
                let dynamic = SystemDynamicVariableSource;
                let resolve = |text: &str| {
                    api_client::resolve(text, &context, &dynamic, ResolveMode::ForSend)
                };
                api_client::build_resolved_request(&request, &resolve)
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
                        store.record_history_entry(
                            HistoryEntry::new(
                                request_id,
                                resolved.method.clone(),
                                resolved.url.clone(),
                                Some(status),
                                sent_at_unix_ms,
                            ),
                            cx,
                        );
                    });
                }
                Err(error) => {
                    let message = error.to_string();
                    this.update(cx, |this, cx| {
                        this.send_state = SendState::Error(message);
                        cx.notify();
                    })
                    .ok();
                    store.update(cx, |store, cx| {
                        store.record_history_entry(
                            HistoryEntry::new(
                                request_id,
                                resolved.method.clone(),
                                resolved.url.clone(),
                                None,
                                sent_at_unix_ms,
                            ),
                            cx,
                        );
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
        self.send_state = SendState::Success(response);
        cx.notify();
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

    /// The row of selectable method chips at the top of the request editor,
    /// each tinted with `method_color` so the active method is recognizable
    /// by color alone, not just by which chip is highlighted.
    fn set_pinned_environment(
        &mut self,
        environment_id: Option<EnvironmentId>,
        cx: &mut Context<Self>,
    ) {
        let request_id = self.request_id;
        self.store.update(cx, |store, cx| {
            store.set_request_pinned_environment(request_id, environment_id, cx)
        });
        cx.notify();
    }

    /// A compact control letting this request override the store's globally
    /// active environment with one it's always meant to run against --
    /// shows "Active Environment" when unpinned, or the pinned environment's
    /// name otherwise, so it's obvious at a glance which environment this
    /// specific request will actually resolve `{{tokens}}` against.
    fn render_environment_pin(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let pinned_id = store
            .requests
            .iter()
            .find(|request| request.id == self.request_id)
            .and_then(|request| request.pinned_environment_id);
        let pinned_name = pinned_id
            .and_then(|id| store.environment_by_id(id))
            .map(|environment| environment.name.clone());
        let trigger_label: SharedString = match &pinned_name {
            Some(name) => format!("Pinned: {name}").into(),
            None => "Active Environment".into(),
        };
        let environments: Vec<(EnvironmentId, String)> = store
            .environments
            .iter()
            .map(|environment| (environment.id, environment.name.clone()))
            .collect();
        let view = cx.entity();
        let popover_handle = self.environment_pin_handle.clone();

        div()
            .id("request-environment-pin")
            .debug_selector(|| "request-environment-pin".to_string())
            .child(
                ui::PopoverMenu::new("request-environment-pin-popover")
                    .with_handle(popover_handle)
                    .trigger(
                        Button::new("request-environment-pin-trigger", trigger_label)
                            .start_icon(Icon::new(IconName::Pin))
                            .style(if pinned_id.is_some() {
                                ButtonStyle::Tinted(ui::TintColor::Accent)
                            } else {
                                ButtonStyle::Subtle
                            }),
                    )
                    .menu(move |window, cx| {
                        let view = view.clone();
                        let environments = environments.clone();
                        Some(ContextMenu::build(window, cx, move |menu, _, _| {
                            let menu = menu.entry("Use Active Environment", None, {
                                let view = view.clone();
                                move |_window, cx| {
                                    view.update(cx, |view, cx| {
                                        view.set_pinned_environment(None, cx);
                                    });
                                }
                            });
                            environments.iter().fold(menu, |menu, (id, name)| {
                                let view = view.clone();
                                let id = *id;
                                menu.entry(name.clone(), None, move |_window, cx| {
                                    view.update(cx, |view, cx| {
                                        view.set_pinned_environment(Some(id), cx);
                                    });
                                })
                            })
                        }))
                    }),
            )
    }

    fn render_method_selector_chip(
        label: &'static str,
        method: HttpMethod,
        is_selected: bool,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Self, &gpui::ClickEvent, &mut Window, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        let color = Self::method_color(&method);
        let tint = color.color(cx);
        div()
            .id(SharedString::from(format!("request-method-chip-{label}")))
            .debug_selector(move || format!("request-method-chip-{label}"))
            .px_2()
            .py_0p5()
            .rounded_md()
            .cursor_pointer()
            .when(is_selected, |el| el.bg(tint.opacity(0.16)))
            .when(!is_selected, |el| {
                el.hover(|el| el.bg(cx.theme().colors().element_hover))
            })
            .child(
                Label::new(label)
                    .size(LabelSize::Small)
                    .color(if is_selected { color } else { Color::Muted })
                    .buffer_font(cx),
            )
            .on_click(cx.listener(on_click))
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

    fn render_key_value_rows(
        rows: &[KeyValueRow],
        add_label: &'static str,
        on_add: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        on_toggle: impl Fn(&mut Self, usize, &mut Context<Self>) + 'static + Clone,
        on_remove: impl Fn(&mut Self, usize, &mut Context<Self>) + 'static + Clone,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let colors = cx.theme().colors();
        let mut list = v_flex().gap_2();
        for (index, row) in rows.iter().enumerate() {
            let on_toggle = on_toggle.clone();
            let on_remove = on_remove.clone();
            list = list.child(
                h_flex()
                    .id(SharedString::from(format!("kv-row-{index}")))
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .id(SharedString::from(format!("kv-row-toggle-{index}")))
                            .cursor_pointer()
                            .child(
                                Icon::new(if row.enabled {
                                    IconName::Check
                                } else {
                                    IconName::Close
                                })
                                .size(IconSize::Small),
                            )
                            .on_click(
                                cx.listener(move |this, _, _window, cx| on_toggle(this, index, cx)),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .border_1()
                            .border_color(colors.border)
                            .bg(colors.background)
                            .child(row.key_editor.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .border_1()
                            .border_color(colors.border)
                            .bg(colors.background)
                            .child(row.value_editor.clone()),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("kv-row-remove-{index}")))
                            .cursor_pointer()
                            .child(Icon::new(IconName::Trash).size(IconSize::Small))
                            .on_click(
                                cx.listener(move |this, _, _window, cx| on_remove(this, index, cx)),
                            ),
                    ),
            );
        }
        list.child(
            div()
                .id("kv-row-add")
                .debug_selector(|| "kv-row-add".to_string())
                .cursor_pointer()
                .child(
                    Label::new(add_label)
                        .size(LabelSize::Small)
                        .color(Color::Accent),
                )
                .on_click(cx.listener(move |this, _, window, cx| on_add(this, window, cx))),
        )
    }

    fn render_tab_body(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        match self.active_tab {
            RequestTab::Params => Self::render_key_value_rows(
                &self.param_rows,
                "Add Param",
                Self::add_param_row,
                Self::toggle_param_row,
                Self::remove_param_row,
                cx,
            )
            .into_any_element(),
            RequestTab::Headers => Self::render_key_value_rows(
                &self.header_rows,
                "Add Header",
                Self::add_header_row,
                Self::toggle_header_row,
                Self::remove_header_row,
                cx,
            )
            .into_any_element(),
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
                        ));
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

    fn render_response_section(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let border = cx.theme().colors().border;
        let border_variant = cx.theme().colors().border_variant;
        let background = cx.theme().colors().background;
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
                    .justify_between()
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
                    ResponseTab::Diff => div()
                        .flex_1()
                        .min_h(px(400.))
                        .px_2()
                        .py_1p5()
                        .rounded_md()
                        .border_1()
                        .border_color(border)
                        .bg(background)
                        .child(self.diff_body_editor.clone())
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
                        column
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
                                    .child(editor),
                            )
                            .into_any_element()
                    }
                    ResponseTab::Headers => {
                        let mut list = v_flex().gap_1();
                        for (key, value) in &headers {
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
                        if cookies.is_empty() {
                            Label::new("No cookies in this response.")
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .into_any_element()
                        } else {
                            let mut list = v_flex().gap_1();
                            for cookie in &cookies {
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
                };

                v_flex()
                    .pt_3()
                    .gap_2()
                    .border_t_1()
                    .border_color(border_variant)
                    .child(summary_row)
                    .child(tab_strip)
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
        let border = cx.theme().colors().border;
        let background = cx.theme().colors().background;
        let editor_background = cx.theme().colors().editor_background;
        let method = self.method.clone();
        let is_get = matches!(method, HttpMethod::Get);
        let is_post = matches!(method, HttpMethod::Post);
        let is_put = matches!(method, HttpMethod::Put);
        let is_patch = matches!(method, HttpMethod::Patch);
        let is_delete = matches!(method, HttpMethod::Delete);
        let is_head = matches!(method, HttpMethod::Head);
        let is_options = matches!(method, HttpMethod::Options);

        let method_row = h_flex()
            .w_full()
            .gap_1()
            .child(Self::render_method_selector_chip(
                "GET",
                HttpMethod::Get,
                is_get,
                cx,
                |this, _, _, cx| this.set_method(HttpMethod::Get, cx),
            ))
            .child(Self::render_method_selector_chip(
                "POST",
                HttpMethod::Post,
                is_post,
                cx,
                |this, _, _, cx| this.set_method(HttpMethod::Post, cx),
            ))
            .child(Self::render_method_selector_chip(
                "PUT",
                HttpMethod::Put,
                is_put,
                cx,
                |this, _, _, cx| this.set_method(HttpMethod::Put, cx),
            ))
            .child(Self::render_method_selector_chip(
                "PATCH",
                HttpMethod::Patch,
                is_patch,
                cx,
                |this, _, _, cx| this.set_method(HttpMethod::Patch, cx),
            ))
            .child(Self::render_method_selector_chip(
                "DELETE",
                HttpMethod::Delete,
                is_delete,
                cx,
                |this, _, _, cx| this.set_method(HttpMethod::Delete, cx),
            ))
            .child(Self::render_method_selector_chip(
                "HEAD",
                HttpMethod::Head,
                is_head,
                cx,
                |this, _, _, cx| this.set_method(HttpMethod::Head, cx),
            ))
            .child(Self::render_method_selector_chip(
                "OPTIONS",
                HttpMethod::Options,
                is_options,
                cx,
                |this, _, _, cx| this.set_method(HttpMethod::Options, cx),
            ))
            .child({
                // Once a custom method is active, the chip shows that exact
                // method text (e.g. "PURGE") instead of the literal word
                // "Custom..." -- so the active method is always visible
                // somewhere in the row, matching every other chip here.
                let custom_label: SharedString = match &method {
                    HttpMethod::Custom(name) => name.clone().into(),
                    _ => "Custom...".into(),
                };
                let is_custom = matches!(method, HttpMethod::Custom(_));
                div()
                    .id("request-method-chip-custom")
                    .debug_selector(|| "request-method-chip-custom".to_string())
                    .px_2()
                    .py_0p5()
                    .rounded_md()
                    .cursor_pointer()
                    .when(is_custom, |el| el.bg(Color::Muted.color(cx).opacity(0.16)))
                    .when(!is_custom, |el| {
                        el.hover(|el| el.bg(cx.theme().colors().element_hover))
                    })
                    .child(
                        Label::new(custom_label)
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .buffer_font(cx),
                    )
                    .on_click(
                        cx.listener(|this, _, window, cx| this.start_custom_method(window, cx)),
                    )
            })
            .child(div().ml_auto().child(self.render_environment_pin(cx)));

        let url_row = h_flex()
            .w_full()
            .gap_2()
            .items_center()
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
                        Button::new("request-copy-curl", "Copy as cURL")
                            .style(ButtonStyle::Subtle)
                            .on_click(cx.listener(|this, _, _window, cx| this.copy_as_curl(cx))),
                    ),
            )
            .child({
                let is_sending = matches!(self.send_state, SendState::Sending);
                div()
                    .id("request-send-hitbox")
                    .debug_selector(|| "request-send".to_string())
                    .child(
                        Button::new(
                            "request-send-button",
                            if is_sending { "Sending..." } else { "Send" },
                        )
                        .style(ButtonStyle::Filled)
                        .disabled(is_sending)
                        .on_click(cx.listener(|this, _, window, cx| this.send(window, cx))),
                    )
            });

        let active_tab = self.active_tab;
        let tab_strip = h_flex()
            .gap_2()
            .child(Self::render_chip(
                "Params",
                active_tab == RequestTab::Params,
                cx,
                |this, _, _, cx| {
                    this.active_tab = RequestTab::Params;
                    cx.notify();
                },
            ))
            .child(Self::render_chip(
                "Headers",
                active_tab == RequestTab::Headers,
                cx,
                |this, _, _, cx| {
                    this.active_tab = RequestTab::Headers;
                    cx.notify();
                },
            ))
            .child(Self::render_chip(
                "Body",
                active_tab == RequestTab::Body,
                cx,
                |this, _, _, cx| {
                    this.active_tab = RequestTab::Body;
                    cx.notify();
                },
            ))
            .child(Self::render_chip(
                "Authorization",
                active_tab == RequestTab::Auth,
                cx,
                |this, _, _, cx| {
                    this.active_tab = RequestTab::Auth;
                    cx.notify();
                },
            ))
            .child(Self::render_chip(
                "Scripts",
                active_tab == RequestTab::Scripts,
                cx,
                |this, _, _, cx| {
                    this.active_tab = RequestTab::Scripts;
                    cx.notify();
                },
            ))
            .child(Self::render_chip(
                "Examples",
                active_tab == RequestTab::Examples,
                cx,
                |this, _, _, cx| {
                    this.active_tab = RequestTab::Examples;
                    cx.notify();
                },
            ));

        let response_section = self.render_response_section(cx);

        if self.response_fullscreen {
            return v_flex()
                .id("api-client-request-view")
                .key_context("ApiClientRequestView")
                .track_focus(&self.focus_handle)
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

        v_flex()
            .id("api-client-request-view")
            .key_context("ApiClientRequestView")
            .track_focus(&self.focus_handle)
            .size_full()
            .p_4()
            .gap_3()
            .bg(editor_background)
            .overflow_scroll()
            .track_scroll(&self.scroll_handle)
            .child(method_row)
            .child(url_row)
            .child(tab_strip)
            .child(div().child(tab_body))
            .child(response_section)
            .custom_scrollbars(
                Scrollbars::always_visible(ScrollAxes::Vertical)
                    .tracked_scroll_handle(&self.scroll_handle),
                window,
                cx,
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::ApiClientStore;
    use gpui::{TestAppContext, VisualTestContext};
    use project::Project;

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
        });
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

    #[gpui::test]
    async fn clicking_a_method_chip_updates_the_stored_method(cx: &mut TestAppContext) {
        let (store, request_id, view, mut cx) = build_request_view(cx).await;
        draw(&mut cx);

        let post_chip = debug_center(&mut cx, "request-method-chip-POST");
        cx.simulate_click(post_chip, gpui::Modifiers::none());
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

        let custom_chip = debug_center(&mut cx, "request-method-chip-custom");
        cx.simulate_click(custom_chip, gpui::Modifiers::none());
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

    #[gpui::test]
    async fn clicking_copy_as_curl_puts_the_generated_command_on_the_clipboard(
        cx: &mut TestAppContext,
    ) {
        let (store, request_id, _view, mut cx) = build_request_view(cx).await;
        store.update(&mut cx, |store, cx| {
            store.update_request(request_id, cx, |request| {
                request.url = "https://api.example.com/ping".to_string();
            });
        });
        draw(&mut cx);

        let copy_button = debug_center(&mut cx, "request-copy-curl");
        cx.simulate_click(copy_button, gpui::Modifiers::none());
        cx.run_until_parked();

        let clipboard_text = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .expect("Copy as cURL should place text on the clipboard");
        assert!(clipboard_text.contains("curl --request GET"));
        assert!(clipboard_text.contains("https://api.example.com/ping"));
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

    #[gpui::test]
    async fn adding_a_param_row_and_typing_persists_it_to_the_store(cx: &mut TestAppContext) {
        let (store, request_id, view, mut cx) = build_request_view(cx).await;
        draw(&mut cx);

        let add_button = debug_center(&mut cx, "kv-row-add");
        cx.simulate_click(add_button, gpui::Modifiers::none());
        cx.run_until_parked();

        let key_editor = view.read_with(&cx, |view, _| view.param_rows[0].key_editor.clone());
        let value_editor = view.read_with(&cx, |view, _| view.param_rows[0].value_editor.clone());
        view.update_in(&mut cx, |_, window, cx| {
            key_editor.update(cx, |editor, cx| editor.focus_handle(cx).focus(window, cx));
        });
        cx.simulate_input("page");
        cx.run_until_parked();
        view.update_in(&mut cx, |_, window, cx| {
            value_editor.update(cx, |editor, cx| editor.focus_handle(cx).focus(window, cx));
        });
        cx.simulate_input("1");
        cx.run_until_parked();

        store.read_with(&cx, |store, _| {
            let request = store.requests.iter().find(|r| r.id == request_id).unwrap();
            assert_eq!(request.params.len(), 1);
            assert_eq!(request.params[0].key, "page");
            assert_eq!(request.params[0].value, "1");
        });
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
}
