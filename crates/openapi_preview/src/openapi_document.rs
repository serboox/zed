use gpui::SharedString;
use serde_yaml_ng::{Mapping, Value};

/// HTTP methods an OpenAPI path item can declare, in the order the preview
/// shows them when one path defines several.
const HTTP_METHODS: [&str; 8] = [
    "get", "post", "put", "patch", "delete", "head", "options", "trace",
];

/// Name given to the synthetic group that holds operations with no tag.
/// `pub(crate)` so `api_collection` can recognize this group without
/// duplicating the literal string.
const UNGROUPED: &str = "Other";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
    Head,
    Options,
    Trace,
}

impl HttpMethod {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "get" => Some(Self::Get),
            "post" => Some(Self::Post),
            "put" => Some(Self::Put),
            "patch" => Some(Self::Patch),
            "delete" => Some(Self::Delete),
            "head" => Some(Self::Head),
            "options" => Some(Self::Options),
            "trace" => Some(Self::Trace),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
            Self::Head => "HEAD",
            Self::Options => "OPTIONS",
            Self::Trace => "TRACE",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Parameter {
    pub name: SharedString,
    pub location: SharedString,
    pub required: bool,
    pub type_label: SharedString,
    /// The values an `enum` schema restricts this parameter to, in
    /// declaration order. Empty when the parameter is not an enum.
    pub allowed_values: Vec<SharedString>,
    pub description: Option<SharedString>,
}

#[derive(Debug, Clone)]
pub struct RequestBody {
    pub required: bool,
    pub content_types: Vec<SharedString>,
    pub type_label: Option<SharedString>,
    pub description: Option<SharedString>,
}

#[derive(Debug, Clone)]
pub struct Response {
    pub status: SharedString,
    pub description: Option<SharedString>,
    pub content_types: Vec<SharedString>,
    pub type_label: Option<SharedString>,
}

#[derive(Debug, Clone)]
pub struct Operation {
    pub method: HttpMethod,
    pub path: SharedString,
    pub summary: Option<SharedString>,
    pub description: Option<SharedString>,
    pub operation_id: Option<SharedString>,
    pub deprecated: bool,
    pub secured: bool,
    pub parameters: Vec<Parameter>,
    pub request_body: Option<RequestBody>,
    pub responses: Vec<Response>,
}

impl Operation {
    /// Stable identity used to remember which operations the user expanded.
    /// Method plus path is unique within a document, while `operationId` is
    /// optional and may repeat in hand-written specs.
    pub fn key(&self) -> SharedString {
        format!("{} {}", self.method.label(), self.path).into()
    }
}

#[derive(Debug, Clone)]
pub struct OperationGroup {
    pub name: SharedString,
    pub description: Option<SharedString>,
    pub operations: Vec<Operation>,
    /// False for the bucket that collects operations declaring no tag. Its name
    /// is a plain word a document is free to use as a real tag, so the two cases
    /// cannot be told apart by name.
    pub tagged: bool,
}

#[derive(Debug, Clone)]
pub struct SchemaSummary {
    pub name: SharedString,
    pub type_label: SharedString,
    pub properties: Vec<(SharedString, SharedString)>,
}

/// One entry of the document's `servers:` list -- a "Try it out" request goes
/// to whichever of these the reader has picked from the servers dropdown.
#[derive(Debug, Clone)]
pub struct Server {
    pub url: SharedString,
    pub description: Option<SharedString>,
}

#[derive(Debug, Clone)]
pub struct OpenApiDocument {
    pub spec_label: SharedString,
    pub title: SharedString,
    pub version: Option<SharedString>,
    pub description: Option<SharedString>,
    pub servers: Vec<Server>,
    pub groups: Vec<OperationGroup>,
    pub schemas: Vec<SchemaSummary>,
    /// Things the document does not say. Surfaced in the preview so an empty
    /// section reads as "the spec omits this" rather than "the preview broke".
    pub notes: Vec<SharedString>,
}

impl OpenApiDocument {
    pub fn operation_count(&self) -> usize {
        self.groups.iter().map(|group| group.operations.len()).sum()
    }
}

/// True when `text` looks like an OpenAPI or Swagger contract, which is what
/// decides whether the split preview is offered for a YAML or JSON file. Only
/// the head of the file is inspected: the version key is required to be at the
/// root, and reading megabytes to answer a menu-enabled check is wasteful.
pub fn looks_like_openapi(text: &str) -> bool {
    const SNIFF_LIMIT: usize = 8 * 1024;
    let head = match text.char_indices().nth(SNIFF_LIMIT) {
        Some((index, _)) => &text[..index],
        None => text,
    };
    declares_version_as_yaml(head) || declares_version_as_json(head)
}

/// A YAML root key sits at the start of its line and is followed by a colon.
fn declares_version_as_yaml(head: &str) -> bool {
    head.lines().any(|line| {
        let key = line.trim_start_matches(['"', '\'']);
        (key.starts_with("openapi") || key.starts_with("swagger"))
            && key
                .trim_start_matches(|c: char| c.is_alphanumeric())
                .trim_start_matches(['"', '\''])
                .trim_start()
                .starts_with(':')
    })
}

/// A JSON key is quoted and may sit anywhere, including in the middle of a
/// single very long line -- a minified contract is one line from `{` to `}`, so
/// scanning line beginnings would never find it.
fn declares_version_as_json(head: &str) -> bool {
    ["\"openapi\"", "\"swagger\""].iter().any(|key| {
        head.match_indices(key)
            .any(|(index, _)| head[index + key.len()..].trim_start().starts_with(':'))
    })
}

/// Parses an OpenAPI 3.x or Swagger 2.0 contract into the shape the preview
/// renders. JSON parses too: YAML is a superset of it.
pub fn parse(text: &str) -> anyhow::Result<OpenApiDocument> {
    let root: Value = serde_yaml_ng::from_str(text)?;
    let root = root
        .as_mapping()
        .ok_or_else(|| anyhow::anyhow!("the document root is not a mapping"))?;

    let mut notes = Vec::new();
    let spec_label = spec_label(root, &mut notes);
    let info = mapping_at(root, "info");
    let title = info
        .and_then(|info| string_at(info, "title"))
        .unwrap_or_else(|| "Untitled API".into());
    let version = info.and_then(|info| string_at(info, "version"));
    let description = info
        .and_then(|info| string_at(info, "description"))
        .map(prose);

    let servers = servers(root);
    let schema_definitions = mapping_at(root, "components")
        .and_then(|components| mapping_at(components, "schemas"))
        .or_else(|| mapping_at(root, "definitions"));

    let groups = operation_groups(root, &mut notes);
    let schemas = schema_definitions.map(schema_summaries).unwrap_or_default();

    Ok(OpenApiDocument {
        spec_label,
        title,
        version,
        description,
        servers,
        groups,
        schemas,
        notes,
    })
}

fn spec_label(root: &Mapping, notes: &mut Vec<SharedString>) -> SharedString {
    if let Some(version) = string_at(root, "openapi") {
        return format!("OpenAPI {version}").into();
    }
    if let Some(version) = string_at(root, "swagger") {
        return format!("Swagger {version}").into();
    }
    notes.push("No `openapi` or `swagger` version key at the document root.".into());
    "Unknown specification".into()
}

fn servers(root: &Mapping) -> Vec<Server> {
    if let Some(entries) = sequence_at(root, "servers") {
        let servers: Vec<Server> = entries
            .iter()
            .filter_map(|server| server.as_mapping())
            .filter_map(|server| {
                string_at(server, "url").map(|url| Server {
                    url,
                    description: string_at(server, "description").map(prose),
                })
            })
            .collect();
        if !servers.is_empty() {
            return servers;
        }
    }

    // Swagger 2.0 spells the same thing out in three separate keys.
    let host = string_at(root, "host");
    let base_path = string_at(root, "basePath").unwrap_or_else(|| "".into());
    let Some(host) = host else {
        return Vec::new();
    };
    let schemes = sequence_at(root, "schemes")
        .map(|schemes| {
            schemes
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if schemes.is_empty() {
        return vec![Server {
            url: format!("{host}{base_path}").into(),
            description: None,
        }];
    }
    schemes
        .into_iter()
        .map(|scheme| Server {
            url: format!("{scheme}://{host}{base_path}").into(),
            description: None,
        })
        .collect()
}

/// The server a fresh "Try it out" panel sends to before the reader picks one
/// explicitly: whichever the document lists first.
pub fn default_server(servers: &[Server]) -> Option<&Server> {
    servers.first()
}

/// The server currently in effect for "Try it out": the reader's last pick,
/// as long as the document still lists it (a reparse may have dropped it),
/// otherwise `default_server`.
pub fn resolve_selected_server<'a>(
    servers: &'a [Server],
    selected_url: Option<&SharedString>,
) -> Option<&'a Server> {
    selected_url
        .and_then(|url| servers.iter().find(|server| &server.url == url))
        .or_else(|| default_server(servers))
}

fn operation_groups(root: &Mapping, notes: &mut Vec<SharedString>) -> Vec<OperationGroup> {
    let Some(paths) = mapping_at(root, "paths") else {
        notes.push("The document declares no `paths` section.".into());
        return Vec::new();
    };

    let declared_tags = declared_tag_order(root);
    let mut grouped: Vec<OperationGroup> = declared_tags
        .iter()
        .map(|(name, description)| OperationGroup {
            name: name.clone(),
            description: description.clone(),
            operations: Vec::new(),
            tagged: true,
        })
        .collect();

    let mut operation_count = 0;
    for (path, path_item) in paths {
        let Some(path) = path.as_str() else { continue };
        let Some(path_item) = path_item.as_mapping() else {
            continue;
        };
        let shared_parameter_nodes = parameter_nodes(root, path_item);

        for method_name in HTTP_METHODS {
            let Some(method) = HttpMethod::parse(method_name) else {
                continue;
            };
            let Some(operation) = mapping_at(path_item, method_name) else {
                continue;
            };

            let mut parameter_nodes = shared_parameter_nodes.clone();
            parameter_nodes.extend(parameter_nodes_of(root, operation));
            let tag = first_tag(operation);
            let group_name = tag.clone().unwrap_or_else(|| UNGROUPED.into());

            let operation = Operation {
                method,
                path: path.to_owned().into(),
                summary: string_at(operation, "summary").map(one_line),
                description: string_at(operation, "description").map(prose),
                operation_id: string_at(operation, "operationId"),
                deprecated: bool_at(operation, "deprecated"),
                secured: is_secured(root, operation),
                parameters: describe_parameters(root, &parameter_nodes),
                request_body: request_body(root, operation, &parameter_nodes),
                responses: responses(operation),
            };
            operation_count += 1;

            let tagged = tag.is_some();
            match grouped
                .iter_mut()
                .find(|group| group.name == group_name && group.tagged == tagged)
            {
                Some(group) => group.operations.push(operation),
                None => grouped.push(OperationGroup {
                    name: group_name,
                    description: None,
                    operations: vec![operation],
                    tagged,
                }),
            }
        }
    }

    if operation_count == 0 {
        notes.push("The `paths` section declares no operations.".into());
    }
    grouped.retain(|group| !group.operations.is_empty());
    grouped
}

fn declared_tag_order(root: &Mapping) -> Vec<(SharedString, Option<SharedString>)> {
    sequence_at(root, "tags")
        .map(|tags| {
            tags.iter()
                .filter_map(Value::as_mapping)
                .filter_map(|tag| {
                    string_at(tag, "name")
                        .map(|name| (name, string_at(tag, "description").map(prose)))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn first_tag(operation: &Mapping) -> Option<SharedString> {
    sequence_at(operation, "tags")?
        .iter()
        .find_map(Value::as_str)
        .map(|tag| tag.to_owned().into())
}

/// An operation inherits the path item's parameters, so both levels are
/// collected before anything is classified: Swagger 2.0 may declare the request
/// body at either level.
fn parameter_nodes<'a>(root: &'a Mapping, owner: &'a Mapping) -> Vec<&'a Mapping> {
    parameter_nodes_of(root, owner)
}

fn parameter_nodes_of<'a>(root: &'a Mapping, owner: &'a Mapping) -> Vec<&'a Mapping> {
    sequence_at(owner, "parameters")
        .map(|entries| {
            entries
                .iter()
                .filter_map(Value::as_mapping)
                .map(|entry| resolve_ref(root, entry))
                .collect()
        })
        .unwrap_or_default()
}

/// How many `$ref` hops are followed before giving up. A contract that points a
/// reference back at itself would otherwise never finish resolving.
const MAX_REF_HOPS: usize = 8;

/// Follows a `$ref` into the document's own components. Naming a parameter or a
/// schema once and referencing it everywhere is the norm, and a reference left
/// unresolved reads as a nameless parameter of the wrong kind.
fn resolve_ref<'a>(root: &'a Mapping, node: &'a Mapping) -> &'a Mapping {
    let mut current = node;
    for _ in 0..MAX_REF_HOPS {
        let Some(reference) = string_at(current, "$ref") else {
            return current;
        };
        let Some(target) = follow_pointer(root, reference.as_ref()) else {
            return current;
        };
        current = target;
    }
    current
}

fn follow_pointer<'a>(root: &'a Mapping, reference: &str) -> Option<&'a Mapping> {
    let path = reference.strip_prefix("#/")?;
    let mut current = root;
    for segment in path.split('/') {
        let segment = segment.replace("~1", "/").replace("~0", "~");
        current = mapping_at(current, &segment)?;
    }
    Some(current)
}

fn describe_parameters(root: &Mapping, nodes: &[&Mapping]) -> Vec<Parameter> {
    nodes
        .iter()
        // A body parameter is Swagger 2.0's request body, rendered as one.
        .filter(|parameter| string_at(parameter, "in").as_deref() != Some("body"))
        .map(|parameter| {
            let schema = mapping_at(parameter, "schema");
            // A parameter's schema is itself often a reference, and that is where
            // the values it accepts live.
            let resolved = schema.map(|schema| resolve_ref(root, schema));
            let allowed_values = resolved
                .map(enum_values)
                .unwrap_or_else(|| enum_values(parameter));
            let type_label = match (schema, resolved) {
                // A named enumeration reads as the kind of value it accepts,
                // not as the name it was defined under and not as the word
                // "enum", which says nothing about what to type.
                (_, Some(resolved)) if !allowed_values.is_empty() => {
                    string_at(resolved, "type").unwrap_or_else(|| type_label(resolved))
                }
                (Some(schema), _) => type_label(schema),
                _ => type_label(parameter),
            };
            Parameter {
                name: string_at(parameter, "name").unwrap_or_else(|| "(unnamed)".into()),
                location: string_at(parameter, "in").unwrap_or_else(|| "query".into()),
                required: bool_at(parameter, "required"),
                type_label,
                allowed_values,
                description: string_at(parameter, "description").map(prose),
            }
        })
        .collect()
}

/// The values an `enum` schema restricts its input to, checked at the same
/// level `type_label` reads its type from: an OpenAPI 3 parameter nests them
/// under `schema`, a Swagger 2.0 one declares them directly on the parameter.
fn enum_values(mapping: &Mapping) -> Vec<SharedString> {
    sequence_at(mapping, "enum")
        .map(|values| {
            values
                .iter()
                .filter_map(|value| match value {
                    Value::String(text) => Some(SharedString::from(text.clone())),
                    Value::Number(number) => Some(SharedString::from(number.to_string())),
                    Value::Bool(flag) => Some(SharedString::from(flag.to_string())),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The line Swagger's reference view shows above an enum input, formatted
/// exactly the way it renders it.
pub fn available_values_label(values: &[SharedString]) -> Option<SharedString> {
    if values.is_empty() {
        return None;
    }
    let joined = values
        .iter()
        .map(SharedString::as_ref)
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!("Available values : {joined}").into())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProseSpan {
    Plain(SharedString),
    Code(SharedString),
}

/// Splits Markdown backtick code spans out of `text` so a renderer can style
/// each one separately instead of the backticks reaching the reader as-is.
/// An unterminated backtick is left as plain text, backtick included, since
/// there is no closing mark to make a span out of.
pub fn split_code_spans(text: &SharedString) -> Vec<ProseSpan> {
    if !text.contains('`') {
        return vec![ProseSpan::Plain(text.clone())];
    }

    let source = text.as_ref();
    let mut spans = Vec::new();
    let mut rest = source;
    while let Some(open) = rest.find('`') {
        if open > 0 {
            spans.push(ProseSpan::Plain(rest[..open].to_owned().into()));
        }
        let after_open = &rest[open + 1..];
        let Some(close) = after_open.find('`') else {
            spans.push(ProseSpan::Plain(rest[open..].to_owned().into()));
            rest = "";
            break;
        };
        let code = &after_open[..close];
        if code.is_empty() {
            // An empty pair is not a code span; dropping it would swallow the
            // backticks the author actually wrote.
            spans.push(ProseSpan::Plain("``".into()));
        } else {
            spans.push(ProseSpan::Code(code.to_owned().into()));
        }
        rest = &after_open[close + 1..];
    }
    if !rest.is_empty() {
        spans.push(ProseSpan::Plain(rest.to_owned().into()));
    }
    spans
}

/// An operation without its own `security` inherits the document's, and an
/// explicit empty list is how a spec opts an operation back out of it.
fn is_secured(root: &Mapping, operation: &Mapping) -> bool {
    match sequence_at(operation, "security") {
        Some(requirements) => !requirements.is_empty(),
        None => sequence_at(root, "security").is_some_and(|requirements| !requirements.is_empty()),
    }
}

fn request_body(
    root: &Mapping,
    operation: &Mapping,
    parameter_nodes: &[&Mapping],
) -> Option<RequestBody> {
    if let Some(body) = mapping_at(operation, "requestBody") {
        let (content_types, type_label) = content_summary(mapping_at(body, "content"));
        return Some(RequestBody {
            required: bool_at(body, "required"),
            content_types,
            type_label,
            description: string_at(body, "description"),
        });
    }

    // Swagger 2.0: the body arrives as a parameter, and its media types are
    // declared once for the whole document unless the operation overrides them.
    let body_parameter = parameter_nodes
        .iter()
        .find(|parameter| string_at(parameter, "in").as_deref() == Some("body"))?;
    let content_types = sequence_at(operation, "consumes")
        .or_else(|| sequence_at(root, "consumes"))
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(|value| SharedString::from(value.to_owned()))
                .collect()
        })
        .unwrap_or_default();
    Some(RequestBody {
        required: bool_at(body_parameter, "required"),
        content_types,
        type_label: mapping_at(body_parameter, "schema").map(type_label),
        description: string_at(body_parameter, "description"),
    })
}

fn responses(operation: &Mapping) -> Vec<Response> {
    let Some(entries) = mapping_at(operation, "responses") else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|(status, response)| {
            let status = match status {
                Value::String(status) => status.clone(),
                Value::Number(status) => status.to_string(),
                _ => return None,
            };
            let response = response.as_mapping()?;
            let (content_types, from_content) = content_summary(mapping_at(response, "content"));
            let type_label =
                from_content.or_else(|| mapping_at(response, "schema").map(type_label));
            Some(Response {
                status: status.into(),
                description: string_at(response, "description").map(prose),
                content_types,
                type_label,
            })
        })
        .collect()
}

fn content_summary(content: Option<&Mapping>) -> (Vec<SharedString>, Option<SharedString>) {
    let Some(content) = content else {
        return (Vec::new(), None);
    };
    let content_types = content
        .keys()
        .filter_map(Value::as_str)
        .map(|media_type| SharedString::from(media_type.to_owned()))
        .collect();
    let type_label = content
        .values()
        .filter_map(Value::as_mapping)
        .find_map(|media| mapping_at(media, "schema").map(type_label));
    (content_types, type_label)
}

fn schema_summaries(schemas: &Mapping) -> Vec<SchemaSummary> {
    schemas
        .iter()
        .filter_map(|(name, schema)| {
            let name = name.as_str()?;
            let schema = schema.as_mapping()?;
            let properties = mapping_at(schema, "properties")
                .map(|properties| {
                    properties
                        .iter()
                        .filter_map(|(property, definition)| {
                            let property = property.as_str()?;
                            let label = definition
                                .as_mapping()
                                .map(type_label)
                                .unwrap_or_else(|| "—".into());
                            Some((SharedString::from(property.to_owned()), label))
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(SchemaSummary {
                name: name.to_owned().into(),
                type_label: type_label(schema),
                properties,
            })
        })
        .collect()
}

/// Renders a schema as a short type, the way an API reference shows it:
/// a reference collapses to the referenced name, an array to `Item[]`.
fn type_label(schema: &Mapping) -> SharedString {
    if let Some(reference) = string_at(schema, "$ref") {
        return reference
            .rsplit('/')
            .next()
            .unwrap_or(reference.as_ref())
            .to_owned()
            .into();
    }
    for composite in ["oneOf", "anyOf", "allOf"] {
        if let Some(variants) = sequence_at(schema, composite) {
            let labels: Vec<String> = variants
                .iter()
                .filter_map(Value::as_mapping)
                .map(|variant| type_label(variant).to_string())
                .collect();
            if labels.is_empty() {
                return composite.into();
            }
            return format!("{composite}({})", labels.join(" | ")).into();
        }
    }
    if schema.contains_key(Value::from("enum")) {
        return "enum".into();
    }

    let Some(declared_type) = string_at(schema, "type") else {
        return "—".into();
    };
    if declared_type == "array" {
        let item_label = mapping_at(schema, "items")
            .map(type_label)
            .unwrap_or_else(|| "—".into());
        return format!("{item_label}[]").into();
    }
    match string_at(schema, "format") {
        Some(format) => format!("{declared_type} ({format})").into(),
        None => declared_type,
    }
}

fn mapping_at<'a>(owner: &'a Mapping, key: &str) -> Option<&'a Mapping> {
    owner.get(Value::from(key))?.as_mapping()
}

fn sequence_at<'a>(owner: &'a Mapping, key: &str) -> Option<&'a Vec<Value>> {
    owner.get(Value::from(key))?.as_sequence()
}

/// Contract prose is Markdown, and a reader of it should not have to read link
/// syntax: `[text](url)` becomes the text it names, or the address itself when
/// the two are the same. Nothing here is clickable, so keeping the brackets
/// would only put punctuation between the reader and the sentence.
fn prose(text: SharedString) -> SharedString {
    if !text.contains("](") {
        return text;
    }
    let source = text.as_ref();
    let mut unwrapped = String::with_capacity(source.len());
    let mut rest = source;
    while let Some(open) = rest.find('[') {
        let after_open = &rest[open + 1..];
        let Some(close) = after_open.find("](") else {
            break;
        };
        let label = &after_open[..close];
        let after_label = &after_open[close + 2..];
        let Some(end) = after_label.find(')') else {
            break;
        };
        let target = &after_label[..end];
        unwrapped.push_str(&rest[..open]);
        unwrapped.push_str(if label.is_empty() { target } else { label });
        rest = &after_label[end + 1..];
    }
    unwrapped.push_str(rest);
    unwrapped.into()
}

/// A summary belongs on the operation's own row, so a contract that wrote it as
/// a folded or multi-line block is squeezed back onto one line. Its description
/// keeps its shape; only this one-line label is collapsed.
fn one_line(text: SharedString) -> SharedString {
    if !text.contains(['\n', '\r', '\t']) && !text.contains("  ") {
        return text;
    }
    text.split_whitespace().collect::<Vec<_>>().join(" ").into()
}

fn string_at(owner: &Mapping, key: &str) -> Option<SharedString> {
    let value = owner.get(Value::from(key))?;
    match value {
        Value::String(text) => Some(text.clone().into()),
        Value::Number(number) => Some(number.to_string().into()),
        Value::Bool(flag) => Some(flag.to_string().into()),
        _ => None,
    }
}

fn bool_at(owner: &Mapping, key: &str) -> bool {
    owner
        .get(Value::from(key))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PETSTORE_V3: &str = r#"
openapi: 3.0.3
info:
  title: Swagger Petstore
  version: 1.0.0
  description: A sample API.
servers:
  - url: https://petstore.example.com/v2
tags:
  - name: pet
    description: Everything about your Pets
  - name: store
    description: Access to Petstore orders
paths:
  /pet:
    post:
      tags: [pet]
      summary: Add a new pet to the store
      operationId: addPet
      security:
        - petstore_auth: []
      requestBody:
        required: true
        content:
          application/json:
            schema:
              $ref: '#/components/schemas/Pet'
      responses:
        '200':
          description: successful operation
          content:
            application/json:
              schema:
                $ref: '#/components/schemas/Pet'
        '405':
          description: Invalid input
  /pet/{petId}:
    parameters:
      - name: petId
        in: path
        required: true
        schema:
          type: integer
          format: int64
    get:
      tags: [pet]
      summary: Find pet by ID
      responses:
        '200':
          description: successful operation
    delete:
      tags: [pet]
      deprecated: true
      summary: Deletes a pet
      responses:
        '400':
          description: Invalid pet value
  /store/inventory:
    get:
      tags: [store]
      summary: Returns pet inventories by status
      responses:
        '200':
          description: successful operation
components:
  schemas:
    Pet:
      type: object
      properties:
        id:
          type: integer
          format: int64
        name:
          type: string
        tags:
          type: array
          items:
            $ref: '#/components/schemas/Tag'
    Tag:
      type: object
      properties:
        name:
          type: string
"#;

    #[test]
    fn recognizes_openapi_and_swagger_documents() {
        assert!(looks_like_openapi("openapi: 3.0.3\ninfo:\n  title: x\n"));
        assert!(looks_like_openapi("swagger: \"2.0\"\n"));
        assert!(looks_like_openapi("{\n  \"openapi\": \"3.1.0\"\n}"));
        // A contract served minified is a single line from brace to brace, and
        // the version key can sit anywhere along it.
        assert!(looks_like_openapi(
            r#"{"openapi":"3.0.4","info":{"title":"Pets"}}"#
        ));
        assert!(looks_like_openapi(
            r#"{"info":{"title":"Pets"},"swagger":"2.0"}"#
        ));
        // A key that merely mentions the word is not a version declaration.
        assert!(!looks_like_openapi("name: openapi-generator\n"));
        assert!(!looks_like_openapi("services:\n  web:\n    image: nginx\n"));
        assert!(!looks_like_openapi(r#"{"generator":"openapi-generator"}"#));
    }

    /// A contract that names its parameters once and references them everywhere
    /// is the norm. Left unresolved, every such parameter reads as "(unnamed)"
    /// query string and the values it accepts are nowhere on the page.
    #[test]
    fn referenced_parameters_and_their_enums_are_resolved() {
        let document = parse(
            "openapi: 3.0.3\ninfo:\n  title: Icons\npaths:\n  /icons/{platform}:\n    delete:\n      parameters:\n        - $ref: '#/components/parameters/IconPlatformPath'\n      responses:\n        '200':\n          description: ok\ncomponents:\n  parameters:\n    IconPlatformPath:\n      name: platform\n      in: path\n      required: true\n      description: Which platform's icons.\n      schema:\n        $ref: '#/components/schemas/IconPlatform'\n  schemas:\n    IconPlatform:\n      type: string\n      enum: [web, android, ios]\n",
        )
        .expect("parse");

        let parameter = &document.groups[0].operations[0].parameters[0];
        assert_eq!(parameter.name.as_ref(), "platform");
        assert_eq!(parameter.location.as_ref(), "path");
        assert!(parameter.required, "the referenced parameter says required");
        assert_eq!(
            parameter.type_label.as_ref(),
            "string",
            "an enumeration reads as what it accepts, not as its schema name"
        );
        assert_eq!(
            parameter.allowed_values,
            vec![
                SharedString::from("web"),
                SharedString::from("android"),
                SharedString::from("ios")
            ]
        );
        assert_eq!(
            parameter.description.as_deref(),
            Some("Which platform's icons.")
        );
    }

    #[test]
    fn a_reference_that_points_nowhere_leaves_the_parameter_as_written() {
        let document = parse(
            "openapi: 3.0.3\ninfo:\n  title: Icons\npaths:\n  /icons:\n    get:\n      parameters:\n        - $ref: '#/components/parameters/Missing'\n      responses:\n        '200':\n          description: ok\n",
        )
        .expect("parse");
        let parameter = &document.groups[0].operations[0].parameters[0];
        assert_eq!(
            parameter.name.as_ref(),
            "(unnamed)",
            "a dangling reference must not panic or invent a name"
        );
    }

    #[test]
    fn link_syntax_is_unwrapped_in_prose() {
        assert_eq!(
            prose("See [the repository](https://example.com/repo) for more.".into()).as_ref(),
            "See the repository for more."
        );
        // A link whose text is the address itself has to keep the address.
        assert_eq!(
            prose("Read [https://example.com](https://example.com).".into()).as_ref(),
            "Read https://example.com."
        );
        assert_eq!(
            prose("- [One](https://a.example) and [Two](https://b.example)".into()).as_ref(),
            "- One and Two"
        );
        // Text that only looks like the start of a link is left alone.
        assert_eq!(
            prose("An array [of things] stays as written".into()).as_ref(),
            "An array [of things] stays as written"
        );
    }

    #[test]
    fn a_multi_line_summary_is_squeezed_onto_one_line() {
        let document = parse(
            "openapi: 3.0.3\ninfo:\n  title: Sessions\npaths:\n  /sessions:\n    get:\n      summary: >\n        Returns a time interval\n        for the given instrument.\n      responses:\n        '200':\n          description: ok\n",
        )
        .expect("parse");
        let operation = &document.groups[0].operations[0];
        assert_eq!(
            operation.summary.as_deref(),
            Some("Returns a time interval for the given instrument."),
            "a row shows one line, however the contract wrapped it"
        );
    }

    #[test]
    fn parses_operations_into_declared_tag_order() {
        let document = parse(PETSTORE_V3).expect("parse");

        assert_eq!(document.spec_label.as_ref(), "OpenAPI 3.0.3");
        assert_eq!(document.title.as_ref(), "Swagger Petstore");
        assert_eq!(document.version.as_deref(), Some("1.0.0"));
        assert_eq!(document.servers.len(), 1);
        assert_eq!(
            document.servers[0].url.as_ref(),
            "https://petstore.example.com/v2"
        );
        assert!(document.notes.is_empty(), "notes: {:?}", document.notes);

        let group_names: Vec<&str> = document
            .groups
            .iter()
            .map(|group| group.name.as_ref())
            .collect();
        assert_eq!(group_names, vec!["pet", "store"]);
        assert_eq!(
            document.groups[0].description.as_deref(),
            Some("Everything about your Pets")
        );
        assert_eq!(document.operation_count(), 4);
    }

    #[test]
    fn carries_path_level_parameters_into_each_operation() {
        let document = parse(PETSTORE_V3).expect("parse");
        let pet_group = &document.groups[0];
        let get_by_id = pet_group
            .operations
            .iter()
            .find(|operation| operation.method == HttpMethod::Get)
            .expect("GET /pet/{petId}");

        assert_eq!(get_by_id.parameters.len(), 1);
        let parameter = &get_by_id.parameters[0];
        assert_eq!(parameter.name.as_ref(), "petId");
        assert_eq!(parameter.location.as_ref(), "path");
        assert!(parameter.required);
        assert_eq!(parameter.type_label.as_ref(), "integer (int64)");

        let delete = pet_group
            .operations
            .iter()
            .find(|operation| operation.method == HttpMethod::Delete)
            .expect("DELETE /pet/{petId}");
        assert!(delete.deprecated);
        assert_eq!(delete.parameters.len(), 1);
    }

    #[test]
    fn summarizes_request_bodies_responses_and_schemas() {
        let document = parse(PETSTORE_V3).expect("parse");
        let post = document.groups[0]
            .operations
            .iter()
            .find(|operation| operation.method == HttpMethod::Post)
            .expect("POST /pet");

        assert!(post.secured);
        let body = post.request_body.as_ref().expect("request body");
        assert!(body.required);
        assert_eq!(
            body.content_types,
            vec![SharedString::from("application/json")]
        );
        assert_eq!(body.type_label.as_deref(), Some("Pet"));

        let statuses: Vec<&str> = post
            .responses
            .iter()
            .map(|response| response.status.as_ref())
            .collect();
        assert_eq!(statuses, vec!["200", "405"]);
        assert_eq!(post.responses[0].type_label.as_deref(), Some("Pet"));

        let pet = document
            .schemas
            .iter()
            .find(|schema| schema.name.as_ref() == "Pet")
            .expect("Pet schema");
        assert_eq!(pet.type_label.as_ref(), "object");
        assert_eq!(
            pet.properties,
            vec![
                (
                    SharedString::from("id"),
                    SharedString::from("integer (int64)")
                ),
                (SharedString::from("name"), SharedString::from("string")),
                (SharedString::from("tags"), SharedString::from("Tag[]")),
            ]
        );
    }

    #[test]
    fn reads_swagger_two_documents_including_body_parameters() {
        let document = parse(
            r#"
swagger: "2.0"
info:
  title: Legacy API
  version: "1.2"
host: api.example.com
basePath: /v1
schemes: [https]
consumes: [application/json]
paths:
  /orders:
    post:
      summary: Place an order
      parameters:
        - name: order
          in: body
          required: true
          schema:
            $ref: '#/definitions/Order'
      responses:
        '201':
          description: created
          schema:
            $ref: '#/definitions/Order'
definitions:
  Order:
    type: object
    properties:
      id:
        type: string
"#,
        )
        .expect("parse");

        assert_eq!(document.spec_label.as_ref(), "Swagger 2.0");
        assert_eq!(document.servers.len(), 1);
        assert_eq!(
            document.servers[0].url.as_ref(),
            "https://api.example.com/v1"
        );
        // Untagged operations still get a group so nothing is dropped.
        assert_eq!(document.groups.len(), 1);
        assert_eq!(document.groups[0].name.as_ref(), UNGROUPED);

        let post = &document.groups[0].operations[0];
        let body = post.request_body.as_ref().expect("body parameter");
        assert!(body.required);
        assert_eq!(body.type_label.as_deref(), Some("Order"));
        assert_eq!(
            body.content_types,
            vec![SharedString::from("application/json")]
        );
        // The body parameter must not also be listed as a plain parameter.
        assert!(post.parameters.is_empty());
        assert_eq!(post.responses[0].type_label.as_deref(), Some("Order"));
    }

    #[test]
    fn reports_what_the_document_omits_instead_of_rendering_nothing() {
        let document = parse("openapi: 3.1.0\ninfo:\n  title: Empty\n").expect("parse");
        assert_eq!(document.title.as_ref(), "Empty");
        assert!(document.version.is_none());
        assert!(document.groups.is_empty());
        assert_eq!(
            document.notes,
            vec![SharedString::from(
                "The document declares no `paths` section."
            )]
        );

        let without_version = parse("info:\n  title: Nameless\npaths: {}\n").expect("parse");
        assert_eq!(without_version.spec_label.as_ref(), "Unknown specification");
        assert_eq!(without_version.notes.len(), 2);
    }

    #[test]
    fn rejects_documents_that_are_not_mappings() {
        assert!(parse("- just\n- a list\n").is_err());
        assert!(parse("openapi: [unclosed\n").is_err());
    }

    #[test]
    fn global_security_marks_operations_that_declare_none_of_their_own() {
        let document = parse(
            r#"
openapi: 3.0.3
info:
  title: Guarded API
  version: "1"
security:
  - bearer: []
paths:
  /guarded:
    get:
      responses:
        '200':
          description: ok
  /open:
    get:
      security: []
      responses:
        '200':
          description: ok
  /own:
    get:
      security:
        - api_key: []
      responses:
        '200':
          description: ok
"#,
        )
        .expect("parse");

        let operations = &document.groups[0].operations;
        let secured_by_path: Vec<(&str, bool)> = operations
            .iter()
            .map(|operation| (operation.path.as_ref(), operation.secured))
            .collect();
        assert_eq!(
            secured_by_path,
            vec![("/guarded", true), ("/open", false), ("/own", true)],
            "an operation inherits the document's security unless it opts out"
        );
    }

    #[test]
    fn a_path_level_body_parameter_is_still_the_request_body() {
        let document = parse(
            r#"
swagger: "2.0"
info:
  title: Shared body
  version: "1"
consumes: [application/json]
paths:
  /items:
    parameters:
      - name: item
        in: body
        required: true
        schema:
          $ref: '#/definitions/Item'
    post:
      responses:
        '201':
          description: created
definitions:
  Item:
    type: object
"#,
        )
        .expect("parse");

        let post = &document.groups[0].operations[0];
        let body = post
            .request_body
            .as_ref()
            .expect("a body declared on the path item must still be found");
        assert!(body.required);
        assert_eq!(body.type_label.as_deref(), Some("Item"));
        assert!(
            post.parameters.is_empty(),
            "the body must not also be listed among the plain parameters"
        );
    }

    #[test]
    fn enum_parameters_capture_their_allowed_values() {
        let document = parse(
            r#"
openapi: 3.0.3
info:
  title: Filters
  version: "1"
paths:
  /pets:
    get:
      parameters:
        - name: status
          in: query
          required: false
          schema:
            type: string
            enum: [available, pending, sold]
      responses:
        '200':
          description: ok
"#,
        )
        .expect("parse");

        let parameter = &document.groups[0].operations[0].parameters[0];
        assert_eq!(
            parameter.allowed_values,
            vec![
                SharedString::from("available"),
                SharedString::from("pending"),
                SharedString::from("sold"),
            ]
        );
        assert_eq!(
            parameter.type_label.as_ref(),
            "string",
            "the reader needs the kind of value to type, which \"enum\" does not say"
        );
    }

    #[test]
    fn a_swagger_two_parameter_declares_its_enum_directly_on_itself() {
        let document = parse(
            r#"
swagger: "2.0"
info:
  title: Filters
  version: "1"
paths:
  /pets:
    get:
      parameters:
        - name: status
          in: query
          required: true
          type: string
          enum: [available, sold]
      responses:
        '200':
          description: ok
"#,
        )
        .expect("parse");

        let parameter = &document.groups[0].operations[0].parameters[0];
        assert_eq!(
            parameter.allowed_values,
            vec![SharedString::from("available"), SharedString::from("sold")]
        );
    }

    #[test]
    fn a_parameter_without_an_enum_has_no_allowed_values() {
        let document = parse(PETSTORE_V3).expect("parse");
        let pet_id = &document.groups[0]
            .operations
            .iter()
            .find(|operation| operation.method == HttpMethod::Get)
            .expect("GET /pet/{petId}")
            .parameters[0];
        assert!(pet_id.allowed_values.is_empty());
    }

    #[test]
    fn available_values_label_matches_the_reference_wording() {
        assert_eq!(available_values_label(&[]), None);
        assert_eq!(
            available_values_label(&["web".into(), "android".into(), "ios".into()]).as_deref(),
            Some("Available values : web, android, ios")
        );
    }

    #[test]
    fn an_empty_backtick_pair_stays_as_written() {
        let spans = split_code_spans(&"Use `` for nothing".into());
        assert_eq!(
            spans,
            vec![
                ProseSpan::Plain("Use ".into()),
                ProseSpan::Plain("``".into()),
                ProseSpan::Plain(" for nothing".into()),
            ],
            "text must never disappear, however odd the backticks are"
        );

        let only_backticks = split_code_spans(&"``".into());
        assert_eq!(only_backticks, vec![ProseSpan::Plain("``".into())]);

        let unterminated = split_code_spans(&"a `b".into());
        assert_eq!(
            unterminated,
            vec![ProseSpan::Plain("a ".into()), ProseSpan::Plain("`b".into())]
        );
    }

    #[test]
    fn split_code_spans_alternates_plain_and_code_text() {
        let spans = split_code_spans(&"See `foo` and `bar`.".into());
        assert_eq!(
            spans,
            vec![
                ProseSpan::Plain("See ".into()),
                ProseSpan::Code("foo".into()),
                ProseSpan::Plain(" and ".into()),
                ProseSpan::Code("bar".into()),
                ProseSpan::Plain(".".into()),
            ]
        );

        // Plain text without a code span is left as a single element.
        assert_eq!(
            split_code_spans(&"No code here.".into()),
            vec![ProseSpan::Plain("No code here.".into())]
        );

        // An unterminated backtick has no closing mark to make a span out of.
        assert_eq!(
            split_code_spans(&"An `open span".into()),
            vec![
                ProseSpan::Plain("An ".into()),
                ProseSpan::Plain("`open span".into()),
            ]
        );
    }

    #[test]
    fn servers_capture_their_description_and_default_to_the_first() {
        let document = parse(
            r#"
openapi: 3.0.3
info:
  title: Multi Server
  version: "1"
servers:
  - url: https://api.example.com/v1
    description: Production
  - url: https://staging.example.com/v1
    description: Staging
paths:
  /ping:
    get:
      responses:
        '200':
          description: ok
"#,
        )
        .expect("parse");

        assert_eq!(document.servers.len(), 2);
        assert_eq!(
            document.servers[0].url.as_ref(),
            "https://api.example.com/v1"
        );
        assert_eq!(
            document.servers[0].description.as_deref(),
            Some("Production")
        );

        let default =
            default_server(&document.servers).expect("a document with servers has a default");
        assert_eq!(default.url.as_ref(), "https://api.example.com/v1");

        // The reader's earlier pick is kept as long as the document still lists it.
        let staging_url: SharedString = "https://staging.example.com/v1".into();
        let resolved = resolve_selected_server(&document.servers, Some(&staging_url))
            .expect("the previously selected server is still in the document");
        assert_eq!(resolved.url.as_ref(), "https://staging.example.com/v1");

        // A pick the document no longer lists (e.g. after an edit) falls back
        // to the default.
        let stale_url: SharedString = "https://removed.example.com".into();
        let fallback = resolve_selected_server(&document.servers, Some(&stale_url))
            .expect("falls back to the default server");
        assert_eq!(fallback.url.as_ref(), "https://api.example.com/v1");

        // No prior pick at all also falls back to the default.
        let fallback_none =
            resolve_selected_server(&document.servers, None).expect("falls back with no pick");
        assert_eq!(fallback_none.url.as_ref(), "https://api.example.com/v1");
    }
}
