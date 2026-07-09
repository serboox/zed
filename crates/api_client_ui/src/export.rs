use api_client::{Collection, Environment, Folder, FolderId, HttpMethod, Request, RequestBody};
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

fn body_text(body: &RequestBody) -> String {
    match body {
        RequestBody::Raw { text, .. } => text.clone(),
        _ => String::new(),
    }
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
    json!({
        "name": request.name,
        "request": {
            "method": method,
            "url": { "raw": request.url },
            "header": headers_to_postman(&request.headers),
            "body": { "mode": "raw", "raw": body_text(&request.body) },
        },
        "response": examples_to_postman(request),
    })
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
