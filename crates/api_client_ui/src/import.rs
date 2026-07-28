use anyhow::{Context as _, Result, bail};
use api_client::{
    ApiKeyPlacement, AuthConfig, Collection, CollectionId, Environment, Folder, FolderId, Header,
    HttpMethod, JwtAlgorithm, JwtAuthConfig, RawBodyContentType, Request, RequestBody,
    SavedExample, Variable, rewrite_path_template,
};
use serde::Deserialize;
use std::collections::HashMap;

/// A folder-less request paste has nowhere to live in the tree on its own,
/// so importing a single cURL command always needs a target collection (and
/// optionally a folder within it) supplied by the caller -- this module
/// only builds the `Request` value, insertion into `ApiClientStore` is the
/// caller's job (mirrors how `store.rs` owns all mutation, `import.rs` only
/// produces plain data).
pub fn parse_curl(command: &str, collection_id: CollectionId) -> Result<Request> {
    let tokens = tokenize_shell_command(command)?;
    let mut tokens = tokens.into_iter().peekable();

    let Some(first) = tokens.next() else {
        bail!("empty command");
    };
    if first != "curl" {
        bail!("expected a command starting with `curl`, got `{first}`");
    }

    let mut method: Option<HttpMethod> = None;
    let mut url: Option<String> = None;
    let mut headers = Vec::new();
    let mut body: Option<String> = None;
    let mut basic_auth: Option<(String, String)> = None;

    while let Some(token) = tokens.next() {
        match token.as_str() {
            "-X" | "--request" => {
                let value = tokens.next().context("-X/--request needs a value")?;
                method = Some(parse_http_method(&value));
            }
            "-H" | "--header" => {
                let value = tokens.next().context("-H/--header needs a value")?;
                if let Some((key, val)) = value.split_once(':') {
                    headers.push(Header {
                        key: key.trim().to_string(),
                        value: val.trim().to_string(),
                        enabled: true,
                        description: None,
                    });
                }
            }
            "-d" | "--data" | "--data-raw" | "--data-binary" | "--data-ascii" => {
                let value = tokens.next().context("-d/--data needs a value")?;
                body = Some(value);
                if method.is_none() {
                    method = Some(HttpMethod::Post);
                }
            }
            "-u" | "--user" => {
                let value = tokens.next().context("-u/--user needs a value")?;
                if let Some((username, password)) = value.split_once(':') {
                    basic_auth = Some((username.to_string(), password.to_string()));
                }
            }
            "--url" => {
                url = Some(tokens.next().context("--url needs a value")?);
            }
            // Every other curl flag (-k, --compressed, -L, -s, -v, ...) is
            // accepted and ignored -- Phase 1 imports the request shape, not
            // transport-level curl behavior.
            flag if flag.starts_with('-') => {}
            value => {
                if url.is_none() {
                    url = Some(value.to_string());
                }
            }
        }
    }

    let url = url.context("no URL found in the curl command")?;
    let mut request = Request::new(collection_id, url.clone());
    request.url = url;
    request.method = method.unwrap_or_default();
    request.headers = headers;
    if let Some(text) = body {
        request.body = RequestBody::Raw {
            content_type: guess_body_content_type(&text),
            text,
        };
    }
    if let Some((username, password)) = basic_auth {
        request.auth = AuthConfig::Basic { username, password };
    }
    Ok(request)
}

fn parse_http_method(value: &str) -> HttpMethod {
    match value.to_ascii_uppercase().as_str() {
        "GET" => HttpMethod::Get,
        "POST" => HttpMethod::Post,
        "PUT" => HttpMethod::Put,
        "PATCH" => HttpMethod::Patch,
        "DELETE" => HttpMethod::Delete,
        "HEAD" => HttpMethod::Head,
        "OPTIONS" => HttpMethod::Options,
        other => HttpMethod::Custom(other.to_string()),
    }
}

fn guess_body_content_type(text: &str) -> RawBodyContentType {
    let trimmed = text.trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        RawBodyContentType::Json
    } else if trimmed.starts_with('<') {
        RawBodyContentType::Xml
    } else {
        RawBodyContentType::Text
    }
}

/// Splits a (possibly multi-line, backslash-continued) shell command into
/// tokens, honoring single and double quotes -- enough to cover the curl
/// commands browsers' "Copy as cURL" actions actually produce, not a full
/// POSIX shell grammar.
fn tokenize_shell_command(command: &str) -> Result<Vec<String>> {
    let normalized = command.replace("\\\n", " ").replace('\n', " ");
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_single_quotes = false;
    let mut in_double_quotes = false;
    let mut has_token = false;

    for ch in normalized.chars() {
        match ch {
            '\'' if !in_double_quotes => {
                in_single_quotes = !in_single_quotes;
                has_token = true;
            }
            '"' if !in_single_quotes => {
                in_double_quotes = !in_double_quotes;
                has_token = true;
            }
            c if c.is_whitespace() && !in_single_quotes && !in_double_quotes => {
                if has_token {
                    tokens.push(std::mem::take(&mut current));
                    has_token = false;
                }
            }
            c => {
                current.push(c);
                has_token = true;
            }
        }
    }
    if in_single_quotes || in_double_quotes {
        bail!("unterminated quote in curl command");
    }
    if has_token {
        tokens.push(current);
    }
    Ok(tokens)
}

// ----- Postman Collection Format v2.1 -----

#[derive(Debug, Deserialize)]
struct PostmanCollection {
    info: PostmanInfo,
    item: Vec<PostmanItem>,
    #[serde(default)]
    variable: Vec<PostmanVariable>,
}

#[derive(Debug, Deserialize)]
struct PostmanInfo {
    name: String,
}

#[derive(Debug, Deserialize)]
struct PostmanVariable {
    key: String,
    #[serde(default)]
    value: String,
}

#[derive(Debug, Deserialize)]
struct PostmanItem {
    name: String,
    #[serde(default)]
    item: Vec<PostmanItem>,
    #[serde(default)]
    request: Option<PostmanRequest>,
    /// Saved example responses attached to this item -- a sibling of
    /// `request`, not nested inside it, matching Postman's own schema.
    #[serde(default)]
    response: Vec<PostmanResponse>,
    /// Pre-request/test scripts, a sibling of `request` in Postman's schema
    /// (not nested inside it) -- see `scripts_from_events`.
    #[serde(default)]
    event: Vec<PostmanEvent>,
}

#[derive(Debug, Deserialize)]
struct PostmanRequest {
    #[serde(default)]
    method: String,
    url: PostmanUrl,
    #[serde(default)]
    header: Vec<PostmanHeader>,
    #[serde(default)]
    body: Option<PostmanBody>,
    #[serde(default)]
    auth: Option<PostmanAuth>,
}

/// Postman's auth block: `{"type": "bearer", "bearer": [{"key":"token", ...}]}`
/// -- the type name doubles as the key of the array holding that type's own
/// params, so every auth type's param list is captured by the same flattened
/// map rather than one dedicated field per type.
#[derive(Debug, Deserialize)]
struct PostmanAuth {
    r#type: String,
    #[serde(flatten)]
    params: HashMap<String, Vec<PostmanAuthParam>>,
}

#[derive(Debug, Deserialize)]
struct PostmanAuthParam {
    key: String,
    #[serde(default)]
    value: serde_json::Value,
}

fn auth_param_string(params: &[PostmanAuthParam], key: &str) -> String {
    params
        .iter()
        .find(|param| param.key == key)
        .map(|param| match &param.value {
            serde_json::Value::String(value) => value.clone(),
            serde_json::Value::Bool(value) => value.to_string(),
            serde_json::Value::Number(value) => value.to_string(),
            _ => String::new(),
        })
        .unwrap_or_default()
}

fn auth_param_bool(params: &[PostmanAuthParam], key: &str) -> bool {
    params
        .iter()
        .find(|param| param.key == key)
        .and_then(|param| param.value.as_bool())
        .unwrap_or(false)
}

/// Maps a Postman `auth` block to `AuthConfig`. `"noauth"` and any
/// unrecognized/absent type map to `AuthConfig::None` -- Postman's own
/// default when no `auth` key is present on a request is "inherit from
/// parent", but since collection/folder-level auth isn't modeled on this
/// side yet (see the module doc comment), there is nothing to inherit from,
/// so `None` is the closer match than `Inherit`.
fn parse_postman_auth(auth: PostmanAuth) -> AuthConfig {
    let empty = Vec::new();
    let params = auth.params.get(&auth.r#type).unwrap_or(&empty);
    match auth.r#type.as_str() {
        "noauth" => AuthConfig::None,
        "basic" => AuthConfig::Basic {
            username: auth_param_string(params, "username"),
            password: auth_param_string(params, "password"),
        },
        "bearer" => AuthConfig::Bearer {
            token: auth_param_string(params, "token"),
        },
        "apikey" => AuthConfig::ApiKey {
            key: auth_param_string(params, "key"),
            value: auth_param_string(params, "value"),
            placement: if auth_param_string(params, "in") == "query" {
                ApiKeyPlacement::Query
            } else {
                ApiKeyPlacement::Header
            },
        },
        "jwt" => AuthConfig::Jwt(JwtAuthConfig {
            algorithm: JwtAlgorithm::from_str(&auth_param_string(params, "algorithm")),
            secret: auth_param_string(params, "secret"),
            is_secret_base64_encoded: auth_param_bool(params, "isSecretBase64Encoded"),
            payload: auth_param_string(params, "payload"),
            header_prefix: auth_param_string(params, "headerPrefix"),
            add_to_query_param: auth_param_string(params, "addTokenTo") == "queryParams",
            query_param_key: auth_param_string(params, "queryParamKey"),
        }),
        _ => AuthConfig::None,
    }
}

/// One `event` entry: `{"listen": "prerequest"|"test", "script": {"exec": [...]}}`.
#[derive(Debug, Deserialize)]
struct PostmanEvent {
    listen: String,
    #[serde(default)]
    script: Option<PostmanScript>,
}

#[derive(Debug, Deserialize)]
struct PostmanScript {
    #[serde(default)]
    exec: Option<PostmanScriptExec>,
}

/// Postman inconsistently emits `exec` as either an array of lines or a
/// single string -- both are seen in real exports.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum PostmanScriptExec {
    Lines(Vec<String>),
    Single(String),
}

impl PostmanScriptExec {
    fn joined(&self) -> String {
        match self {
            PostmanScriptExec::Lines(lines) => lines.join("\n"),
            PostmanScriptExec::Single(text) => text.clone(),
        }
    }
}

/// Splits `event` entries into `(pre_request_script, test_script)`,
/// matching `Request::pre_request_script`/`test_script`. Multiple events
/// with the same `listen` value (unusual, but not disallowed by the schema)
/// are joined with a blank line rather than the later one silently winning.
fn scripts_from_events(events: Vec<PostmanEvent>) -> (String, String) {
    let mut pre_request_lines = Vec::new();
    let mut test_lines = Vec::new();
    for event in events {
        let Some(exec) = event.script.and_then(|script| script.exec) else {
            continue;
        };
        let joined = exec.joined();
        if joined.is_empty() {
            continue;
        }
        match event.listen.as_str() {
            "prerequest" => pre_request_lines.push(joined),
            "test" => test_lines.push(joined),
            _ => {}
        }
    }
    (pre_request_lines.join("\n\n"), test_lines.join("\n\n"))
}

/// One embedded "saved example" response, Postman's own name and JSON shape
/// for a named request/response snapshot attached to an item -- see
/// `SavedExample`, the equivalent type on this side.
#[derive(Debug, Deserialize)]
struct PostmanResponse {
    #[serde(default)]
    name: String,
    #[serde(default)]
    code: u16,
    #[serde(default)]
    header: Vec<PostmanHeader>,
    #[serde(default)]
    body: String,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum PostmanUrl {
    Raw(String),
    Detailed { raw: String },
}

impl PostmanUrl {
    fn raw(&self) -> &str {
        match self {
            PostmanUrl::Raw(raw) => raw,
            PostmanUrl::Detailed { raw } => raw,
        }
    }
}

#[derive(Debug, Deserialize)]
struct PostmanHeader {
    key: String,
    #[serde(default)]
    value: String,
    #[serde(default)]
    disabled: bool,
}

#[derive(Debug, Deserialize)]
struct PostmanBody {
    #[serde(default)]
    mode: String,
    #[serde(default)]
    raw: String,
    #[serde(default)]
    urlencoded: Vec<PostmanUrlEncodedParam>,
    #[serde(default)]
    graphql: Option<PostmanGraphQl>,
}

#[derive(Debug, Deserialize)]
struct PostmanUrlEncodedParam {
    key: String,
    #[serde(default)]
    value: String,
    #[serde(default)]
    disabled: bool,
}

#[derive(Debug, Deserialize)]
struct PostmanGraphQl {
    #[serde(default)]
    query: String,
    #[serde(default)]
    variables: String,
}

/// The result of importing a Postman collection: a `Collection` plus every
/// `Folder`/`Request` in it, all with `id`s already assigned and
/// `parent_id`/`folder_id` correctly wired -- ready to be pushed straight
/// into `ApiClientStore`'s vectors.
pub struct ImportedCollection {
    pub collection: Collection,
    pub folders: Vec<Folder>,
    pub requests: Vec<Request>,
}

pub fn parse_postman_collection(json: &str) -> Result<ImportedCollection> {
    let parsed: PostmanCollection =
        serde_json::from_str(json).context("not a valid Postman Collection v2.1 JSON document")?;

    let mut collection = Collection::new(parsed.info.name);
    collection.variables = parsed
        .variable
        .into_iter()
        .filter(|variable| !variable.key.is_empty())
        .map(|variable| Variable::new(variable.key, variable.value))
        .collect();

    let mut folders = Vec::new();
    let mut requests = Vec::new();
    let mut next_order = 0i64;
    for item in parsed.item {
        import_item(
            item,
            collection.id,
            None,
            &mut folders,
            &mut requests,
            &mut next_order,
        );
    }

    Ok(ImportedCollection {
        collection,
        folders,
        requests,
    })
}

/// A leaf with neither a nested `item` list nor a `request` object is
/// malformed Postman JSON (every leaf must be one or the other), but this
/// treats a missing `request` as "it must be a folder" -- so a malformed
/// leaf silently becomes an empty folder rather than aborting the whole
/// import. Acceptable for Phase 1: a bad collection produces a visibly
/// empty folder in the tree instead of failing the entire import.
fn import_item(
    item: PostmanItem,
    collection_id: CollectionId,
    parent_folder: Option<FolderId>,
    folders: &mut Vec<Folder>,
    requests: &mut Vec<Request>,
    next_order: &mut i64,
) {
    if let Some(postman_request) = item.request {
        let mut request = Request::new(collection_id, item.name);
        request.folder_id = parent_folder;
        request.order = *next_order;
        *next_order += 1;
        request.method = parse_http_method(&postman_request.method);
        request.url = postman_request.url.raw().to_string();
        request.headers = postman_request
            .header
            .into_iter()
            .map(|header| Header {
                key: header.key,
                value: header.value,
                enabled: !header.disabled,
                description: None,
            })
            .collect();
        if let Some(body) = postman_request.body {
            match body.mode.as_str() {
                "raw" if !body.raw.is_empty() => {
                    request.body = RequestBody::Raw {
                        content_type: guess_body_content_type(&body.raw),
                        text: body.raw,
                    };
                }
                "urlencoded" if !body.urlencoded.is_empty() => {
                    request.body = RequestBody::UrlEncoded(
                        body.urlencoded
                            .into_iter()
                            .filter(|param| !param.disabled)
                            .map(|param| (param.key, param.value))
                            .collect(),
                    );
                }
                "graphql" => {
                    if let Some(graphql) = body.graphql {
                        request.body = RequestBody::GraphQl {
                            query: graphql.query,
                            variables: graphql.variables,
                        };
                    }
                }
                _ => {}
            }
        }
        if let Some(auth) = postman_request.auth {
            request.auth = parse_postman_auth(auth);
        }
        let (pre_request_script, test_script) = scripts_from_events(item.event);
        request.pre_request_script = pre_request_script;
        request.test_script = test_script;
        request.examples = item
            .response
            .into_iter()
            .map(|response| {
                SavedExample::new(
                    if response.name.is_empty() {
                        format!("{} {}", response.code, request.name)
                    } else {
                        response.name
                    },
                    request.method.clone(),
                    request.url.clone(),
                    request.headers.clone(),
                    match &request.body {
                        RequestBody::Raw { text, .. } => text.clone(),
                        _ => String::new(),
                    },
                    response.code,
                    response
                        .header
                        .into_iter()
                        .map(|header| (header.key, header.value))
                        .collect(),
                    response.body,
                )
            })
            .collect();
        requests.push(request);
        return;
    }

    let folder = Folder::new(collection_id, item.name, parent_folder, *next_order);
    let folder_id = folder.id;
    *next_order += 1;
    folders.push(folder);

    let mut child_order = 0i64;
    for child in item.item {
        import_item(
            child,
            collection_id,
            Some(folder_id),
            folders,
            requests,
            &mut child_order,
        );
    }
}

// ----- Postman Environment export format -----

#[derive(Debug, Deserialize)]
struct PostmanEnvironment {
    #[serde(default)]
    name: String,
    #[serde(default)]
    values: Vec<PostmanEnvironmentValue>,
}

#[derive(Debug, Deserialize)]
struct PostmanEnvironmentValue {
    key: String,
    #[serde(default)]
    value: String,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    r#type: String,
}

fn default_true() -> bool {
    true
}

/// Parses a Postman environment export (`.postman_environment.json`):
/// `{ name, values: [{ key, value, enabled, type }], ... }`. `type: "secret"`
/// maps to `Variable.secret`; any other (or missing) `type` is a plain
/// variable. The environment's own `id` from the export is discarded --
/// `Environment::new` assigns a fresh one, matching how `parse_postman_collection`
/// also never reuses the source document's ids.
pub fn parse_postman_environment(json: &str) -> Result<Environment> {
    let parsed: PostmanEnvironment =
        serde_json::from_str(json).context("not a valid Postman environment export document")?;
    let mut environment = Environment::new(parsed.name);
    environment.variables = parsed
        .values
        .into_iter()
        .filter(|value| !value.key.is_empty())
        .map(|value| Variable {
            key: value.key,
            initial_value: value.value.clone(),
            current_value: value.value,
            secret: value.r#type == "secret",
            enabled: value.enabled,
        })
        .collect();
    Ok(environment)
}

// ----- OpenAPI 3.x / Swagger 2.0 -----

const OPENAPI_HTTP_METHODS: &[&str] = &[
    "get", "put", "post", "delete", "options", "head", "patch", "trace",
];

#[derive(Debug, Deserialize)]
struct OpenApiDocument {
    #[serde(default)]
    openapi: Option<String>,
    #[serde(default)]
    swagger: Option<String>,
    info: OpenApiInfo,
    #[serde(default)]
    servers: Vec<OpenApiServer>,
    #[serde(default)]
    host: Option<String>,
    #[serde(default, rename = "basePath")]
    base_path: Option<String>,
    #[serde(default)]
    schemes: Vec<String>,
    #[serde(default)]
    paths:
        std::collections::BTreeMap<String, std::collections::BTreeMap<String, serde_json::Value>>,
}

#[derive(Debug, Deserialize)]
struct OpenApiInfo {
    #[serde(default = "default_openapi_title")]
    title: String,
}

fn default_openapi_title() -> String {
    "Imported API".to_string()
}

#[derive(Debug, Deserialize)]
struct OpenApiServer {
    url: String,
}

#[derive(Debug, Default, Deserialize)]
struct OpenApiOperation {
    #[serde(default)]
    summary: String,
    #[serde(default, rename = "operationId")]
    operation_id: String,
    #[serde(default)]
    parameters: Vec<OpenApiParameter>,
    #[serde(default, rename = "requestBody")]
    request_body: Option<OpenApiRequestBody>,
}

#[derive(Debug, Deserialize)]
struct OpenApiParameter {
    name: String,
    #[serde(rename = "in")]
    location: String,
}

#[derive(Debug, Deserialize)]
struct OpenApiRequestBody {
    #[serde(default)]
    content: std::collections::BTreeMap<String, OpenApiMediaType>,
}

#[derive(Debug, Default, Deserialize)]
struct OpenApiMediaType {
    #[serde(default)]
    example: Option<serde_json::Value>,
}

/// Resolves the document's base URL: OpenAPI 3.x's `servers[0].url`, or
/// Swagger 2.0's `scheme://host/basePath` -- an empty string (rather than an
/// error) when neither is present, since the request is still importable
/// with just its path and the user can fill in a base URL or variable later.
fn openapi_base_url(document: &OpenApiDocument) -> String {
    if let Some(server) = document.servers.first() {
        return server.url.trim_end_matches('/').to_string();
    }
    let Some(host) = &document.host else {
        return String::new();
    };
    let scheme = document
        .schemes
        .first()
        .map(String::as_str)
        .unwrap_or("https");
    let base_path = document.base_path.as_deref().unwrap_or("");
    format!("{scheme}://{host}{base_path}")
}

pub fn parse_openapi_document(json: &str) -> Result<ImportedCollection> {
    let document: OpenApiDocument = serde_json::from_str(json)
        .context("not a valid OpenAPI 3.x or Swagger 2.0 JSON document")?;
    if document.openapi.is_none() && document.swagger.is_none() {
        bail!("missing `openapi` or `swagger` version field -- not an OpenAPI/Swagger document");
    }

    let base_url = openapi_base_url(&document);
    let collection = Collection::new(document.info.title);
    let mut requests = Vec::new();
    let mut order = 0i64;

    for (path, path_item) in &document.paths {
        for method_name in OPENAPI_HTTP_METHODS {
            let Some(operation_value) = path_item.get(*method_name) else {
                continue;
            };
            let operation: OpenApiOperation =
                serde_json::from_value(operation_value.clone()).unwrap_or_default();

            let name = if !operation.summary.is_empty() {
                operation.summary.clone()
            } else if !operation.operation_id.is_empty() {
                operation.operation_id.clone()
            } else {
                format!("{} {path}", method_name.to_uppercase())
            };

            let mut request = Request::new(collection.id, name);
            request.order = order;
            order += 1;
            request.method = parse_http_method(method_name);
            request.url = format!("{base_url}{}", rewrite_path_template(path));

            for parameter in &operation.parameters {
                match parameter.location.as_str() {
                    "query" => request.params.push(api_client::QueryParam {
                        key: parameter.name.clone(),
                        value: String::new(),
                        enabled: true,
                        description: None,
                    }),
                    "header" => request.headers.push(Header {
                        key: parameter.name.clone(),
                        value: String::new(),
                        enabled: true,
                        description: None,
                    }),
                    // "path" parameters are already captured by
                    // `rewrite_path_template`; "cookie" parameters have no
                    // representation in `Request` yet.
                    _ => {}
                }
            }

            if let Some(request_body) = &operation.request_body {
                if let Some(json_media_type) = request_body.content.get("application/json") {
                    let text = json_media_type
                        .example
                        .as_ref()
                        .map(|example| serde_json::to_string_pretty(example).unwrap_or_default())
                        .unwrap_or_default();
                    request.body = RequestBody::Raw {
                        content_type: RawBodyContentType::Json,
                        text,
                    };
                } else if let Some((content_type, _)) = request_body.content.iter().next() {
                    request.body = RequestBody::Raw {
                        content_type: if content_type.contains("xml") {
                            RawBodyContentType::Xml
                        } else {
                            RawBodyContentType::Text
                        },
                        text: String::new(),
                    };
                }
            }

            requests.push(request);
        }
    }

    Ok(ImportedCollection {
        collection,
        folders: Vec::new(),
        requests,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn a_simple_get_curl_command_parses_method_and_url() {
        let request = parse_curl("curl https://api.example.com/users", Uuid::new_v4()).unwrap();
        assert_eq!(request.method, HttpMethod::Get);
        assert_eq!(request.url, "https://api.example.com/users");
    }

    #[test]
    fn headers_and_a_json_body_are_parsed_and_method_defaults_to_post_with_a_body() {
        let command = r#"curl https://api.example.com/users -H "Content-Type: application/json" -d '{"name":"Alice"}'"#;
        let request = parse_curl(command, Uuid::new_v4()).unwrap();
        assert_eq!(request.method, HttpMethod::Post);
        assert_eq!(request.headers.len(), 1);
        assert_eq!(request.headers[0].key, "Content-Type");
        match &request.body {
            RequestBody::Raw { content_type, text } => {
                assert_eq!(*content_type, RawBodyContentType::Json);
                assert_eq!(text, r#"{"name":"Alice"}"#);
            }
            other => panic!("expected a Raw body, got {other:?}"),
        }
    }

    #[test]
    fn an_explicit_x_flag_overrides_the_data_implied_post_method() {
        let command = "curl -X PUT https://api.example.com/users/1 -d 'name=Alice'";
        let request = parse_curl(command, Uuid::new_v4()).unwrap();
        assert_eq!(request.method, HttpMethod::Put);
    }

    #[test]
    fn basic_auth_from_the_dash_u_flag_is_captured() {
        let command = "curl -u alice:secret https://api.example.com/me";
        let request = parse_curl(command, Uuid::new_v4()).unwrap();
        assert!(
            matches!(request.auth, AuthConfig::Basic { username, password } if username == "alice" && password == "secret")
        );
    }

    #[test]
    fn a_multi_line_backslash_continued_command_is_treated_as_one_line() {
        let command = "curl https://api.example.com/users \\\n  -H \"Accept: application/json\"";
        let request = parse_curl(command, Uuid::new_v4()).unwrap();
        assert_eq!(request.headers.len(), 1);
    }

    #[test]
    fn a_command_not_starting_with_curl_is_rejected() {
        assert!(parse_curl("wget https://api.example.com", Uuid::new_v4()).is_err());
    }

    #[test]
    fn a_command_with_no_url_is_rejected() {
        assert!(parse_curl("curl -X GET", Uuid::new_v4()).is_err());
    }

    #[test]
    fn an_unterminated_quote_is_rejected_rather_than_panicking() {
        assert!(
            parse_curl(
                "curl https://api.example.com -H 'unterminated",
                Uuid::new_v4()
            )
            .is_err()
        );
    }

    const SAMPLE_POSTMAN_COLLECTION: &str = r#"{
        "info": { "name": "Sample API", "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json" },
        "variable": [{ "key": "base_url", "value": "https://api.example.com" }],
        "item": [
            {
                "name": "Get users",
                "request": {
                    "method": "GET",
                    "url": { "raw": "{{base_url}}/users" },
                    "header": [{ "key": "Accept", "value": "application/json" }]
                }
            },
            {
                "name": "Auth",
                "item": [
                    {
                        "name": "Login",
                        "request": {
                            "method": "POST",
                            "url": "{{base_url}}/login",
                            "header": [],
                            "body": { "mode": "raw", "raw": "{\"user\":\"alice\"}" }
                        }
                    }
                ]
            }
        ]
    }"#;

    #[test]
    fn a_postman_collection_produces_the_collection_variables_top_level_request_and_nested_folder()
    {
        let imported = parse_postman_collection(SAMPLE_POSTMAN_COLLECTION).unwrap();
        assert_eq!(imported.collection.name, "Sample API");
        assert_eq!(imported.collection.variables.len(), 1);
        assert_eq!(imported.collection.variables[0].key, "base_url");

        assert_eq!(imported.folders.len(), 1);
        assert_eq!(imported.folders[0].name, "Auth");

        assert_eq!(imported.requests.len(), 2);
        let top_level = imported
            .requests
            .iter()
            .find(|r| r.name == "Get users")
            .unwrap();
        assert_eq!(top_level.method, HttpMethod::Get);
        assert_eq!(top_level.url, "{{base_url}}/users");
        assert!(top_level.folder_id.is_none());

        let nested = imported
            .requests
            .iter()
            .find(|r| r.name == "Login")
            .unwrap();
        assert_eq!(nested.method, HttpMethod::Post);
        assert_eq!(nested.folder_id, Some(imported.folders[0].id));
        match &nested.body {
            RequestBody::Raw { text, .. } => assert_eq!(text, r#"{"user":"alice"}"#),
            other => panic!("expected a Raw body, got {other:?}"),
        }
    }

    #[test]
    fn malformed_json_is_rejected_with_a_clear_error_rather_than_panicking() {
        assert!(parse_postman_collection("{not json").is_err());
    }

    const SAMPLE_POSTMAN_COLLECTION_WITH_EXAMPLE: &str = r#"{
        "info": { "name": "Sample API" },
        "item": [
            {
                "name": "Get users",
                "request": {
                    "method": "GET",
                    "url": { "raw": "https://api.example.com/users" },
                    "header": [{ "key": "Accept", "value": "application/json" }]
                },
                "response": [
                    {
                        "name": "200 OK",
                        "code": 200,
                        "header": [{ "key": "Content-Type", "value": "application/json" }],
                        "body": "{\"id\":1}"
                    }
                ]
            }
        ]
    }"#;

    #[test]
    fn a_postman_item_with_an_embedded_response_produces_a_saved_example_on_the_request() {
        let imported = parse_postman_collection(SAMPLE_POSTMAN_COLLECTION_WITH_EXAMPLE).unwrap();
        let request = &imported.requests[0];
        assert_eq!(request.examples.len(), 1);
        let example = &request.examples[0];
        assert_eq!(example.name, "200 OK");
        assert_eq!(example.response_status, 200);
        assert_eq!(example.response_body, r#"{"id":1}"#);
        assert_eq!(example.request_method, HttpMethod::Get);
        assert_eq!(example.request_url, "https://api.example.com/users");
    }

    const SAMPLE_POSTMAN_COLLECTION_WITH_AUTH_AND_SCRIPTS: &str = r#"{
        "info": { "name": "Sample API" },
        "item": [
            {
                "name": "Basic auth request",
                "request": {
                    "method": "GET",
                    "url": { "raw": "https://api.example.com/basic" },
                    "auth": {
                        "type": "basic",
                        "basic": [
                            { "key": "username", "value": "alice" },
                            { "key": "password", "value": "secret" }
                        ]
                    }
                },
                "event": [
                    { "listen": "prerequest", "script": { "exec": ["pm.environment.set(\"x\", 1);"] } },
                    { "listen": "test", "script": { "exec": ["pm.test(\"ok\", () => {});"] } }
                ]
            },
            {
                "name": "JWT auth request",
                "request": {
                    "method": "GET",
                    "url": { "raw": "https://api.example.com/jwt" },
                    "auth": {
                        "type": "jwt",
                        "jwt": [
                            { "key": "algorithm", "value": "HS256" },
                            { "key": "secret", "value": "changeit" },
                            { "key": "isSecretBase64Encoded", "value": false },
                            { "key": "payload", "value": "{\"sub\":\"user-1\"}" },
                            { "key": "headerPrefix", "value": "Bearer" },
                            { "key": "addTokenTo", "value": "header" }
                        ]
                    }
                }
            },
            {
                "name": "No auth request",
                "request": {
                    "method": "GET",
                    "url": { "raw": "https://api.example.com/public" },
                    "auth": { "type": "noauth" }
                }
            },
            {
                "name": "Form request",
                "request": {
                    "method": "POST",
                    "url": { "raw": "https://api.example.com/form" },
                    "body": {
                        "mode": "urlencoded",
                        "urlencoded": [
                            { "key": "grant_type", "value": "client_credentials" },
                            { "key": "disabled_field", "value": "x", "disabled": true }
                        ]
                    }
                }
            },
            {
                "name": "GraphQL request",
                "request": {
                    "method": "POST",
                    "url": { "raw": "https://api.example.com/graphql" },
                    "body": {
                        "mode": "graphql",
                        "graphql": {
                            "query": "query { users { id } }",
                            "variables": "{}"
                        }
                    }
                }
            }
        ]
    }"#;

    #[test]
    fn basic_auth_is_parsed_from_the_postman_auth_block() {
        let imported =
            parse_postman_collection(SAMPLE_POSTMAN_COLLECTION_WITH_AUTH_AND_SCRIPTS).unwrap();
        let request = imported
            .requests
            .iter()
            .find(|request| request.name == "Basic auth request")
            .unwrap();
        match &request.auth {
            AuthConfig::Basic { username, password } => {
                assert_eq!(username, "alice");
                assert_eq!(password, "secret");
            }
            other => panic!("expected AuthConfig::Basic, got {other:?}"),
        }
    }

    #[test]
    fn pre_request_and_test_scripts_are_parsed_from_the_event_array() {
        let imported =
            parse_postman_collection(SAMPLE_POSTMAN_COLLECTION_WITH_AUTH_AND_SCRIPTS).unwrap();
        let request = imported
            .requests
            .iter()
            .find(|request| request.name == "Basic auth request")
            .unwrap();
        assert_eq!(request.pre_request_script, "pm.environment.set(\"x\", 1);");
        assert_eq!(request.test_script, "pm.test(\"ok\", () => {});");
    }

    #[test]
    fn jwt_auth_is_parsed_from_the_postman_auth_block() {
        let imported =
            parse_postman_collection(SAMPLE_POSTMAN_COLLECTION_WITH_AUTH_AND_SCRIPTS).unwrap();
        let request = imported
            .requests
            .iter()
            .find(|request| request.name == "JWT auth request")
            .unwrap();
        match &request.auth {
            AuthConfig::Jwt(config) => {
                assert_eq!(config.algorithm, JwtAlgorithm::HS256);
                assert_eq!(config.secret, "changeit");
                assert!(!config.is_secret_base64_encoded);
                assert_eq!(config.payload, r#"{"sub":"user-1"}"#);
                assert_eq!(config.header_prefix, "Bearer");
                assert!(!config.add_to_query_param);
            }
            other => panic!("expected AuthConfig::Jwt, got {other:?}"),
        }
    }

    #[test]
    fn a_noauth_block_maps_to_auth_config_none() {
        let imported =
            parse_postman_collection(SAMPLE_POSTMAN_COLLECTION_WITH_AUTH_AND_SCRIPTS).unwrap();
        let request = imported
            .requests
            .iter()
            .find(|request| request.name == "No auth request")
            .unwrap();
        assert!(matches!(request.auth, AuthConfig::None));
    }

    #[test]
    fn a_urlencoded_body_skips_disabled_fields() {
        let imported =
            parse_postman_collection(SAMPLE_POSTMAN_COLLECTION_WITH_AUTH_AND_SCRIPTS).unwrap();
        let request = imported
            .requests
            .iter()
            .find(|request| request.name == "Form request")
            .unwrap();
        match &request.body {
            RequestBody::UrlEncoded(pairs) => {
                assert_eq!(
                    pairs,
                    &vec![("grant_type".to_string(), "client_credentials".to_string())]
                );
            }
            other => panic!("expected RequestBody::UrlEncoded, got {other:?}"),
        }
    }

    #[test]
    fn a_graphql_body_is_parsed_into_query_and_variables() {
        let imported =
            parse_postman_collection(SAMPLE_POSTMAN_COLLECTION_WITH_AUTH_AND_SCRIPTS).unwrap();
        let request = imported
            .requests
            .iter()
            .find(|request| request.name == "GraphQL request")
            .unwrap();
        match &request.body {
            RequestBody::GraphQl { query, variables } => {
                assert_eq!(query, "query { users { id } }");
                assert_eq!(variables, "{}");
            }
            other => panic!("expected RequestBody::GraphQl, got {other:?}"),
        }
    }

    const SAMPLE_POSTMAN_ENVIRONMENT: &str = r#"{
        "id": "00000000-0000-0000-0000-000000000099",
        "name": "Staging",
        "values": [
            { "key": "base_url", "value": "https://staging.example.com", "enabled": true, "type": "default" },
            { "key": "api_key", "value": "shh", "enabled": true, "type": "secret" },
            { "key": "unused", "value": "x", "enabled": false, "type": "default" }
        ],
        "_postman_variable_scope": "environment"
    }"#;

    #[test]
    fn a_postman_environment_export_produces_an_environment_with_secret_and_disabled_variables() {
        let environment = parse_postman_environment(SAMPLE_POSTMAN_ENVIRONMENT).unwrap();
        assert_eq!(environment.name, "Staging");
        assert_eq!(environment.variables.len(), 3);

        let base_url = environment
            .variables
            .iter()
            .find(|v| v.key == "base_url")
            .unwrap();
        assert!(!base_url.secret);
        assert_eq!(base_url.current_value, "https://staging.example.com");

        let api_key = environment
            .variables
            .iter()
            .find(|v| v.key == "api_key")
            .unwrap();
        assert!(api_key.secret);
        assert_eq!(api_key.current_value, "shh");

        let unused = environment
            .variables
            .iter()
            .find(|v| v.key == "unused")
            .unwrap();
        assert!(!unused.enabled);
    }

    #[test]
    fn malformed_postman_environment_json_is_rejected_rather_than_panicking() {
        assert!(parse_postman_environment("{not json").is_err());
    }

    const SAMPLE_OPENAPI_DOCUMENT: &str = r#"{
        "openapi": "3.0.0",
        "info": { "title": "Sample API" },
        "servers": [{ "url": "https://api.example.com/v1" }],
        "paths": {
            "/users/{id}": {
                "get": {
                    "summary": "Get a user",
                    "parameters": [
                        { "name": "id", "in": "path" },
                        { "name": "verbose", "in": "query" },
                        { "name": "X-Trace-Id", "in": "header" }
                    ]
                },
                "post": {
                    "operationId": "createUser",
                    "requestBody": {
                        "content": {
                            "application/json": { "example": { "name": "Alice" } }
                        }
                    }
                }
            }
        }
    }"#;

    #[test]
    fn an_openapi_document_produces_one_request_per_operation_with_the_server_url_and_rewritten_path_params()
     {
        let imported = parse_openapi_document(SAMPLE_OPENAPI_DOCUMENT).unwrap();
        assert_eq!(imported.collection.name, "Sample API");
        assert!(imported.folders.is_empty());
        assert_eq!(imported.requests.len(), 2);

        let get_user = imported
            .requests
            .iter()
            .find(|r| r.name == "Get a user")
            .unwrap();
        assert_eq!(get_user.method, HttpMethod::Get);
        assert_eq!(get_user.url, "https://api.example.com/v1/users/{{id}}");
        assert!(get_user.params.iter().any(|p| p.key == "verbose"));
        assert!(get_user.headers.iter().any(|h| h.key == "X-Trace-Id"));

        let create_user = imported
            .requests
            .iter()
            .find(|r| r.name == "createUser")
            .unwrap();
        assert_eq!(create_user.method, HttpMethod::Post);
        match &create_user.body {
            RequestBody::Raw { content_type, text } => {
                assert_eq!(*content_type, RawBodyContentType::Json);
                assert!(text.contains("Alice"));
            }
            other => panic!("expected a Raw JSON body, got {other:?}"),
        }
    }

    const SAMPLE_SWAGGER_2_DOCUMENT: &str = r#"{
        "swagger": "2.0",
        "info": { "title": "Legacy API" },
        "host": "legacy.example.com",
        "basePath": "/v2",
        "schemes": ["https"],
        "paths": {
            "/ping": {
                "get": {}
            }
        }
    }"#;

    #[test]
    fn a_swagger_2_document_builds_its_base_url_from_scheme_host_and_base_path() {
        let imported = parse_openapi_document(SAMPLE_SWAGGER_2_DOCUMENT).unwrap();
        assert_eq!(imported.requests.len(), 1);
        assert_eq!(
            imported.requests[0].url,
            "https://legacy.example.com/v2/ping"
        );
    }

    #[test]
    fn a_document_missing_both_version_fields_is_rejected() {
        let document = r#"{ "info": { "title": "No version" }, "paths": {} }"#;
        assert!(parse_openapi_document(document).is_err());
    }

    #[test]
    fn malformed_openapi_json_is_rejected_rather_than_panicking() {
        assert!(parse_openapi_document("{not json").is_err());
    }
}
