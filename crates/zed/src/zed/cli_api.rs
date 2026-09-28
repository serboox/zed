use std::sync::Arc;

use api_client::{
    AuthConfig, CollectionId, FolderId, Header, HttpMethod, QueryParam, RawBodyContentType,
    Request, RequestBody,
};
use api_client_ui::ApiClientStore;
use cli::{
    ApiChangeInfo, ApiCollectionInfo, ApiData, ApiFolderInfo, ApiHighlight, ApiOperation, ApiPair,
    ApiRequestChanges, ApiRequestDetail, ApiSnippetInfo, CliResponse, CliResponseSink, exit_status,
};
use gpui::{AsyncApp, Entity};
use theme::ActiveTheme as _;
use util::ResultExt as _;
use workspace::AppState;

use super::cli_requests::{API_NOT_READY, api_request_named, loaded_api_store, say_and_exit};

/// What stands in for a secret wherever zedcli shows a saved request.
const MASK: &str = "••••";

/// Headers whose literal value is a credential.
const SECRET_HEADERS: [&str; 5] = [
    "authorization",
    "cookie",
    "proxy-authorization",
    "x-api-key",
    "x-auth-token",
];

pub(crate) async fn manage_api(
    operation: ApiOperation,
    app_state: &Arc<AppState>,
    responses: &dyn CliResponseSink,
    cx: &mut AsyncApp,
) {
    let Some(store) = loaded_api_store(cx).await else {
        return say_and_exit(responses, API_NOT_READY.to_string(), exit_status::FAILED);
    };
    let answered = match operation {
        ApiOperation::Snippet {
            request,
            language,
            environment,
            variables,
            changes,
        } => {
            snippet(
                &store,
                SnippetAsked {
                    request,
                    language,
                    environment,
                    variables,
                    changes,
                },
                app_state,
                cx,
            )
            .await
        }
        operation => store.update(cx, |store, cx| operate(store, operation, cx)),
    };
    match answered {
        Ok(data) => {
            responses.send(CliResponse::Api { data }).log_err();
            responses.send(CliResponse::Exit { status: 0 }).log_err();
        }
        Err(Refusal { message, status }) => say_and_exit(responses, message, status),
    }
}

pub(crate) struct Refusal {
    pub message: String,
    pub status: i32,
}

impl Refusal {
    pub(crate) fn not_found(message: String) -> Self {
        Self {
            message,
            status: exit_status::NOT_FOUND,
        }
    }

    fn bad_arguments(message: String) -> Self {
        Self {
            message,
            status: exit_status::BAD_ARGUMENTS,
        }
    }

    fn failed(message: String) -> Self {
        Self {
            message,
            status: exit_status::FAILED,
        }
    }
}

fn operate(
    store: &mut ApiClientStore,
    operation: ApiOperation,
    cx: &mut gpui::Context<ApiClientStore>,
) -> Result<ApiData, Refusal> {
    match operation {
        ApiOperation::ListCollections => {
            let mut items: Vec<_> = store
                .collections
                .iter()
                .map(|collection| {
                    let (folders, requests) = store.collection_contents(collection.id);
                    ApiCollectionInfo {
                        id: collection.id.to_string(),
                        name: collection.name.clone(),
                        folders: folders as u64,
                        requests: requests as u64,
                    }
                })
                .collect();
            items.sort_by(|left, right| left.name.cmp(&right.name));
            Ok(ApiData::Collections(items))
        }
        ApiOperation::ListFolders => {
            let mut items: Vec<_> = store
                .folders
                .iter()
                .map(|folder| {
                    let (folders, requests) = store.folder_contents(folder.id);
                    ApiFolderInfo {
                        id: folder.id.to_string(),
                        path: folder_path(store, folder.id),
                        folders: folders as u64,
                        requests: requests as u64,
                    }
                })
                .collect();
            items.sort_by(|left, right| left.path.cmp(&right.path));
            Ok(ApiData::Folders(items))
        }
        ApiOperation::ShowRequest { request } => {
            let id = api_request_named(store, &request).map_err(Refusal::not_found)?;
            let request = saved_request(store, id)?;
            Ok(ApiData::Request(detail_of(store, &request)))
        }
        ApiOperation::CreateCollection { name } => {
            let name = a_name(&name)?;
            if store
                .collections
                .iter()
                .any(|collection| collection.name == name)
            {
                return Err(Refusal::failed(format!(
                    "There is a collection named '{name}' already."
                )));
            }
            let id = store.create_collection(name.clone(), cx);
            Ok(changed("created", "collection", id.to_string(), name))
        }
        ApiOperation::RenameCollection { collection, name } => {
            let id = collection_named(store, &collection)?;
            let name = a_name(&name)?;
            store.rename_collection(id, name.clone(), cx);
            Ok(changed("renamed", "collection", id.to_string(), name))
        }
        ApiOperation::DeleteCollection {
            collection,
            recursive,
        } => {
            let id = collection_named(store, &collection)?;
            let name = collection_name(store, id);
            let deleted = match recursive {
                true => store.delete_collection_with_contents(id, cx),
                false => store.delete_collection(id, cx),
            };
            if !deleted {
                let (folders, requests) = store.collection_contents(id);
                return Err(Refusal::failed(format!(
                    "'{name}' holds {folders} folders and {requests} requests; \
                     pass --recursive to delete them with it."
                )));
            }
            Ok(changed("deleted", "collection", id.to_string(), name))
        }
        ApiOperation::CreateFolder { path } => {
            let (collection, parent, name) = new_item_at(store, &path)?;
            let id = store
                .create_folder(collection, name, parent, cx)
                .ok_or_else(|| {
                    Refusal::failed(format!("'{path}' would nest folders too deeply."))
                })?;
            Ok(changed(
                "created",
                "folder",
                id.to_string(),
                folder_path(store, id),
            ))
        }
        ApiOperation::RenameFolder { folder, name } => {
            let id = folder_named(store, &folder)?;
            let name = a_name(&name)?;
            store.rename_folder(id, name, cx);
            Ok(changed(
                "renamed",
                "folder",
                id.to_string(),
                folder_path(store, id),
            ))
        }
        ApiOperation::DeleteFolder { folder, recursive } => {
            let id = folder_named(store, &folder)?;
            let path = folder_path(store, id);
            let deleted = match recursive {
                true => store.delete_folder_with_contents(id, cx),
                false => store.delete_folder(id, cx),
            };
            if !deleted {
                let (folders, requests) = store.folder_contents(id);
                return Err(Refusal::failed(format!(
                    "'{path}' holds {folders} folders and {requests} requests; \
                     pass --recursive to delete them with it."
                )));
            }
            Ok(changed("deleted", "folder", id.to_string(), path))
        }
        ApiOperation::CreateRequest { path, changes } => {
            let (collection, folder, name) = new_item_at(store, &path)?;
            // Built in full first, so a bad change creates nothing.
            let mut request = Request::new(collection, name.clone());
            apply_changes(&mut request, &changes)?;
            let id = store.create_request(collection, name, folder, cx);
            store.update_request(id, cx, |saved| {
                request.id = saved.id;
                request.folder_id = saved.folder_id;
                request.order = saved.order;
                *saved = request;
            });
            let path = path_of(store, id);
            Ok(changed("created", "request", id.to_string(), path))
        }
        ApiOperation::UpdateRequest {
            request,
            rename,
            move_to,
            changes,
        } => {
            let id = api_request_named(store, &request).map_err(Refusal::not_found)?;
            let mut edited = saved_request(store, id)?;
            apply_changes(&mut edited, &changes)?;
            if let Some(name) = &rename {
                edited.name = a_name(name)?;
            }
            if let Some(target) = &move_to {
                let (collection, folder) = place_named(store, target)?;
                if (collection, folder) != (edited.collection_id, edited.folder_id) {
                    edited.collection_id = collection;
                    edited.folder_id = folder;
                    edited.order = store.next_order_in(collection, folder);
                }
            }
            store.update_request(id, cx, |saved| *saved = edited);
            let action = match move_to.is_some() {
                true => "moved",
                false => "updated",
            };
            Ok(changed(
                action,
                "request",
                id.to_string(),
                path_of(store, id),
            ))
        }
        ApiOperation::DeleteRequest { request } => {
            let id = api_request_named(store, &request).map_err(Refusal::not_found)?;
            let path = path_of(store, id);
            store.delete_request(id, cx);
            Ok(changed("deleted", "request", id.to_string(), path))
        }
        ApiOperation::Snippet { .. } => Err(Refusal::failed(
            "a snippet is written outside the store".to_string(),
        )),
    }
}

fn changed(action: &str, kind: &str, id: String, path: String) -> ApiData {
    ApiData::Changed(ApiChangeInfo {
        action: action.to_string(),
        kind: kind.to_string(),
        id,
        path,
    })
}

fn a_name(name: &str) -> Result<String, Refusal> {
    let name = name.trim();
    if name.is_empty() {
        return Err(Refusal::bad_arguments(
            "A name cannot be empty.".to_string(),
        ));
    }
    if name.contains('/') {
        return Err(Refusal::bad_arguments(format!(
            "'{name}': a name cannot hold '/', which separates a path."
        )));
    }
    Ok(name.to_string())
}

fn saved_request(store: &ApiClientStore, id: api_client::RequestId) -> Result<Request, Refusal> {
    store
        .requests
        .iter()
        .find(|request| request.id == id)
        .cloned()
        .ok_or_else(|| Refusal::not_found("The request no longer exists.".to_string()))
}

fn path_of(store: &ApiClientStore, id: api_client::RequestId) -> String {
    store
        .requests
        .iter()
        .find(|request| request.id == id)
        .map(|request| store.path_of(request))
        .unwrap_or_default()
}

fn collection_name(store: &ApiClientStore, id: CollectionId) -> String {
    store
        .collections
        .iter()
        .find(|collection| collection.id == id)
        .map(|collection| collection.name.clone())
        .unwrap_or_default()
}

/// `Collection/Folder/...`, bounded by the number of folders so a parent
/// loop in a hand-edited file cannot spin forever.
fn folder_path(store: &ApiClientStore, id: FolderId) -> String {
    let mut parts = Vec::new();
    let mut folder_id = Some(id);
    let mut collection = None;
    for _ in 0..=store.folders.len() {
        let Some(id) = folder_id else {
            break;
        };
        let Some(folder) = store.folders.iter().find(|folder| folder.id == id) else {
            break;
        };
        parts.push(folder.name.clone());
        collection = Some(folder.collection_id);
        folder_id = folder.parent_id;
    }
    if let Some(collection) = collection {
        parts.push(collection_name(store, collection));
    }
    parts.reverse();
    parts.join("/")
}

fn collection_named(store: &ApiClientStore, named: &str) -> Result<CollectionId, Refusal> {
    let matching: Vec<_> = store
        .collections
        .iter()
        .filter(|collection| collection.id.to_string() == named || collection.name == named)
        .collect();
    match matching.as_slice() {
        [only] => Ok(only.id),
        [] => Err(Refusal::not_found(format!(
            "No collection '{named}'. See `zedcli api collections`."
        ))),
        several => Err(Refusal::not_found(format!(
            "{} collections are named '{named}'; give one of their ids: {}",
            several.len(),
            several
                .iter()
                .map(|collection| collection.id.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

fn folder_named(store: &ApiClientStore, named: &str) -> Result<FolderId, Refusal> {
    let matching: Vec<_> = store
        .folders
        .iter()
        .filter(|folder| folder.id.to_string() == named || folder_path(store, folder.id) == named)
        .collect();
    match matching.as_slice() {
        [only] => Ok(only.id),
        [] => Err(Refusal::not_found(format!(
            "No folder '{named}'. See `zedcli api folders`."
        ))),
        several => Err(Refusal::not_found(format!(
            "{} folders have the path '{named}'; give one of their ids: {}",
            several.len(),
            several
                .iter()
                .map(|folder| folder.id.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// A collection's name or a folder's path, as the place something goes into.
fn place_named(
    store: &ApiClientStore,
    named: &str,
) -> Result<(CollectionId, Option<FolderId>), Refusal> {
    let named = named.trim_end_matches('/');
    if !named.contains('/') {
        return Ok((collection_named(store, named)?, None));
    }
    let folder = folder_named(store, named)?;
    let collection = store
        .folders
        .iter()
        .find(|candidate| candidate.id == folder)
        .map(|candidate| candidate.collection_id)
        .ok_or_else(|| Refusal::not_found(format!("No folder '{named}'.")))?;
    Ok((collection, Some(folder)))
}

/// `Collection/Folder/.../Name`: where a new item goes, and its name.
fn new_item_at(
    store: &ApiClientStore,
    path: &str,
) -> Result<(CollectionId, Option<FolderId>, String), Refusal> {
    let Some((place, name)) = path.trim_end_matches('/').rsplit_once('/') else {
        return Err(Refusal::bad_arguments(format!(
            "'{path}': give the whole path, Collection/Name or Collection/Folder/Name."
        )));
    };
    let (collection, folder) = place_named(store, place)?;
    Ok((collection, folder, a_name(name)?))
}

pub(crate) fn apply_changes(
    request: &mut Request,
    changes: &ApiRequestChanges,
) -> Result<(), Refusal> {
    if let Some(method) = &changes.method {
        request.method = method_named(method)?;
    }
    if let Some(url) = &changes.url {
        request.url = url.trim().to_string();
    }
    for (key, value) in &changes.set_headers {
        match request
            .headers
            .iter_mut()
            .find(|header| header.key.eq_ignore_ascii_case(key))
        {
            Some(header) => {
                header.value = value.clone();
                header.enabled = true;
            }
            None => request.headers.push(Header {
                key: key.clone(),
                value: value.clone(),
                enabled: true,
                description: None,
            }),
        }
    }
    for key in &changes.remove_headers {
        request
            .headers
            .retain(|header| !header.key.eq_ignore_ascii_case(key));
    }
    for (key, value) in &changes.set_params {
        match request.params.iter_mut().find(|param| &param.key == key) {
            Some(param) => {
                param.value = value.clone();
                param.enabled = true;
            }
            None => request.params.push(QueryParam {
                key: key.clone(),
                value: value.clone(),
                enabled: true,
                description: None,
            }),
        }
    }
    for key in &changes.remove_params {
        request.params.retain(|param| &param.key != key);
    }
    let content_type = changes
        .content_type
        .as_deref()
        .map(content_type_named)
        .transpose()?;
    match (&changes.body, content_type) {
        (Some(text), _) if text.is_empty() => request.body = RequestBody::None,
        (Some(text), content_type) => {
            let content_type = content_type.unwrap_or_else(|| match &request.body {
                RequestBody::Raw { content_type, .. } => *content_type,
                _ => guessed_content_type(text),
            });
            request.body = RequestBody::Raw {
                content_type,
                text: text.clone(),
            };
        }
        (None, Some(new_type)) => match &mut request.body {
            RequestBody::Raw { content_type, .. } => *content_type = new_type,
            _ => {
                return Err(Refusal::bad_arguments(
                    "--content-type is for a raw body; give one with --body.".to_string(),
                ));
            }
        },
        (None, None) => {}
    }
    if let Some(description) = &changes.description {
        request.description = (!description.is_empty()).then(|| description.clone());
    }
    Ok(())
}

fn method_named(method: &str) -> Result<HttpMethod, Refusal> {
    let method = method.trim().to_ascii_uppercase();
    Ok(match method.as_str() {
        "GET" => HttpMethod::Get,
        "POST" => HttpMethod::Post,
        "PUT" => HttpMethod::Put,
        "PATCH" => HttpMethod::Patch,
        "DELETE" => HttpMethod::Delete,
        "HEAD" => HttpMethod::Head,
        "OPTIONS" => HttpMethod::Options,
        custom
            if !custom.is_empty()
                && custom
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '-') =>
        {
            HttpMethod::Custom(custom.to_string())
        }
        _ => {
            return Err(Refusal::bad_arguments(format!(
                "'{method}' is not an HTTP method."
            )));
        }
    })
}

fn content_type_named(name: &str) -> Result<RawBodyContentType, Refusal> {
    Ok(match name.trim().to_ascii_lowercase().as_str() {
        "text" | "plain" => RawBodyContentType::Text,
        "json" => RawBodyContentType::Json,
        "xml" => RawBodyContentType::Xml,
        "html" => RawBodyContentType::Html,
        "javascript" | "js" => RawBodyContentType::JavaScript,
        _ => {
            return Err(Refusal::bad_arguments(format!(
                "'{name}' is not a body type: text, json, xml, html or javascript."
            )));
        }
    })
}

fn guessed_content_type(text: &str) -> RawBodyContentType {
    match text.trim_start().chars().next() {
        Some('{') | Some('[') => RawBodyContentType::Json,
        Some('<') => RawBodyContentType::Xml,
        _ => RawBodyContentType::Text,
    }
}

/// A value as it may be shown: made only of `{{variable}}` references, and at
/// most an auth scheme such as `Bearer`, it is shown as written; anything else
/// is a literal credential and is masked.
fn unless_a_reference(value: &str) -> String {
    let trimmed = value.trim();
    let mut literal = String::new();
    let mut rest = trimmed;
    while let Some(start) = rest.find("{{") {
        let Some(length) = rest[start..].find("}}") else {
            break;
        };
        literal.push_str(&rest[..start]);
        rest = &rest[start + length + 2..];
    }
    literal.push_str(rest);
    let literal = literal.trim().to_ascii_lowercase();
    match literal.is_empty() || AUTH_SCHEMES.contains(&literal.as_str()) {
        true => trimmed.to_string(),
        false => MASK.to_string(),
    }
}

const AUTH_SCHEMES: [&str; 4] = ["basic", "bearer", "digest", "token"];

/// Query parameters whose literal value is a credential.
const SECRET_PARAMS: [&str; 9] = [
    "access_token",
    "api_key",
    "apikey",
    "key",
    "password",
    "secret",
    "sig",
    "signature",
    "token",
];

fn is_a_secret_header(key: &str) -> bool {
    SECRET_HEADERS
        .iter()
        .any(|secret| key.eq_ignore_ascii_case(secret))
}

fn is_a_secret_param(key: &str) -> bool {
    SECRET_PARAMS
        .iter()
        .any(|secret| key.eq_ignore_ascii_case(secret))
}

/// `url` with the password of a `user:password@` in it masked, unless that
/// password is a `{{variable}}` reference.
fn url_as_shown(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else {
        return url.to_string();
    };
    let authority_start = scheme_end + 3;
    let authority_end = url[authority_start..]
        .find(['/', '?', '#'])
        .map_or(url.len(), |at| authority_start + at);
    let Some(at) = url[authority_start..authority_end].rfind('@') else {
        return url.to_string();
    };
    let userinfo = &url[authority_start..authority_start + at];
    let Some(colon) = userinfo.find(':') else {
        return url.to_string();
    };
    let password = &userinfo[colon + 1..];
    format!(
        "{}{}{}",
        &url[..authority_start + colon + 1],
        unless_a_reference(password),
        &url[authority_start + at..]
    )
}

/// Every literal credential in `request` masked, so it can be printed as
/// code: `{{variable}}` references stay, and a secret variable's value is
/// masked where it is resolved.
fn with_literal_credentials_masked(request: &mut Request) {
    request.url = url_as_shown(&request.url);
    for header in &mut request.headers {
        if is_a_secret_header(&header.key) {
            header.value = unless_a_reference(&header.value);
        }
    }
    for param in &mut request.params {
        if is_a_secret_param(&param.key) {
            param.value = unless_a_reference(&param.value);
        }
    }
    let mask = |value: &mut String| *value = unless_a_reference(value);
    match &mut request.auth {
        AuthConfig::Inherit | AuthConfig::None => {}
        AuthConfig::Basic { password, .. } => mask(password),
        AuthConfig::Bearer { token } => mask(token),
        AuthConfig::ApiKey { value, .. } => mask(value),
        AuthConfig::OAuth2(config) => {
            mask(&mut config.client_secret);
            mask(&mut config.access_token);
            mask(&mut config.refresh_token);
        }
        AuthConfig::AwsSigV4(config) => {
            mask(&mut config.secret_key);
            mask(&mut config.session_token);
        }
        AuthConfig::Jwt(config) => mask(&mut config.secret),
    }
}

fn detail_of(store: &ApiClientStore, request: &Request) -> ApiRequestDetail {
    let (body_kind, body) = match &request.body {
        RequestBody::None => ("none".to_string(), None),
        RequestBody::Raw { content_type, text } => {
            (raw_kind(*content_type).to_string(), Some(text.clone()))
        }
        RequestBody::FormData(fields) => (
            "form-data".to_string(),
            Some(
                fields
                    .iter()
                    .map(|field| match &field.value {
                        api_client::FormDataValue::Text(text) => format!("{}={text}", field.key),
                        api_client::FormDataValue::File(path) => {
                            format!("{}=@{}", field.key, path.display())
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
        ),
        RequestBody::UrlEncoded(pairs) => (
            "urlencoded".to_string(),
            Some(
                pairs
                    .iter()
                    .map(|(key, value)| format!("{key}={value}"))
                    .collect::<Vec<_>>()
                    .join("&"),
            ),
        ),
        RequestBody::Binary { path } => ("binary".to_string(), Some(path.display().to_string())),
        RequestBody::GraphQl { query, variables } => (
            "graphql".to_string(),
            Some(match variables.trim().is_empty() {
                true => query.clone(),
                false => format!("{query}\n\nvariables: {variables}"),
            }),
        ),
    };
    ApiRequestDetail {
        id: request.id.to_string(),
        path: store.path_of(request),
        method: request.method.as_str().to_string(),
        url: url_as_shown(&request.url),
        description: request.description.clone(),
        params: request
            .params
            .iter()
            .map(|param| ApiPair {
                key: param.key.clone(),
                value: match is_a_secret_param(&param.key) {
                    true => unless_a_reference(&param.value),
                    false => param.value.clone(),
                },
                enabled: param.enabled,
            })
            .collect(),
        headers: request
            .headers
            .iter()
            .map(|header| ApiPair {
                key: header.key.clone(),
                value: match is_a_secret_header(&header.key) {
                    true => unless_a_reference(&header.value),
                    false => header.value.clone(),
                },
                enabled: header.enabled,
            })
            .collect(),
        body_kind,
        body,
        auth: auth_summary(&request.auth),
        pre_request_script: !request.pre_request_script.trim().is_empty(),
        test_script: !request.test_script.trim().is_empty(),
    }
}

fn raw_kind(content_type: RawBodyContentType) -> &'static str {
    match content_type {
        RawBodyContentType::Text => "text",
        RawBodyContentType::Json => "json",
        RawBodyContentType::Xml => "xml",
        RawBodyContentType::Html => "html",
        RawBodyContentType::JavaScript => "javascript",
    }
}

fn auth_summary(auth: &AuthConfig) -> String {
    match auth {
        AuthConfig::Inherit => "inherit".to_string(),
        AuthConfig::None => "none".to_string(),
        AuthConfig::Basic { username, password } => {
            format!("basic {username}:{}", unless_a_reference(password))
        }
        AuthConfig::Bearer { token } => format!("bearer {}", unless_a_reference(token)),
        AuthConfig::ApiKey {
            key,
            value,
            placement,
        } => format!(
            "api-key {key}={} in the {}",
            unless_a_reference(value),
            match placement {
                api_client::ApiKeyPlacement::Header => "header",
                api_client::ApiKeyPlacement::Query => "query",
            }
        ),
        AuthConfig::OAuth2(_) => "oauth2".to_string(),
        AuthConfig::AwsSigV4(_) => "aws-sigv4".to_string(),
        AuthConfig::Jwt(_) => "jwt".to_string(),
    }
}

struct SnippetAsked {
    request: String,
    language: String,
    environment: Option<String>,
    variables: Vec<(String, String)>,
    changes: ApiRequestChanges,
}

async fn snippet(
    store: &Entity<ApiClientStore>,
    asked: SnippetAsked,
    app_state: &Arc<AppState>,
    cx: &mut AsyncApp,
) -> Result<ApiData, Refusal> {
    let Some(shape) = api_client_ui::Snippet::from_cli_name(&asked.language) else {
        return Err(Refusal::bad_arguments(format!(
            "No snippet language '{}'; one of: {}.",
            asked.language,
            api_client_ui::Snippet::ALL
                .iter()
                .map(|snippet| snippet.cli_name())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    };
    let send = store.read_with(cx, |store, _| {
        let id = api_request_named(store, &asked.request).map_err(Refusal::not_found)?;
        let environment = environment_named(store, asked.environment.as_deref())?;
        let mut edited = saved_request(store, id)?;
        apply_changes(&mut edited, &asked.changes)?;
        with_literal_credentials_masked(&mut edited);
        Ok::<_, Refusal>(api_client_ui::headless_send::HeadlessSend {
            request: id,
            environment,
            variables: asked.variables,
            timeout: std::time::Duration::ZERO,
            edited: Some(edited),
        })
    })?;
    let code = api_client_ui::headless_send::snippet(store, send, shape, cx)
        .await
        .map_err(|error| Refusal::failed(format!("{error:#}")))?;
    let highlights = highlighted(&code, shape.language_name(), app_state, cx).await;
    Ok(ApiData::Snippet(ApiSnippetInfo {
        label: shape.label().to_string(),
        code,
        highlights,
    }))
}

pub(crate) fn environment_named(
    store: &ApiClientStore,
    named: Option<&str>,
) -> Result<Option<api_client::EnvironmentId>, Refusal> {
    let Some(named) = named else {
        return Ok(None);
    };
    store
        .environments
        .iter()
        .find(|candidate| candidate.id.to_string() == named || candidate.name == named)
        .map(|candidate| Some(candidate.id))
        .ok_or_else(|| {
            Refusal::not_found(format!("No environment '{named}'. See `zedcli api envs`."))
        })
}

/// `code` in the syntax colours of the editor's theme; nothing when the editor
/// has no grammar for `language`.
async fn highlighted(
    code: &str,
    language: &str,
    app_state: &Arc<AppState>,
    cx: &mut AsyncApp,
) -> Vec<ApiHighlight> {
    let Some(language) = app_state.languages.language_for_name(language).await.ok() else {
        return Vec::new();
    };
    cx.update(|cx| {
        let syntax = cx.theme().syntax().clone();
        language
            .highlight_text_with_theme(&rope::Rope::from(code), 0..code.len(), &syntax)
            .into_iter()
            .filter_map(|(range, highlight_id)| {
                let style = syntax.get(highlight_id)?;
                let color = gpui::Rgba::from(style.color?);
                let channel = |value: f32| (value.clamp(0., 1.) * 255.).round() as u32;
                Some(ApiHighlight {
                    start: range.start as u64,
                    end: range.end as u64,
                    color: channel(color.r) << 16 | channel(color.g) << 8 | channel(color.b),
                    bold: style
                        .font_weight
                        .is_some_and(|weight| weight >= gpui::FontWeight::BOLD),
                    italic: style.font_style == Some(gpui::FontStyle::Italic),
                })
            })
            .collect()
    })
}
