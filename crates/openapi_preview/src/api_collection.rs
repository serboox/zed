use api_client::{
    Collection, CollectionId, Folder, FolderId, Header, HttpMethod as ApiHttpMethod, QueryParam,
    RawBodyContentType, Request, RequestBody as ApiRequestBody, Variable, rewrite_path_template,
};
use gpui::SharedString;

use crate::openapi_document::{HttpMethod as OpenApiHttpMethod, OpenApiDocument, Operation};

const BASE_URL_VARIABLE: &str = "baseUrl";

/// Which operations `collection_from_document` turns into requests.
#[derive(Debug, Clone)]
pub enum OperationSelection {
    AllOperations,
    /// Only the operation whose `Operation::key()` matches.
    SingleOperation(SharedString),
}

impl OperationSelection {
    fn includes(&self, operation: &Operation) -> bool {
        match self {
            OperationSelection::AllOperations => true,
            OperationSelection::SingleOperation(key) => operation.key() == *key,
        }
    }
}

/// A collection built from an OpenAPI/Swagger contract: a `Collection` plus
/// every `Folder`/`Request` in it, with ids already assigned and
/// `parent_id`/`folder_id`/`order` already wired -- ready to be pushed
/// straight into `ApiClientStore` (either wholesale via `import_collection`,
/// or merged request-by-request into an existing collection).
pub struct ImportedCollection {
    pub collection: Collection,
    pub folders: Vec<Folder>,
    pub requests: Vec<Request>,
}

/// Converts a parsed OpenAPI/Swagger contract into an API-client collection.
/// One folder is created per tag (in the document's own group order); an
/// operation with no tag is placed directly in the collection. The document's
/// first server URL becomes the `baseUrl` collection variable, and every
/// request URL is `{{baseUrl}}` followed by the operation's path, keeping any
/// `{name}` path placeholders as-is; each distinct placeholder also becomes
/// its own collection variable so it has one place to be filled in.
pub fn collection_from_document(
    document: &OpenApiDocument,
    selection: OperationSelection,
) -> ImportedCollection {
    let mut collection = Collection::new(collection_name(document));
    collection.description = collection_description(document);

    let base_url = document
        .base_urls
        .first()
        .map(|url| url.to_string())
        .unwrap_or_default();
    collection
        .variables
        .push(Variable::new(BASE_URL_VARIABLE.to_string(), base_url));

    let mut folders = Vec::new();
    let mut requests = Vec::new();
    let mut path_variable_names: Vec<String> = Vec::new();
    let mut root_order = 0i64;

    for group in &document.groups {
        let operations: Vec<&Operation> = group
            .operations
            .iter()
            .filter(|operation| selection.includes(operation))
            .collect();
        if operations.is_empty() {
            continue;
        }

        let folder_id = if !group.tagged {
            None
        } else {
            let folder = Folder::new(collection.id, group.name.to_string(), None, root_order);
            root_order += 1;
            let id = folder.id;
            folders.push(folder);
            Some(id)
        };

        for (index, operation) in operations.into_iter().enumerate() {
            // Requests inside a folder are their own sibling group starting
            // at 0; requests with no folder share the collection's own root
            // sibling order with the folders created above.
            let order = match folder_id {
                Some(_) => index as i64,
                None => {
                    let order = root_order;
                    root_order += 1;
                    order
                }
            };
            requests.push(build_request(
                document,
                operation,
                collection.id,
                folder_id,
                order,
                &mut path_variable_names,
            ));
        }
    }

    for name in path_variable_names {
        collection
            .variables
            .push(Variable::new(name, String::new()));
    }

    ImportedCollection {
        collection,
        folders,
        requests,
    }
}

fn collection_name(document: &OpenApiDocument) -> String {
    let title = document.title.trim();
    if title.is_empty() {
        "Imported API".to_string()
    } else {
        title.to_string()
    }
}

fn collection_description(document: &OpenApiDocument) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(version) = &document.version {
        parts.push(format!("Version {version}"));
    }
    if let Some(description) = &document.description {
        parts.push(description.to_string());
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

fn base_url_placeholder() -> String {
    ["{{", BASE_URL_VARIABLE, "}}"].concat()
}

fn build_request(
    document: &OpenApiDocument,
    operation: &Operation,
    collection_id: CollectionId,
    folder_id: Option<FolderId>,
    order: i64,
    path_variable_names: &mut Vec<String>,
) -> Request {
    let name = operation
        .summary
        .as_ref()
        .map(|summary| summary.to_string())
        .unwrap_or_else(|| format!("{} {}", operation.method.label(), operation.path));

    let mut request = Request::new(collection_id, name);
    request.folder_id = folder_id;
    request.order = order;
    request.description = operation
        .description
        .as_ref()
        .map(|description| description.to_string());
    request.method = map_http_method(operation.method);
    request.url = format!(
        "{}{}",
        base_url_placeholder(),
        rewrite_path_template(&operation.path)
    );

    for path_parameter in path_parameter_names(&operation.path) {
        if !path_variable_names.contains(&path_parameter) {
            path_variable_names.push(path_parameter);
        }
    }

    for parameter in &operation.parameters {
        match parameter.location.as_ref() {
            "query" => request.params.push(QueryParam {
                key: parameter.name.to_string(),
                value: String::new(),
                enabled: parameter.required,
                description: None,
            }),
            "header" => request.headers.push(Header {
                key: parameter.name.to_string(),
                value: String::new(),
                enabled: parameter.required,
                description: None,
            }),
            // Path parameters are captured by `path_parameter_names` instead;
            // cookie parameters have no representation in `Request` yet.
            _ => {}
        }
    }

    if let Some(body) = &operation.request_body {
        if let Some(content_type) = json_content_type(&body.content_types) {
            let skeleton = json_skeleton(document, body.type_label.as_ref());
            let text = serde_json::to_string_pretty(&skeleton).unwrap_or_default();
            request.body = ApiRequestBody::Raw {
                content_type: RawBodyContentType::Json,
                text,
            };
            // The document may declare its own Content-Type parameter, which
            // arrives here as an empty header row. Sending is literal about
            // headers, so the media type has to be written into it either way.
            match request
                .headers
                .iter_mut()
                .find(|header| header.key.eq_ignore_ascii_case("content-type"))
            {
                Some(header) => {
                    header.value = content_type.to_string();
                    header.enabled = true;
                }
                None => request.headers.push(Header {
                    key: "Content-Type".to_string(),
                    value: content_type.to_string(),
                    enabled: true,
                    description: None,
                }),
            }
        }
    }

    request
}

fn map_http_method(method: OpenApiHttpMethod) -> ApiHttpMethod {
    match method {
        OpenApiHttpMethod::Get => ApiHttpMethod::Get,
        OpenApiHttpMethod::Post => ApiHttpMethod::Post,
        OpenApiHttpMethod::Put => ApiHttpMethod::Put,
        OpenApiHttpMethod::Patch => ApiHttpMethod::Patch,
        OpenApiHttpMethod::Delete => ApiHttpMethod::Delete,
        OpenApiHttpMethod::Head => ApiHttpMethod::Head,
        OpenApiHttpMethod::Options => ApiHttpMethod::Options,
        OpenApiHttpMethod::Trace => ApiHttpMethod::Custom("TRACE".to_string()),
    }
}

/// Extracts every `{name}` placeholder from an OpenAPI path template, in
/// the order they appear.
fn path_parameter_names(path: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut chars = path.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '{' {
            continue;
        }
        let mut name = String::new();
        for inner in chars.by_ref() {
            if inner == '}' {
                break;
            }
            name.push(inner);
        }
        if !name.is_empty() {
            names.push(name);
        }
    }
    names
}

fn json_content_type(content_types: &[SharedString]) -> Option<&SharedString> {
    content_types
        .iter()
        .find(|content_type| content_type.to_lowercase().contains("json"))
}

/// Builds a `{ "property": null, ... }` skeleton from the named schema's
/// property list, falling back to an empty object when the type label does
/// not resolve to a known schema (an array, a composite, a primitive, or an
/// unresolvable reference).
fn json_skeleton(
    document: &OpenApiDocument,
    type_label: Option<&SharedString>,
) -> serde_json::Value {
    let mut object = serde_json::Map::new();
    if let Some(type_label) = type_label {
        if let Some(schema) = document
            .schemas
            .iter()
            .find(|schema| schema.name.as_ref() == type_label.as_ref())
        {
            for (property, _) in &schema.properties {
                object.insert(property.to_string(), serde_json::Value::Null);
            }
        }
    }
    serde_json::Value::Object(object)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openapi_document::parse;
    use api_client::{
        Environment, ResolveMode, SystemDynamicVariableSource, VariableContext, resolve,
    };

    /// Sends a saved request's URL through the same substitution the client
    /// itself uses, so a token this module writes is proven to be one the
    /// client can actually fill in.
    fn resolved_url(collection: &Collection, url: &str, filled: &[(&str, &str)]) -> String {
        let mut collection = collection.clone();
        for (key, value) in filled {
            if let Some(variable) = collection
                .variables
                .iter_mut()
                .find(|variable| variable.key == *key)
            {
                variable.current_value = (*value).to_string();
            }
        }
        let global = Environment::global();
        let context = VariableContext {
            environment: None,
            collection: Some(&collection),
            global: &global,
        };
        resolve(
            url,
            &context,
            &SystemDynamicVariableSource,
            ResolveMode::ForSend,
        )
    }

    const SAMPLE_DOCUMENT: &str = r#"
openapi: 3.0.3
info:
  title: Sample Store API
  version: 2.1.0
  description: A small sample API.
servers:
  - url: https://api.example.com/v1
tags:
  - name: pets
    description: Everything about pets
  - name: store
    description: Store operations
paths:
  /pets/{petId}:
    get:
      tags: [pets]
      summary: Get a pet
      parameters:
        - name: petId
          in: path
          required: true
          schema:
            type: integer
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
      tags: [pets]
      operationId: updatePet
      requestBody:
        required: true
        content:
          application/json:
            schema:
              $ref: '#/components/schemas/Pet'
      responses:
        '200':
          description: ok
  /store/inventory:
    get:
      tags: [store]
      summary: Get inventory
      responses:
        '200':
          description: ok
  /ping:
    get:
      summary: Health check
      responses:
        '200':
          description: ok
components:
  schemas:
    Pet:
      type: object
      properties:
        id:
          type: integer
        name:
          type: string
"#;

    #[test]
    fn whole_document_conversion_creates_one_folder_per_tag_and_a_base_url_variable() {
        let document = parse(SAMPLE_DOCUMENT).expect("parse");
        let imported = collection_from_document(&document, OperationSelection::AllOperations);

        assert_eq!(imported.collection.name, "Sample Store API");
        let description = imported
            .collection
            .description
            .clone()
            .expect("description");
        assert!(description.contains("2.1.0"));
        assert!(description.contains("A small sample API."));

        let base_url = imported
            .collection
            .variables
            .iter()
            .find(|variable| variable.key == "baseUrl")
            .expect("baseUrl variable");
        assert_eq!(base_url.initial_value, "https://api.example.com/v1");

        let path_variable = imported
            .collection
            .variables
            .iter()
            .find(|variable| variable.key == "petId")
            .expect("petId variable");
        assert_eq!(path_variable.initial_value, "");

        let folder_names: Vec<&str> = imported
            .folders
            .iter()
            .map(|folder| folder.name.as_str())
            .collect();
        assert_eq!(folder_names, vec!["pets", "store"]);

        assert_eq!(imported.requests.len(), 4);

        let get_pet = imported
            .requests
            .iter()
            .find(|request| request.name == "Get a pet")
            .expect("Get a pet");
        assert_eq!(get_pet.url, "{{baseUrl}}/pets/{{petId}}");
        assert_eq!(get_pet.method, api_client::HttpMethod::Get);
        let pets_folder = imported
            .folders
            .iter()
            .find(|folder| folder.name == "pets")
            .expect("pets folder");
        assert_eq!(get_pet.folder_id, Some(pets_folder.id));

        let verbose = get_pet
            .params
            .iter()
            .find(|param| param.key == "verbose")
            .expect("verbose query param");
        assert!(
            !verbose.enabled,
            "an optional parameter must start disabled"
        );
        assert_eq!(verbose.value, "");

        let trace_id = get_pet
            .headers
            .iter()
            .find(|header| header.key == "X-Trace-Id")
            .expect("X-Trace-Id header");
        assert!(trace_id.enabled, "a required parameter must start enabled");

        assert_eq!(
            resolved_url(
                &imported.collection,
                &get_pet.url,
                &[("baseUrl", "https://api.example.com/v1"), ("petId", "42")]
            ),
            "https://api.example.com/v1/pets/42",
            "every variable this module writes has to be one the client can fill in"
        );

        let update_pet = imported
            .requests
            .iter()
            .find(|request| request.name == "POST /pets/{petId}")
            .expect("a summary-less operation falls back to METHOD /path");
        match &update_pet.body {
            api_client::RequestBody::Raw { content_type, text } => {
                assert_eq!(*content_type, api_client::RawBodyContentType::Json);
                let value: serde_json::Value = serde_json::from_str(text).expect("valid json");
                assert_eq!(value["id"], serde_json::Value::Null);
                assert_eq!(value["name"], serde_json::Value::Null);
            }
            other => panic!("expected a Raw JSON body, got {other:?}"),
        }
        assert!(
            update_pet
                .headers
                .iter()
                .any(|header| header.key == "Content-Type" && header.value == "application/json")
        );

        let ping = imported
            .requests
            .iter()
            .find(|request| request.name == "Health check")
            .expect("Health check");
        assert!(
            ping.folder_id.is_none(),
            "an untagged operation must sit directly in the collection"
        );
    }

    #[test]
    fn single_operation_selection_produces_only_that_request_and_its_folder() {
        let document = parse(SAMPLE_DOCUMENT).expect("parse");
        let get_pet_key = document.groups[0].operations[0].key();

        let imported =
            collection_from_document(&document, OperationSelection::SingleOperation(get_pet_key));

        assert_eq!(imported.requests.len(), 1);
        assert_eq!(imported.folders.len(), 1);
        assert_eq!(imported.folders[0].name, "pets");
        assert_eq!(imported.requests[0].folder_id, Some(imported.folders[0].id));
        assert_eq!(imported.requests[0].name, "Get a pet");
    }

    #[test]
    fn an_operation_with_no_tag_is_placed_directly_in_the_collection() {
        let document = parse(SAMPLE_DOCUMENT).expect("parse");
        let ping_operation = document
            .groups
            .iter()
            .flat_map(|group| &group.operations)
            .find(|operation| operation.path.as_ref() == "/ping")
            .expect("ping operation")
            .key();

        let imported = collection_from_document(
            &document,
            OperationSelection::SingleOperation(ping_operation),
        );

        assert_eq!(imported.requests.len(), 1);
        assert!(imported.folders.is_empty());
        assert!(imported.requests[0].folder_id.is_none());
    }

    #[test]
    fn a_document_with_no_servers_leaves_the_base_url_variable_empty() {
        let document = parse(
            r#"
openapi: 3.0.3
info:
  title: No Server API
  version: "1"
paths:
  /ping:
    get:
      responses:
        '200':
          description: ok
"#,
        )
        .expect("parse");
        assert!(document.base_urls.is_empty());

        let imported = collection_from_document(&document, OperationSelection::AllOperations);
        let base_url = imported
            .collection
            .variables
            .iter()
            .find(|variable| variable.key == "baseUrl")
            .expect("baseUrl variable");
        assert_eq!(base_url.initial_value, "");
        assert_eq!(imported.requests[0].url, "{{baseUrl}}/ping");
    }

    #[test]
    fn request_body_falls_back_to_an_empty_object_when_the_schema_cannot_be_resolved() {
        let document = parse(
            r#"
openapi: 3.0.3
info:
  title: Unresolvable Body API
  version: "1"
paths:
  /items:
    post:
      requestBody:
        required: true
        content:
          application/json:
            schema:
              type: array
              items:
                type: string
      responses:
        '200':
          description: ok
"#,
        )
        .expect("parse");

        let imported = collection_from_document(&document, OperationSelection::AllOperations);
        match &imported.requests[0].body {
            api_client::RequestBody::Raw { content_type, text } => {
                assert_eq!(*content_type, api_client::RawBodyContentType::Json);
                let value: serde_json::Value = serde_json::from_str(text).expect("valid json");
                assert_eq!(value, serde_json::json!({}));
            }
            other => panic!("expected a Raw JSON body, got {other:?}"),
        }
    }
}
