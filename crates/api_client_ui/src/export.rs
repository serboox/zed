use api_client::{
    ApiKeyPlacement, AuthConfig, Collection, Environment, Folder, FolderId, HttpMethod, Request,
    RequestBody,
};
use serde_json::{Value, json};

/// Serializes `header` rows to Postman's own header shape:
/// `{ "key": ..., "value": ..., "disabled": ... }` -- the exact inverse of
/// `PostmanHeader`'s `Deserialize` impl in `import.rs`.
fn headers_to_postman(headers: &[api_client::Header]) -> Vec<Value> {
    headers
        .iter()
        .map(|header| {
            json!({
                "key": header.key,
                "value": header.value,
                "disabled": !header.enabled,
            })
        })
        .collect()
}

/// Serializes `body` to Postman's own `body` shape -- the exact inverse of
/// `PostmanBody`'s `Deserialize` impl in `import.rs`. Body kinds this crate
/// has no Postman-native representation for (`FormData`, `Binary`) fall back
/// to an empty raw body, matching how `import.rs` never produces them either.
fn body_to_postman(body: &RequestBody) -> Value {
    match body {
        RequestBody::Raw { text, .. } => json!({ "mode": "raw", "raw": text }),
        RequestBody::UrlEncoded(pairs) => json!({
            "mode": "urlencoded",
            "urlencoded": pairs
                .iter()
                .map(|(key, value)| json!({ "key": key, "value": value, "disabled": false }))
                .collect::<Vec<_>>(),
        }),
        RequestBody::GraphQl { query, variables } => json!({
            "mode": "graphql",
            "graphql": { "query": query, "variables": variables },
        }),
        RequestBody::None | RequestBody::FormData(_) | RequestBody::Binary { .. } => {
            json!({ "mode": "raw", "raw": "" })
        }
    }
}

/// Serializes `auth` to Postman's own `auth` block shape -- the exact
/// inverse of `parse_postman_auth` in `import.rs`. `AuthConfig::Inherit`
/// exports as no `auth` key at all (`None`), matching Postman's own
/// omit-to-inherit convention; `OAuth2`/`AwsSigV4` have no Postman-native
/// auth type, so they export as `"noauth"` rather than losing the request
/// silently or crashing the export.
fn auth_to_postman(auth: &AuthConfig) -> Option<Value> {
    match auth {
        AuthConfig::Inherit => None,
        AuthConfig::None | AuthConfig::OAuth2(_) | AuthConfig::AwsSigV4(_) => {
            Some(json!({ "type": "noauth" }))
        }
        AuthConfig::Basic { username, password } => Some(json!({
            "type": "basic",
            "basic": [
                { "key": "username", "value": username },
                { "key": "password", "value": password },
            ],
        })),
        AuthConfig::Bearer { token } => Some(json!({
            "type": "bearer",
            "bearer": [{ "key": "token", "value": token }],
        })),
        AuthConfig::ApiKey {
            key,
            value,
            placement,
        } => Some(json!({
            "type": "apikey",
            "apikey": [
                { "key": "key", "value": key },
                { "key": "value", "value": value },
                {
                    "key": "in",
                    "value": if *placement == ApiKeyPlacement::Query { "query" } else { "header" },
                },
            ],
        })),
        AuthConfig::Jwt(config) => Some(json!({
            "type": "jwt",
            "jwt": [
                { "key": "algorithm", "value": config.algorithm.as_str() },
                { "key": "secret", "value": config.secret },
                { "key": "isSecretBase64Encoded", "value": config.is_secret_base64_encoded },
                { "key": "payload", "value": config.payload },
                { "key": "headerPrefix", "value": config.header_prefix },
                {
                    "key": "addTokenTo",
                    "value": if config.add_to_query_param { "queryParams" } else { "header" },
                },
                { "key": "queryParamKey", "value": config.query_param_key },
            ],
        })),
    }
}

/// Serializes a request's scripts to Postman's `event` array -- the exact
/// inverse of `scripts_from_events` in `import.rs`. An empty script produces
/// no `event` entry for that listener, matching how a Postman request with
/// no script for a given phase omits it entirely rather than emitting an
/// entry with empty `exec`.
fn scripts_to_postman(request: &Request) -> Vec<Value> {
    let mut events = Vec::new();
    if !request.pre_request_script.is_empty() {
        events.push(json!({
            "listen": "prerequest",
            "script": { "type": "text/javascript", "exec": [request.pre_request_script] },
        }));
    }
    if !request.test_script.is_empty() {
        events.push(json!({
            "listen": "test",
            "script": { "type": "text/javascript", "exec": [request.test_script] },
        }));
    }
    events
}

/// Serializes one `Request`'s saved examples to Postman's embedded
/// `response: [...]` array -- the inverse of `PostmanResponse`'s
/// `Deserialize` impl and `import_item`'s example-building in `import.rs`.
fn examples_to_postman(request: &Request) -> Vec<Value> {
    request
        .examples
        .iter()
        .map(|example| {
            json!({
                "name": example.name,
                "originalRequest": {
                    "method": example.request_method.as_str(),
                    "url": { "raw": example.request_url },
                    "header": headers_to_postman(&example.request_headers),
                    "body": { "mode": "raw", "raw": example.request_body_text },
                },
                "status": if (200..300).contains(&example.response_status) { "OK" } else { "" },
                "code": example.response_status,
                "header": example
                    .response_headers
                    .iter()
                    .map(|(key, value)| json!({ "key": key, "value": value, "disabled": false }))
                    .collect::<Vec<_>>(),
                "body": example.response_body,
            })
        })
        .collect()
}

fn request_to_postman_item(request: &Request) -> Value {
    let method = match &request.method {
        HttpMethod::Custom(name) => name.clone(),
        other => other.as_str().to_string(),
    };
    let mut postman_request = json!({
        "method": method,
        "url": { "raw": request.url },
        "header": headers_to_postman(&request.headers),
        "body": body_to_postman(&request.body),
    });
    if let Some(auth) = auth_to_postman(&request.auth) {
        postman_request["auth"] = auth;
    }
    let mut item = json!({
        "name": request.name,
        "request": postman_request,
        "response": examples_to_postman(request),
    });
    let events = scripts_to_postman(request);
    if !events.is_empty() {
        item["event"] = json!(events);
    }
    item
}

/// Builds the `item` array for one folder level: every request directly in
/// `parent_folder_id` (top-level requests when `None`), then every child
/// folder as a nested `item` object with its own requests/subfolders --
/// mirrors `parse_postman_collection`'s `import_item` tree shape in reverse.
fn build_items(
    parent_folder_id: Option<FolderId>,
    folders: &[Folder],
    requests: &[Request],
) -> Vec<Value> {
    let mut items = Vec::new();

    let mut own_requests: Vec<&Request> = requests
        .iter()
        .filter(|request| request.folder_id == parent_folder_id)
        .collect();
    own_requests.sort_by_key(|request| request.order);
    items.extend(own_requests.into_iter().map(request_to_postman_item));

    let mut child_folders: Vec<&Folder> = folders
        .iter()
        .filter(|folder| folder.parent_id == parent_folder_id)
        .collect();
    child_folders.sort_by_key(|folder| folder.order);
    for folder in child_folders {
        items.push(json!({
            "name": folder.name,
            "item": build_items(Some(folder.id), folders, requests),
        }));
    }

    items
}

/// Serializes a `Collection` and its `Folder`s/`Request`s (including every
/// request's `SavedExample`s) to a Postman Collection Format v2.1 JSON
/// document -- the structural inverse of `import.rs::parse_postman_collection`.
pub fn export_postman_collection(
    collection: &Collection,
    folders: &[Folder],
    requests: &[Request],
) -> String {
    let document = json!({
        "info": {
            "name": collection.name,
            "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json",
        },
        "variable": collection
            .variables
            .iter()
            .map(|variable| json!({ "key": variable.key, "value": variable.value_for_send() }))
            .collect::<Vec<_>>(),
        "item": build_items(None, folders, requests),
    });
    serde_json::to_string_pretty(&document).unwrap_or_default()
}

/// Serializes an `Environment` to Postman's environment export shape --
/// the structural inverse of `import.rs::parse_postman_environment`. Exports
/// the real (unmasked) value for secret variables, matching what Postman's
/// own export does -- `Variable::value_for_display()`'s masking is a UI
/// display concern only, never applied here.
pub fn export_postman_environment(environment: &Environment) -> String {
    let document = json!({
        "id": environment.id.to_string(),
        "name": environment.name,
        "values": environment
            .variables
            .iter()
            .map(|variable| json!({
                "key": variable.key,
                "value": variable.value_for_send(),
                "enabled": variable.enabled,
                "type": if variable.secret { "secret" } else { "default" },
            }))
            .collect::<Vec<_>>(),
        "_postman_variable_scope": "environment",
    });
    serde_json::to_string_pretty(&document).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use api_client::{Header, Variable};

    #[test]
    fn a_collection_with_a_folder_and_request_exports_then_reimports_with_the_same_shape() {
        let mut collection = Collection::new("Sample API".to_string());
        collection.variables.push(Variable::new(
            "base_url".to_string(),
            "https://api.example.com".to_string(),
        ));

        let folder = Folder::new(collection.id, "Auth".to_string(), None, 0);

        let mut top_level = Request::new(collection.id, "Get users".to_string());
        top_level.method = HttpMethod::Get;
        top_level.url = "{{base_url}}/users".to_string();
        top_level.headers.push(Header {
            key: "Accept".to_string(),
            value: "application/json".to_string(),
            enabled: true,
            description: None,
        });

        let mut nested = Request::new(collection.id, "Login".to_string());
        nested.folder_id = Some(folder.id);
        nested.method = HttpMethod::Post;
        nested.url = "{{base_url}}/login".to_string();
        nested.body = RequestBody::Raw {
            content_type: api_client::RawBodyContentType::Json,
            text: r#"{"user":"alice"}"#.to_string(),
        };

        let exported = export_postman_collection(&collection, &[folder], &[top_level, nested]);

        let reimported = crate::import::parse_postman_collection(&exported).unwrap();
        assert_eq!(reimported.collection.name, "Sample API");
        assert_eq!(reimported.collection.variables.len(), 1);
        assert_eq!(reimported.collection.variables[0].key, "base_url");
        assert_eq!(reimported.folders.len(), 1);
        assert_eq!(reimported.folders[0].name, "Auth");
        assert_eq!(reimported.requests.len(), 2);

        let get_users = reimported
            .requests
            .iter()
            .find(|r| r.name == "Get users")
            .unwrap();
        assert_eq!(get_users.method, HttpMethod::Get);
        assert_eq!(get_users.url, "{{base_url}}/users");
        assert!(get_users.folder_id.is_none());
        assert_eq!(get_users.headers.len(), 1);
        assert_eq!(get_users.headers[0].key, "Accept");

        let login = reimported
            .requests
            .iter()
            .find(|r| r.name == "Login")
            .unwrap();
        assert_eq!(login.method, HttpMethod::Post);
        assert_eq!(login.folder_id, Some(reimported.folders[0].id));
        match &login.body {
            RequestBody::Raw { text, .. } => assert_eq!(text, r#"{"user":"alice"}"#),
            other => panic!("expected a Raw body, got {other:?}"),
        }
    }

    #[test]
    fn a_saved_example_round_trips_through_export_and_reimport() {
        let collection = Collection::new("Sample API".to_string());
        let mut request = Request::new(collection.id, "Get users".to_string());
        request.method = HttpMethod::Get;
        request.url = "https://api.example.com/users".to_string();
        request.examples.push(api_client::SavedExample::new(
            "200 OK".to_string(),
            HttpMethod::Get,
            "https://api.example.com/users".to_string(),
            Vec::new(),
            String::new(),
            200,
            vec![("Content-Type".to_string(), "application/json".to_string())],
            r#"{"id":1}"#.to_string(),
        ));

        let exported = export_postman_collection(&collection, &[], &[request]);
        let reimported = crate::import::parse_postman_collection(&exported).unwrap();

        assert_eq!(reimported.requests.len(), 1);
        let examples = &reimported.requests[0].examples;
        assert_eq!(examples.len(), 1);
        assert_eq!(examples[0].name, "200 OK");
        assert_eq!(examples[0].response_status, 200);
        assert_eq!(examples[0].response_body, r#"{"id":1}"#);
    }

    #[test]
    fn basic_auth_round_trips_through_export_and_reimport() {
        let collection = Collection::new("Sample API".to_string());
        let mut request = Request::new(collection.id, "Get users".to_string());
        request.auth = AuthConfig::Basic {
            username: "alice".to_string(),
            password: "secret".to_string(),
        };

        let exported = export_postman_collection(&collection, &[], &[request]);
        let reimported = crate::import::parse_postman_collection(&exported).unwrap();

        match &reimported.requests[0].auth {
            AuthConfig::Basic { username, password } => {
                assert_eq!(username, "alice");
                assert_eq!(password, "secret");
            }
            other => panic!("expected AuthConfig::Basic, got {other:?}"),
        }
    }

    #[test]
    fn jwt_auth_round_trips_through_export_and_reimport() {
        let collection = Collection::new("Sample API".to_string());
        let mut request = Request::new(collection.id, "Get users".to_string());
        request.auth = AuthConfig::Jwt(api_client::JwtAuthConfig {
            algorithm: api_client::JwtAlgorithm::HS384,
            secret: "changeit".to_string(),
            is_secret_base64_encoded: true,
            payload: r#"{"sub":"user-1"}"#.to_string(),
            header_prefix: "Bearer".to_string(),
            add_to_query_param: true,
            query_param_key: "token".to_string(),
        });

        let exported = export_postman_collection(&collection, &[], &[request]);
        let reimported = crate::import::parse_postman_collection(&exported).unwrap();

        match &reimported.requests[0].auth {
            AuthConfig::Jwt(config) => {
                assert_eq!(config.algorithm, api_client::JwtAlgorithm::HS384);
                assert_eq!(config.secret, "changeit");
                assert!(config.is_secret_base64_encoded);
                assert_eq!(config.payload, r#"{"sub":"user-1"}"#);
                assert!(config.add_to_query_param);
                assert_eq!(config.query_param_key, "token");
            }
            other => panic!("expected AuthConfig::Jwt, got {other:?}"),
        }
    }

    #[test]
    fn an_inherited_auth_config_exports_with_no_auth_key_and_reimports_as_inherit() {
        let collection = Collection::new("Sample API".to_string());
        let request = Request::new(collection.id, "Get users".to_string());
        assert!(matches!(request.auth, AuthConfig::Inherit));

        let exported = export_postman_collection(&collection, &[], &[request]);
        assert!(
            !serde_json::from_str::<Value>(&exported).unwrap()["item"][0]["request"]
                .as_object()
                .unwrap()
                .contains_key("auth")
        );

        let reimported = crate::import::parse_postman_collection(&exported).unwrap();
        assert!(matches!(reimported.requests[0].auth, AuthConfig::Inherit));
    }

    #[test]
    fn pre_request_and_test_scripts_round_trip_through_export_and_reimport() {
        let collection = Collection::new("Sample API".to_string());
        let mut request = Request::new(collection.id, "Get users".to_string());
        request.pre_request_script = "pm.environment.set(\"x\", 1);".to_string();
        request.test_script = "pm.test(\"ok\", () => {});".to_string();

        let exported = export_postman_collection(&collection, &[], &[request]);
        let reimported = crate::import::parse_postman_collection(&exported).unwrap();

        assert_eq!(
            reimported.requests[0].pre_request_script,
            "pm.environment.set(\"x\", 1);"
        );
        assert_eq!(
            reimported.requests[0].test_script,
            "pm.test(\"ok\", () => {});"
        );
    }

    #[test]
    fn a_urlencoded_body_round_trips_through_export_and_reimport() {
        let collection = Collection::new("Sample API".to_string());
        let mut request = Request::new(collection.id, "Login".to_string());
        request.body = RequestBody::UrlEncoded(vec![(
            "grant_type".to_string(),
            "client_credentials".to_string(),
        )]);

        let exported = export_postman_collection(&collection, &[], &[request]);
        let reimported = crate::import::parse_postman_collection(&exported).unwrap();

        match &reimported.requests[0].body {
            RequestBody::UrlEncoded(pairs) => assert_eq!(
                pairs,
                &vec![("grant_type".to_string(), "client_credentials".to_string())]
            ),
            other => panic!("expected RequestBody::UrlEncoded, got {other:?}"),
        }
    }

    #[test]
    fn a_graphql_body_round_trips_through_export_and_reimport() {
        let collection = Collection::new("Sample API".to_string());
        let mut request = Request::new(collection.id, "Query users".to_string());
        request.body = RequestBody::GraphQl {
            query: "query { users { id } }".to_string(),
            variables: "{}".to_string(),
        };

        let exported = export_postman_collection(&collection, &[], &[request]);
        let reimported = crate::import::parse_postman_collection(&exported).unwrap();

        match &reimported.requests[0].body {
            RequestBody::GraphQl { query, variables } => {
                assert_eq!(query, "query { users { id } }");
                assert_eq!(variables, "{}");
            }
            other => panic!("expected RequestBody::GraphQl, got {other:?}"),
        }
    }

    #[test]
    fn an_environment_round_trips_through_export_and_reimport_preserving_secret_type() {
        let mut environment = Environment::new("Staging".to_string());
        environment.variables.push(Variable::new(
            "base_url".to_string(),
            "https://staging.example.com".to_string(),
        ));
        let mut secret = Variable::new("api_key".to_string(), "shh".to_string());
        secret.secret = true;
        environment.variables.push(secret);

        let exported = export_postman_environment(&environment);
        let reimported = crate::import::parse_postman_environment(&exported).unwrap();

        assert_eq!(reimported.name, "Staging");
        assert_eq!(reimported.variables.len(), 2);
        let base_url = reimported
            .variables
            .iter()
            .find(|v| v.key == "base_url")
            .unwrap();
        assert!(!base_url.secret);
        assert_eq!(base_url.current_value, "https://staging.example.com");
        let api_key = reimported
            .variables
            .iter()
            .find(|v| v.key == "api_key")
            .unwrap();
        assert!(api_key.secret);
        assert_eq!(api_key.current_value, "shh");
    }
}
