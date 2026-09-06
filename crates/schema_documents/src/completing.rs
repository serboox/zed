use std::ops::Range;

use collections::HashSet;
use serde_json::Value;

const MAX_SCHEMA_DEPTH: usize = 32;

/// One step from the root of the document towards the cursor.
#[derive(Debug, Clone, PartialEq)]
pub enum Segment {
    Key(String),
    Item,
}

/// What belongs where the cursor is.
#[derive(Debug, PartialEq)]
pub enum Place {
    Key {
        existing: Vec<String>,
    },
    Value,
    /// Either would be written here, and only the schema can say which. A
    /// YAML sequence item is the case: `- ` is followed by a key where the
    /// items are objects and by a value where they are not, and the text
    /// alone does not distinguish them.
    KeyOrValue,
}

/// Where in the schema the cursor is, as the language's own scanner worked it
/// out. Everything past this point is about the schema alone, and is shared
/// by both languages.
#[derive(Debug)]
pub struct CursorContext {
    pub path: Vec<Segment>,
    pub place: Place,
    pub token: Range<usize>,
}

/// How a suggestion is written into the document. The one thing about
/// completion that the language decides rather than the schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    Json,
    Yaml,
    Toml,
}

impl Style {
    /// A property name as it would appear in this language.
    fn key(self, name: &str) -> String {
        match self {
            Style::Json => literal(&Value::String(name.to_string())),
            Style::Yaml => self.value(&Value::String(name.to_string())),
            Style::Toml if is_a_bare_toml_key(name) => name.to_string(),
            Style::Toml => literal(&Value::String(name.to_string())),
        }
    }

    /// What stands between a key and its value in this language.
    fn separator(self) -> &'static str {
        match self {
            Style::Json | Style::Yaml => ": ",
            Style::Toml => " = ",
        }
    }

    /// A value as it would appear in this language.
    fn value(self, value: &Value) -> String {
        match self {
            Style::Json => literal(value),
            Style::Yaml => match value {
                Value::String(text) if is_a_plain_yaml_scalar(text) => text.clone(),
                // Every JSON literal is also a YAML one -- YAML 1.2 is a
                // superset of JSON -- so quoting the JSON way is always safe
                // and is what anything unsafe to write plainly falls back to.
                value => literal(value),
            },
            // Every TOML scalar is written the JSON way: a quoted string, a
            // bare number, `true` or `false`. What differs is only the
            // brackets around collections, and a schema offers a collection
            // as a value only where it names one as an `enum` member.
            Style::Toml => literal(value),
        }
    }
}

/// Whether a key can be written in TOML with no quotes. Anything else --
/// a dot, a space, a bracket -- would be read as structure rather than as
/// part of the name.
fn is_a_bare_toml_key(name: &str) -> bool {
    !name.is_empty()
        && name.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '_' || character == '-'
        })
}

/// Whether a string can be written in YAML with no quotes and read back as
/// the same string.
///
/// The conservative half of this is the last check: `true`, `null` and `3.5`
/// are strings the schema may well offer, and writing any of them plainly
/// would put a boolean, a null or a number in the file instead.
fn is_a_plain_yaml_scalar(text: &str) -> bool {
    const NOT_AT_THE_START: [char; 20] = [
        '-', '?', ':', ',', '[', ']', '{', '}', '#', '&', '*', '!', '|', '>', '\'', '"', '%', '@',
        '`', '\t',
    ];
    if text.is_empty() || text.trim() != text {
        return false;
    }
    if text.starts_with(NOT_AT_THE_START) {
        return false;
    }
    if text.contains(['\n', '\r']) || text.contains(": ") || text.contains(" #") {
        return false;
    }
    if text.ends_with(':') {
        return false;
    }
    !reads_back_as_something_else(text)
}

fn reads_back_as_something_else(text: &str) -> bool {
    matches!(
        text,
        "true" | "false" | "True" | "False" | "TRUE" | "FALSE" | "null" | "Null" | "NULL" | "~"
    ) || text.parse::<f64>().is_ok()
}

#[derive(Debug, PartialEq)]
pub struct Suggestion {
    pub label: String,
    pub new_text: String,
    pub detail: Option<String>,
    pub documentation: Option<String>,
}

#[derive(Debug)]
pub struct Suggestions {
    pub range: Range<usize>,
    pub items: Vec<Suggestion>,
}

/// Everything the schema allows where the cursor is.
pub fn suggestions_for(
    schema: &Value,
    text: &str,
    context: CursorContext,
    style: Style,
) -> Option<Suggestions> {
    let mut schemas = Vec::new();
    expand(
        schema,
        schema,
        MAX_SCHEMA_DEPTH,
        &mut HashSet::default(),
        &mut schemas,
    );
    for segment in &context.path {
        schemas = child_schemas(schema, &schemas, segment);
        if schemas.is_empty() {
            return None;
        }
    }

    let separator_needed = needs_a_separator(text, &context.token, style);
    let items = match &context.place {
        Place::Key { existing } => {
            key_suggestions(schema, &schemas, existing, separator_needed, style)
        }
        Place::Value => value_suggestions(&schemas, style),
        Place::KeyOrValue => {
            let keys = key_suggestions(schema, &schemas, &[], separator_needed, style);
            if keys.is_empty() {
                value_suggestions(&schemas, style)
            } else {
                keys
            }
        }
    };
    if items.is_empty() {
        return None;
    }
    Some(Suggestions {
        range: context.token,
        items,
    })
}

/// Flattens a schema into the concrete schemas that apply at the same place:
/// the schema itself, whatever its `$ref` points at, and every branch of its
/// `allOf` / `anyOf` / `oneOf`.
fn expand<'a>(
    root: &'a Value,
    schema: &'a Value,
    depth: usize,
    seen_refs: &mut HashSet<String>,
    out: &mut Vec<&'a Value>,
) {
    if depth == 0 {
        return;
    }
    let Some(object) = schema.as_object() else {
        return;
    };
    if let Some(Value::String(reference)) = object.get("$ref")
        && seen_refs.insert(reference.clone())
        && let Some(target) = resolve_pointer(root, reference)
    {
        expand(root, target, depth - 1, seen_refs, out);
    }
    for keyword in ["allOf", "anyOf", "oneOf"] {
        if let Some(Value::Array(branches)) = object.get(keyword) {
            for branch in branches {
                expand(root, branch, depth - 1, seen_refs, out);
            }
        }
    }
    out.push(schema);
}

fn resolve_pointer<'a>(root: &'a Value, reference: &str) -> Option<&'a Value> {
    let pointer = reference.strip_prefix('#')?;
    if pointer.is_empty() {
        return Some(root);
    }
    root.pointer(pointer)
}

fn child_schemas<'a>(root: &'a Value, schemas: &[&'a Value], segment: &Segment) -> Vec<&'a Value> {
    let mut children = Vec::new();
    for &schema in schemas {
        let Some(object) = schema.as_object() else {
            continue;
        };
        match segment {
            Segment::Key(key) => {
                if let Some(property) = object
                    .get("properties")
                    .and_then(|properties| properties.as_object())
                    .and_then(|properties| properties.get(key))
                {
                    children.push(property);
                } else if let Some(additional) = object
                    .get("additionalProperties")
                    .filter(|additional| additional.is_object())
                {
                    children.push(additional);
                }
            }
            Segment::Item => match object.get("items") {
                Some(Value::Array(items)) => children.extend(items.first()),
                Some(items) if items.is_object() => children.push(items),
                _ => {}
            },
        }
    }

    let mut expanded = Vec::new();
    let mut seen_refs = HashSet::default();
    for child in children {
        expand(root, child, MAX_SCHEMA_DEPTH, &mut seen_refs, &mut expanded);
    }
    expanded
}

fn key_suggestions(
    root: &Value,
    schemas: &[&Value],
    existing: &[String],
    separator_needed: bool,
    style: Style,
) -> Vec<Suggestion> {
    let mut offered = HashSet::default();
    let mut items = Vec::new();
    for &schema in schemas {
        let Some(properties) = schema
            .get("properties")
            .and_then(|properties| properties.as_object())
        else {
            continue;
        };
        for (name, property) in properties {
            if existing.iter().any(|key| key == name) || !offered.insert(name.clone()) {
                continue;
            }
            let mut expanded = Vec::new();
            expand(
                root,
                property,
                MAX_SCHEMA_DEPTH,
                &mut HashSet::default(),
                &mut expanded,
            );
            let label = style.key(name);
            let new_text = if separator_needed {
                format!("{label}{}", style.separator())
            } else {
                label.clone()
            };
            items.push(Suggestion {
                label,
                new_text,
                detail: type_name(&expanded),
                documentation: description(&expanded),
            });
        }
    }
    items.sort_by(|left, right| left.label.cmp(&right.label));
    items
}

fn value_suggestions(schemas: &[&Value], style: Style) -> Vec<Suggestion> {
    let mut offered = HashSet::default();
    let mut items = Vec::new();
    let mut allows_boolean = false;

    for schema in schemas {
        let documentation = description(std::slice::from_ref(schema));
        if let Some(Value::Array(values)) = schema.get("enum") {
            for value in values {
                push_value(
                    value,
                    documentation.clone(),
                    style,
                    &mut offered,
                    &mut items,
                );
            }
        }
        if let Some(value) = schema.get("const") {
            push_value(value, documentation, style, &mut offered, &mut items);
        }
        allows_boolean |= match schema.get("type") {
            Some(Value::String(name)) => name == "boolean",
            Some(Value::Array(names)) => names.iter().any(|name| name == "boolean"),
            _ => false,
        };
    }

    if allows_boolean {
        for value in [Value::Bool(true), Value::Bool(false)] {
            push_value(&value, None, style, &mut offered, &mut items);
        }
    }
    items
}

fn push_value(
    value: &Value,
    documentation: Option<String>,
    style: Style,
    offered: &mut HashSet<String>,
    items: &mut Vec<Suggestion>,
) {
    let text = style.value(value);
    if !offered.insert(text.clone()) {
        return;
    }
    items.push(Suggestion {
        label: text.clone(),
        new_text: text,
        detail: None,
        documentation,
    });
}

fn literal(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
}

fn type_name(schemas: &[&Value]) -> Option<String> {
    for schema in schemas {
        match schema.get("type") {
            Some(Value::String(name)) => return Some(name.clone()),
            Some(Value::Array(names)) => {
                let names = names
                    .iter()
                    .filter_map(|name| name.as_str())
                    .filter(|name| *name != "null")
                    .collect::<Vec<_>>();
                if !names.is_empty() {
                    return Some(names.join(" | "));
                }
            }
            _ => {}
        }
    }
    schemas.iter().find_map(|schema| {
        if schema.get("properties").is_some() {
            Some("object".to_string())
        } else if schema.get("items").is_some() {
            Some("array".to_string())
        } else if schema.get("enum").is_some() || schema.get("const").is_some() {
            Some("enum".to_string())
        } else {
            None
        }
    })
}

fn description(schemas: &[&Value]) -> Option<String> {
    schemas.iter().find_map(|schema| {
        schema
            .get("description")
            .and_then(|text| text.as_str())
            .filter(|text| !text.is_empty())
            .map(|text| text.to_string())
    })
}

fn needs_a_separator(text: &str, token: &Range<usize>, style: Style) -> bool {
    let after = text
        .get(token.end..)
        .unwrap_or("")
        .trim_start_matches([' ', '\t']);
    match style {
        Style::Json | Style::Yaml => !after.starts_with(':'),
        Style::Toml => !after.starts_with('='),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn a_yaml_scalar_is_written_plainly_only_where_it_reads_back_as_itself() {
        let plain = ["One Dark", "editor_width", "a-b", "x.y", "café"];
        for text in plain {
            assert_eq!(
                Style::Yaml.value(&Value::String(text.to_string())),
                text,
                "{text}"
            );
        }

        // Each of these would be read back as something other than the
        // string the schema offered, or would not parse at all.
        let quoted = [
            "true", "null", "3.5", "007", "", " a", "a: b", "- a", "*a", "{a}", "a #b", "a:",
        ];
        for text in quoted {
            assert_eq!(
                Style::Yaml.value(&Value::String(text.to_string())),
                format!("{text:?}"),
                "{text}"
            );
        }
    }

    #[test]
    fn a_non_string_reads_the_same_in_both_languages() {
        for value in [
            Value::Bool(true),
            Value::Null,
            serde_json::json!(3),
            serde_json::json!(3.5),
        ] {
            assert_eq!(Style::Yaml.value(&value), Style::Json.value(&value));
        }
    }

    #[test]
    fn a_key_is_quoted_in_json_and_bare_in_yaml() {
        assert_eq!(Style::Json.key("tab_size"), "\"tab_size\"");
        assert_eq!(Style::Yaml.key("tab_size"), "tab_size");
        assert_eq!(
            Style::Yaml.key("on"),
            "on",
            "a plain word stays plain -- YAML 1.2 has no `on` boolean"
        );
        assert_eq!(Style::Yaml.key("true"), "\"true\"");
    }

    /// A TOML key is written bare where TOML would read it back as the same
    /// name, and quoted otherwise -- a dot or a space in a bare key would be
    /// read as structure. The value goes after `=` rather than after `:`.
    #[test]
    fn a_toml_key_is_bare_only_where_toml_reads_it_back_as_itself() {
        assert_eq!(Style::Toml.key("tab-size"), "tab-size");
        assert_eq!(Style::Toml.key("rust-version"), "rust-version");
        assert_eq!(Style::Toml.key("a.b"), "\"a.b\"");
        assert_eq!(Style::Toml.key("a b"), "\"a b\"");
        assert_eq!(Style::Toml.key(""), "\"\"");
        assert_eq!(Style::Toml.separator(), " = ");
        assert_eq!(
            Style::Toml.value(&Value::String("2024".to_string())),
            "\"2024\""
        );
        assert_eq!(Style::Toml.value(&Value::Bool(true)), "true");
    }

    /// A key that already has its separator is completed without a second
    /// one, and TOML's separator is not a colon.
    #[test]
    fn a_key_that_already_has_its_separator_does_not_get_another() {
        assert!(needs_a_separator("name", &(0..4), Style::Toml));
        assert!(!needs_a_separator("name = 1", &(0..4), Style::Toml));
        assert!(
            needs_a_separator("name: 1", &(0..4), Style::Toml),
            "a colon is not TOML's"
        );
        assert!(!needs_a_separator("name: 1", &(0..4), Style::Yaml));
    }
}
