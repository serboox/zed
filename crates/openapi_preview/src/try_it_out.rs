use api_client::{
    Collection, Environment, Header, RawBodyContentType, Request, RequestBody, ResolveMode,
    ResolvedRequest, SystemDynamicVariableSource, VariableContext, build_resolved_request, resolve,
};

use crate::api_collection::{BASE_URL_VARIABLE, path_parameter_names};
use crate::openapi_document::Parameter;

/// Above this many bytes a response body is truncated before it is rendered,
/// so a huge response cannot freeze the UI. Chosen to comfortably fit a
/// typical JSON error/list payload while still being cheap to pretty-print.
pub const MAX_RESPONSE_BODY_BYTES: usize = 256 * 1024;

/// One value the reader typed into a parameter field of the "Try it out"
/// panel, keyed by the same name/location pair the parsed operation declares.
#[derive(Debug, Clone)]
pub struct ParameterOverride {
    pub name: String,
    pub location: String,
    pub required: bool,
    pub value: String,
}

/// Everything the "Try it out" panel lets the reader override before a
/// single operation is sent.
#[derive(Debug, Clone)]
pub struct TryItOutOverrides {
    pub server_url: String,
    /// Read once when a request is sent and folded into the `Authorization`
    /// header of the returned `Request` -- never written into `Collection`,
    /// `Request`, or anything else that could later be persisted or logged.
    pub auth_header_value: String,
    pub body_text: Option<String>,
    pub parameters: Vec<ParameterOverride>,
}

/// Applies the reader's typed-in values to the `Request`/`Collection` pair
/// `collection_from_document` built for a single operation: the server URL
/// and every path parameter become collection-variable overrides (they are
/// substituted through `{{...}}` tokens), query and header parameters are
/// written directly onto the matching row, and the body/auth header replace
/// whatever placeholder was there. The result is ready for
/// `resolve_and_build`.
pub fn apply_overrides(
    mut request: Request,
    mut collection: Collection,
    overrides: &TryItOutOverrides,
) -> (Request, Collection) {
    if let Some(base_url) = collection
        .variables
        .iter_mut()
        .find(|variable| variable.key == BASE_URL_VARIABLE)
    {
        base_url.current_value = overrides.server_url.clone();
    }

    for parameter in &overrides.parameters {
        // A required parameter is always sent, even left blank, so a missing
        // value surfaces as a server-side validation error instead of being
        // silently dropped; an optional one is only sent once the reader
        // actually typed something into it.
        let enabled = parameter.required || !parameter.value.is_empty();
        match parameter.location.as_str() {
            "path" => {
                if let Some(variable) = collection
                    .variables
                    .iter_mut()
                    .find(|variable| variable.key == parameter.name)
                {
                    variable.current_value = parameter.value.clone();
                }
            }
            "query" => {
                if let Some(row) = request
                    .params
                    .iter_mut()
                    .find(|row| row.key == parameter.name)
                {
                    row.value = parameter.value.clone();
                    row.enabled = enabled;
                }
            }
            "header" => {
                if let Some(row) = request
                    .headers
                    .iter_mut()
                    .find(|row| row.key.eq_ignore_ascii_case(&parameter.name))
                {
                    row.value = parameter.value.clone();
                    row.enabled = enabled;
                }
            }
            // Cookie parameters have no representation in `Request`, matching
            // `api_collection::build_request`.
            _ => {}
        }
    }

    if let Some(body_text) = &overrides.body_text {
        let content_type = match &request.body {
            RequestBody::Raw { content_type, .. } => *content_type,
            _ => RawBodyContentType::Json,
        };
        request.body = RequestBody::Raw {
            content_type,
            text: body_text.clone(),
        };
    }

    let auth_value = overrides.auth_header_value.trim();
    if !auth_value.is_empty() {
        match request
            .headers
            .iter_mut()
            .find(|header| header.key.eq_ignore_ascii_case("authorization"))
        {
            Some(header) => {
                header.value = auth_value.to_string();
                header.enabled = true;
            }
            None => request.headers.push(Header {
                key: "Authorization".to_string(),
                value: auth_value.to_string(),
                enabled: true,
                description: None,
            }),
        }
    }

    (request, collection)
}

/// Resolves every `{{token}}` in `request` against `collection`'s variables
/// (no environment applies to a one-off "Try it out" send) and builds the
/// concrete request ready for `api_client::execute`.
pub fn resolve_and_build(request: &Request, collection: &Collection) -> ResolvedRequest {
    let global = Environment::global();
    let context = VariableContext {
        environment: None,
        collection: Some(collection),
        global: &global,
    };
    let dynamic = SystemDynamicVariableSource;
    let resolve_token = |text: &str| resolve(text, &context, &dynamic, ResolveMode::ForSend);
    build_resolved_request(request, &resolve_token)
}

/// Whether a value typed for this parameter can reach the request at all.
/// A location this module does not carry, or a path parameter the path template
/// never mentions, would take input and quietly drop it -- so it is not offered.
pub fn parameter_is_fillable(location: &str, name: &str, path: &str) -> bool {
    match location {
        "query" | "header" => true,
        "path" => path_parameter_names(path).iter().any(|found| found == name),
        _ => false,
    }
}

/// Renders a response body for display: pretty-printed when it parses as
/// JSON, the raw (lossily-decoded) text otherwise. `body` is capped to
/// `MAX_RESPONSE_BODY_BYTES` first; a truncated body is never pretty-printed
/// since a cut-off JSON document would either fail to parse (misleadingly
/// implying the response is not JSON) or, worse, happen to parse into
/// something that isn't actually the full response. Returns the rendered
/// text plus whether the body was truncated.
pub fn cap_and_render_body(body: &[u8], content_type: &str) -> (String, bool) {
    let truncated = body.len() > MAX_RESPONSE_BODY_BYTES;
    let shown = if truncated {
        // Cutting on a byte count can land in the middle of a character, which
        // would show up as a stray replacement glyph at the end of the text.
        let capped = &body[..MAX_RESPONSE_BODY_BYTES];
        match std::str::from_utf8(capped) {
            Ok(_) => capped,
            Err(error) => &capped[..error.valid_up_to()],
        }
    } else {
        body
    };
    let text = String::from_utf8_lossy(shown).into_owned();

    if !truncated {
        let looks_like_json =
            content_type.contains("json") || text.trim_start().starts_with(['{', '[']);
        if looks_like_json
            && let Ok(value) = serde_json::from_str::<serde_json::Value>(&text)
            && let Ok(pretty) = serde_json::to_string_pretty(&value)
        {
            return (pretty, truncated);
        }
    }
    (text, truncated)
}

/// Every `{name}` path-template placeholder in `path` that `declared_parameters`
/// does not already cover as a `path`-location parameter. Some contracts
/// declare a shared path parameter only on one HTTP method instead of at the
/// path-item level; `api_collection::build_request`'s URL templating still
/// expects that token to be filled in for every method on the path, so a
/// "Try it out" panel needs a field for it even when the parsed operation
/// itself does not declare it.
pub fn undeclared_path_parameter_names(
    path: &str,
    declared_parameters: &[Parameter],
) -> Vec<String> {
    path_parameter_names(path)
        .into_iter()
        .filter(|name| {
            !declared_parameters.iter().any(|parameter| {
                parameter.location.as_ref() == "path" && parameter.name.as_ref() == name
            })
        })
        .collect()
}

/// Formats a byte count the way a response inspector should: plain bytes
/// below 1 KB, otherwise one decimal of KB.
pub fn format_response_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_collection::{OperationSelection, collection_from_document};
    use crate::openapi_document::parse;

    const SAMPLE_DOCUMENT: &str = r#"
openapi: 3.0.3
info:
  title: Try It Out API
  version: "1.0.0"
servers:
  - url: https://api.example.com/v1
paths:
  /items/{itemId}:
    get:
      operationId: getItem
      parameters:
        - name: itemId
          in: path
          required: true
          schema:
            type: string
        - name: verbose
          in: query
          required: false
          schema:
            type: boolean
        - name: X-Trace-Id
          in: header
          required: true
          schema:
            type: string
      responses:
        '200':
          description: ok
    post:
      operationId: updateItem
      requestBody:
        required: true
        content:
          application/json:
            schema:
              type: object
      responses:
        '200':
          description: ok
"#;

    fn base_request_and_collection(operation_key: &str) -> (Request, Collection) {
        let document = parse(SAMPLE_DOCUMENT).expect("parse");
        let key = document
            .groups
            .iter()
            .flat_map(|group| &group.operations)
            .find(|operation| operation.key() == operation_key)
            .expect("operation")
            .key();
        let imported =
            collection_from_document(&document, OperationSelection::SingleOperation(key));
        let request = imported.requests.into_iter().next().expect("one request");
        (request, imported.collection)
    }

    #[test]
    fn filled_in_values_flow_through_to_the_final_resolved_request() {
        let (request, collection) = base_request_and_collection("GET /items/{itemId}");
        let overrides = TryItOutOverrides {
            server_url: "https://staging.example.com/v1".to_string(),
            auth_header_value: "Bearer secret-token".to_string(),
            body_text: None,
            parameters: vec![
                ParameterOverride {
                    name: "itemId".to_string(),
                    location: "path".to_string(),
                    required: true,
                    value: "42".to_string(),
                },
                ParameterOverride {
                    name: "verbose".to_string(),
                    location: "query".to_string(),
                    required: false,
                    value: "true".to_string(),
                },
                ParameterOverride {
                    name: "X-Trace-Id".to_string(),
                    location: "header".to_string(),
                    required: true,
                    value: "trace-abc".to_string(),
                },
            ],
        };

        let (request, collection) = apply_overrides(request, collection, &overrides);
        let resolved = resolve_and_build(&request, &collection);

        assert_eq!(
            resolved.url,
            "https://staging.example.com/v1/items/42?verbose=true"
        );
        assert!(
            resolved
                .headers
                .contains(&("X-Trace-Id".to_string(), "trace-abc".to_string()))
        );
        assert!(resolved.headers.contains(&(
            "Authorization".to_string(),
            "Bearer secret-token".to_string()
        )));
    }

    #[test]
    fn an_empty_optional_parameter_is_left_out_but_an_empty_required_one_is_still_sent() {
        let (request, collection) = base_request_and_collection("GET /items/{itemId}");
        let overrides = TryItOutOverrides {
            server_url: "https://api.example.com/v1".to_string(),
            auth_header_value: String::new(),
            body_text: None,
            parameters: vec![
                ParameterOverride {
                    name: "itemId".to_string(),
                    location: "path".to_string(),
                    required: true,
                    value: "1".to_string(),
                },
                ParameterOverride {
                    name: "verbose".to_string(),
                    location: "query".to_string(),
                    required: false,
                    value: String::new(),
                },
                ParameterOverride {
                    name: "X-Trace-Id".to_string(),
                    location: "header".to_string(),
                    required: true,
                    value: String::new(),
                },
            ],
        };

        let (request, collection) = apply_overrides(request, collection, &overrides);
        let resolved = resolve_and_build(&request, &collection);

        assert!(
            !resolved.url.contains("verbose"),
            "an untouched optional parameter must not be sent: {}",
            resolved.url
        );
        assert!(
            resolved
                .headers
                .contains(&("X-Trace-Id".to_string(), String::new())),
            "a required parameter is still sent even when left blank"
        );
        assert!(
            !resolved
                .headers
                .iter()
                .any(|(key, _)| key.eq_ignore_ascii_case("authorization")),
            "an empty auth field must not add an Authorization header"
        );
    }

    #[test]
    fn a_body_override_replaces_the_skeleton_and_keeps_the_json_content_type() {
        let (request, collection) = base_request_and_collection("POST /items/{itemId}");
        let overrides = TryItOutOverrides {
            server_url: "https://api.example.com/v1".to_string(),
            auth_header_value: String::new(),
            body_text: Some(r#"{"name":"Widget"}"#.to_string()),
            parameters: vec![ParameterOverride {
                name: "itemId".to_string(),
                location: "path".to_string(),
                required: true,
                value: "7".to_string(),
            }],
        };

        let (request, collection) = apply_overrides(request, collection, &overrides);
        let resolved = resolve_and_build(&request, &collection);

        assert_eq!(resolved.url, "https://api.example.com/v1/items/7");
        assert_eq!(resolved.body, Some(br#"{"name":"Widget"}"#.to_vec()));
        assert!(
            resolved
                .headers
                .contains(&("Content-Type".to_string(), "application/json".to_string()))
        );
    }

    #[test]
    fn valid_json_within_the_limit_is_pretty_printed() {
        let (text, truncated) = cap_and_render_body(br#"{"a":1}"#, "application/json");
        assert!(!truncated);
        assert_eq!(text, "{\n  \"a\": 1\n}");
    }

    #[test]
    fn a_body_over_the_limit_is_truncated_and_never_pretty_printed() {
        let huge = vec![b'a'; MAX_RESPONSE_BODY_BYTES + 10];
        let (text, truncated) = cap_and_render_body(&huge, "text/plain");
        assert!(truncated);
        assert_eq!(text.len(), MAX_RESPONSE_BODY_BYTES);
    }

    #[test]
    fn truncation_lands_on_a_character_boundary() {
        // The last character straddles the cap, so a byte-count cut would slice
        // it in half and leave a replacement glyph behind.
        let mut body = vec![b'a'; MAX_RESPONSE_BODY_BYTES - 1];
        body.extend_from_slice("é".as_bytes());
        body.extend_from_slice(b"tail");

        let (text, truncated) = cap_and_render_body(&body, "text/plain");
        assert!(truncated);
        assert!(
            !text.contains('\u{fffd}'),
            "a cut in the middle of a character must not reach the reader"
        );
        assert_eq!(text.len(), MAX_RESPONSE_BODY_BYTES - 1);
    }

    #[test]
    fn a_parameter_the_path_never_mentions_is_not_offered() {
        assert!(parameter_is_fillable("query", "verbose", "/pets"));
        assert!(parameter_is_fillable("header", "X-Trace-Id", "/pets"));
        assert!(parameter_is_fillable("path", "petId", "/pets/{petId}"));
        assert!(
            !parameter_is_fillable("path", "petId", "/pets"),
            "a path parameter with no placeholder to fill has nowhere to go"
        );
        assert!(!parameter_is_fillable("cookie", "session", "/pets"));
        assert!(!parameter_is_fillable("formData", "file", "/pets"));
    }

    #[test]
    fn non_json_content_is_shown_as_is() {
        let (text, truncated) = cap_and_render_body(b"plain text", "text/plain");
        assert!(!truncated);
        assert_eq!(text, "plain text");
    }

    #[test]
    fn format_response_size_switches_units_at_one_kilobyte() {
        assert_eq!(format_response_size(512), "512 B");
        assert_eq!(format_response_size(2048), "2.0 KB");
    }

    #[test]
    fn a_path_parameter_declared_on_a_sibling_method_is_still_reported_as_undeclared() {
        // POST has no parameters of its own; `itemId` was only declared on
        // GET, but the path template still needs a value for it.
        let document = parse(SAMPLE_DOCUMENT).expect("parse");
        let post = document
            .groups
            .iter()
            .flat_map(|group| &group.operations)
            .find(|operation| operation.key() == "POST /items/{itemId}")
            .expect("POST /items/{itemId}");
        assert!(post.parameters.is_empty());

        let missing = undeclared_path_parameter_names(&post.path, &post.parameters);
        assert_eq!(missing, vec!["itemId".to_string()]);
    }

    #[test]
    fn a_declared_path_parameter_is_not_reported_as_undeclared() {
        let document = parse(SAMPLE_DOCUMENT).expect("parse");
        let get = document
            .groups
            .iter()
            .flat_map(|group| &group.operations)
            .find(|operation| operation.key() == "GET /items/{itemId}")
            .expect("GET /items/{itemId}");

        let missing = undeclared_path_parameter_names(&get.path, &get.parameters);
        assert!(missing.is_empty());
    }
}
