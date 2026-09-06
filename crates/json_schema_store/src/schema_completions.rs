use std::path::Path;
use std::sync::{Arc, LazyLock};

use anyhow::Result;
use collections::HashMap;
use gpui::{App, AsyncApp, Entity, Task};
use language::{Buffer, CodeLabel, LanguageRegistry, PointUtf16, ToOffset as _};
use lsp::CompletionContext;
use parking_lot::RwLock;
use project::lsp_store::CompletionDocumentation;
use project::{Completion, CompletionSource, InProcessCompletions, InProcessProject, LspStore};
use schema_documents::{CursorContext, Place, Segment, Style, Suggestion, Suggestions};
use serde_json::Value;

use crate::{all_schema_file_associations, handle_schema_request};

/// Schema documents parsed once per URI. Entries are dropped alongside the
/// serialized ones in `SchemaStore::notify_schema_changed`, so a schema that
/// depends on runtime state is re-read after that state changes.
pub(crate) static PARSED_SCHEMAS: LazyLock<RwLock<HashMap<String, Arc<Value>>>> =
    LazyLock::new(|| RwLock::new(HashMap::default()));

pub fn init(cx: &mut App) {
    project::register_in_process_completions(Arc::new(SchemaCompletions), cx);
}

struct SchemaCompletions;

impl InProcessCompletions for SchemaCompletions {
    fn completions(
        &self,
        project: &InProcessProject,
        buffer: &Entity<Buffer>,
        position: PointUtf16,
        _context: &CompletionContext,
        cx: &mut App,
    ) -> Task<Result<Vec<Completion>>> {
        let nothing = || Task::ready(Ok(Vec::new()));

        let snapshot = buffer.read(cx).snapshot();
        let Some(file) = snapshot.file() else {
            return nothing();
        };
        let path = file.full_path(cx);
        if !matches!(
            path.extension().and_then(|extension| extension.to_str()),
            Some("json") | Some("jsonc")
        ) {
            return nothing();
        }

        let languages = project.languages.clone();
        let Some(uri) = schema_uri_for_path(&languages, &path, cx) else {
            return nothing();
        };
        let lsp_store = project.lsp_store.clone();
        let offset = position.to_offset(&snapshot);

        cx.spawn(async move |cx| {
            let schema = schema_document(lsp_store, uri, cx).await?;
            let executor = cx.background_executor().clone();
            executor
                .spawn(async move {
                    let text = snapshot.text();
                    let Some(found) = suggestions(&schema, &text, offset) else {
                        return Ok(Vec::new());
                    };
                    let start = snapshot.anchor_before(found.range.start);
                    let end = snapshot.anchor_after(found.range.end);
                    Ok(found
                        .items
                        .into_iter()
                        .map(|suggestion| {
                            let Suggestion {
                                label,
                                new_text,
                                detail,
                                documentation,
                            } = suggestion;
                            let label_len = label.len();
                            let text = match detail {
                                Some(detail) => format!("{label}  {detail}"),
                                None => label,
                            };
                            Completion {
                                replace_range: start..end,
                                new_text,
                                label: CodeLabel::filtered(text, label_len, None, Vec::new()),
                                documentation: documentation.map(|text| {
                                    CompletionDocumentation::MultiLineMarkdown(text.into())
                                }),
                                source: CompletionSource::Custom,
                                icon_path: None,
                                icon_color: None,
                                match_start: Some(start),
                                snippet_deduplication_key: None,
                                insert_text_mode: None,
                                confirm: None,
                                group: None,
                            }
                        })
                        .collect())
                })
                .await
        })
    }
}

pub async fn schema_document(
    lsp_store: Entity<LspStore>,
    uri: String,
    cx: &mut AsyncApp,
) -> Result<Arc<Value>> {
    if let Some(parsed) = PARSED_SCHEMAS.read().get(&uri).cloned() {
        return Ok(parsed);
    }
    let json = handle_schema_request(lsp_store, uri.clone(), cx).await?;
    let parsed = Arc::new(serde_json::from_str::<Value>(&json)?);
    PARSED_SCHEMAS.write().insert(uri, parsed.clone());
    Ok(parsed)
}

fn schema_uri_for_path(
    languages: &Arc<LanguageRegistry>,
    path: &Path,
    cx: &mut App,
) -> Option<String> {
    let file_name = path.file_name()?.to_str()?;
    let path = path.to_string_lossy().replace('\\', "/");

    let associations = all_schema_file_associations(languages, None, cx);
    for association in associations.as_array()? {
        let Some(url) = association.get("url").and_then(|url| url.as_str()) else {
            continue;
        };
        let Some(patterns) = association
            .get("fileMatch")
            .and_then(|patterns| patterns.as_array())
        else {
            continue;
        };
        for pattern in patterns.iter().filter_map(|pattern| pattern.as_str()) {
            if matches_file_match(pattern, &path, file_name) {
                return Some(url.to_string());
            }
        }
    }
    None
}

/// The `fileMatch` dialect the JSON language server uses: a pattern without a
/// slash is matched against the file name, one with a slash against a suffix of
/// the path that starts at a component boundary, and `*` stands for any run of
/// characters inside one component.
fn matches_file_match(pattern: &str, path: &str, file_name: &str) -> bool {
    if !pattern.contains('/') {
        return glob_match(pattern, file_name);
    }
    let pattern = pattern.trim_start_matches('/');
    if glob_match(pattern, path) {
        return true;
    }
    path.match_indices('/')
        .any(|(index, _)| glob_match(pattern, &path[index + 1..]))
}

/// `*` stands for any run of characters inside one path component, `**` for
/// any run that may cross component boundaries.
fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern = pattern.as_bytes();
    let text = text.as_bytes();
    let mut pattern_index = 0;
    let mut text_index = 0;
    let mut star: Option<(usize, bool)> = None;
    let mut swallowed = 0;

    while text_index < text.len() {
        if pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
            let crosses_components = pattern.get(pattern_index + 1) == Some(&b'*');
            pattern_index += if crosses_components { 2 } else { 1 };
            star = Some((pattern_index, crosses_components));
            swallowed = text_index;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == text[text_index] {
            pattern_index += 1;
            text_index += 1;
        } else if let Some((resume, crosses_components)) = star
            && (crosses_components || text[swallowed] != b'/')
        {
            swallowed += 1;
            pattern_index = resume;
            text_index = swallowed;
        } else {
            return false;
        }
    }
    pattern[pattern_index..].iter().all(|byte| *byte == b'*')
}

struct Frame {
    is_object: bool,
    keys: Vec<String>,
    pending_key: Option<String>,
    after_colon: bool,
}

impl Frame {
    fn commit_value(&mut self) {
        if let Some(key) = self.pending_key.take() {
            self.keys.push(key);
        }
        self.after_colon = false;
    }
}

/// Reads the document up to the cursor and reports where in the schema the
/// cursor is: the chain of keys leading to the enclosing container, whether a
/// key or a value belongs here, and the token the completion would replace.
///
/// It is a scanner rather than a parse because the document is nearly always
/// syntactically broken while a key is being typed.
fn analyze(text: &str, offset: usize) -> Option<CursorContext> {
    if offset > text.len() || !text.is_char_boundary(offset) {
        return None;
    }
    let bytes = text.as_bytes();
    let mut stack: Vec<Frame> = Vec::new();
    let mut path: Vec<Segment> = Vec::new();
    let mut token = offset..offset;
    let mut index = 0;

    while index < offset {
        match bytes[index] {
            b'{' | b'[' => {
                let is_object = bytes[index] == b'{';
                if let Some(frame) = stack.last() {
                    path.push(if frame.is_object {
                        Segment::Key(frame.pending_key.clone().unwrap_or_default())
                    } else {
                        Segment::Item
                    });
                }
                stack.push(Frame {
                    is_object,
                    keys: Vec::new(),
                    pending_key: None,
                    after_colon: false,
                });
                index += 1;
            }
            b'}' | b']' => {
                if stack.pop().is_some() && !stack.is_empty() {
                    path.pop();
                }
                if let Some(frame) = stack.last_mut() {
                    frame.commit_value();
                }
                index += 1;
            }
            b':' => {
                if let Some(frame) = stack.last_mut() {
                    frame.after_colon = true;
                }
                index += 1;
            }
            b',' => {
                if let Some(frame) = stack.last_mut() {
                    frame.commit_value();
                }
                index += 1;
            }
            b'"' => {
                let (end, closed) = scan_string(bytes, index);
                // A token that reaches the cursor is the one being typed, not
                // one already written, so it is never recorded as a key or a
                // value -- it is what the completion replaces.
                if end >= offset {
                    token = index..end;
                    break;
                }
                if let Some(frame) = stack.last_mut() {
                    if frame.is_object && !frame.after_colon {
                        frame.pending_key = closed.map(|content| unescape(&content));
                    } else {
                        frame.commit_value();
                    }
                }
                index = end;
            }
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                index = bytes[index..]
                    .iter()
                    .position(|byte| *byte == b'\n')
                    .map_or(bytes.len(), |newline| index + newline + 1);
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index = bytes[index + 2..]
                    .windows(2)
                    .position(|pair| pair == b"*/")
                    .map_or(bytes.len(), |close| index + 2 + close + 2);
            }
            byte if byte.is_ascii_whitespace() => index += 1,
            _ => {
                let end = scan_bare_token(bytes, index);
                if end >= offset {
                    token = index..end;
                    break;
                }
                if let Some(frame) = stack.last_mut() {
                    frame.commit_value();
                }
                index = end;
            }
        }
    }

    let place = match stack.last() {
        Some(frame) if frame.is_object => {
            if frame.after_colon {
                path.push(Segment::Key(frame.pending_key.clone().unwrap_or_default()));
                Place::Value
            } else {
                Place::Key {
                    existing: frame.keys.clone(),
                }
            }
        }
        Some(_) => {
            path.push(Segment::Item);
            Place::Value
        }
        None => Place::Value,
    };

    Some(CursorContext { path, place, token })
}

/// Returns the offset just past the string that starts at `start`, and its
/// contents when it is closed. An unterminated string ends at the line break or
/// at the end of the document.
fn scan_string(bytes: &[u8], start: usize) -> (usize, Option<String>) {
    let mut index = start + 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index = (index + 2).min(bytes.len()),
            b'"' => {
                let content = String::from_utf8_lossy(&bytes[start + 1..index]).into_owned();
                return (index + 1, Some(content));
            }
            b'\n' => return (index, None),
            _ => index += 1,
        }
    }
    (bytes.len(), None)
}

fn scan_bare_token(bytes: &[u8], start: usize) -> usize {
    let mut index = start;
    while index < bytes.len()
        && (bytes[index].is_ascii_alphanumeric()
            || matches!(bytes[index], b'_' | b'-' | b'+' | b'.'))
    {
        index += 1;
    }
    if index > start {
        return index;
    }
    // Nothing matched, and the scan still has to move. One byte past a
    // multi-byte character's lead byte is inside that character, and an offset
    // inside a character is what `anchor_before` asserts against; the whole
    // character is the smallest honest step.
    start + width_of_character_at(bytes, start)
}

/// How many bytes the UTF-8 character beginning at `start` takes, read from its
/// lead byte. One for anything that is not a lead byte, so a scan over text
/// that is not valid UTF-8 still moves.
fn width_of_character_at(bytes: &[u8], start: usize) -> usize {
    match bytes.get(start) {
        Some(byte) if *byte >= 0xf0 => 4,
        Some(byte) if *byte >= 0xe0 => 3,
        Some(byte) if *byte >= 0xc0 => 2,
        _ => 1,
    }
}

fn unescape(content: &str) -> String {
    if !content.contains('\\') {
        return content.to_string();
    }
    let mut out = String::with_capacity(content.len());
    let mut characters = content.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            out.push(character);
            continue;
        }
        match characters.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('b') => out.push('\u{8}'),
            Some('f') => out.push('\u{c}'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

/// Everything the schema allows where the cursor is, for a JSON document.
fn suggestions(schema: &Value, text: &str, offset: usize) -> Option<Suggestions> {
    let context = analyze(text, offset)?;
    schema_documents::suggestions_for(schema, text, context, Style::Json)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> Value {
        serde_json::json!({
            "type": "object",
            "allOf": [{ "$ref": "#/$defs/root" }],
            "$defs": {
                "root": {
                    "type": "object",
                    "properties": {
                        "theme": {
                            "description": "The name of the theme.",
                            "type": "string",
                            "enum": ["One Dark", "One Light"]
                        },
                        "vim_mode": { "type": "boolean" },
                        "editor": { "$ref": "#/$defs/editor" },
                        "languages": {
                            "type": "object",
                            "additionalProperties": { "$ref": "#/$defs/editor" }
                        },
                        "paths": {
                            "type": "array",
                            "items": { "type": "string", "enum": ["a", "b"] }
                        }
                    }
                },
                "editor": {
                    "type": "object",
                    "properties": {
                        "tab_size": { "type": "integer" },
                        "soft_wrap": { "type": "string", "enum": ["none", "editor_width"] }
                    }
                }
            }
        })
    }

    fn labels(text: &str) -> Vec<String> {
        let offset = text.find('|').expect("the test text must mark the cursor");
        let text = text.replace('|', "");
        suggestions(&schema(), &text, offset)
            .map(|found| found.items.into_iter().map(|item| item.label).collect())
            .unwrap_or_default()
    }

    #[test]
    fn offers_the_keys_the_schema_allows() {
        assert_eq!(
            labels("{\n  |\n}"),
            vec![
                "\"editor\"",
                "\"languages\"",
                "\"paths\"",
                "\"theme\"",
                "\"vim_mode\""
            ]
        );
    }

    #[test]
    fn does_not_offer_a_key_the_object_already_has() {
        assert_eq!(
            labels("{\n  \"theme\": \"One Dark\",\n  |\n}"),
            vec!["\"editor\"", "\"languages\"", "\"paths\"", "\"vim_mode\""]
        );
    }

    #[test]
    fn offers_enum_values_at_a_value_position() {
        assert_eq!(
            labels("{\n  \"theme\": |\n}"),
            vec!["\"One Dark\"", "\"One Light\""]
        );
    }

    #[test]
    fn offers_both_booleans_for_a_boolean() {
        assert_eq!(labels("{\n  \"vim_mode\": |\n}"), vec!["true", "false"]);
    }

    #[test]
    fn follows_a_ref_into_a_nested_object() {
        assert_eq!(
            labels("{\n  \"editor\": {\n    |\n  }\n}"),
            vec!["\"soft_wrap\"", "\"tab_size\""]
        );
    }

    #[test]
    fn follows_additional_properties_and_then_a_ref() {
        assert_eq!(
            labels("{\n  \"languages\": {\n    \"Rust\": {\n      |\n    }\n  }\n}"),
            vec!["\"soft_wrap\"", "\"tab_size\""]
        );
    }

    #[test]
    fn descends_into_array_items() {
        assert_eq!(labels("{\n  \"paths\": [|]\n}"), vec!["\"a\"", "\"b\""]);
    }

    #[test]
    fn replaces_the_partial_key_being_typed() {
        let text = "{\n  \"the\n}";
        let offset = text.find("\n}").unwrap();
        let found = suggestions(&schema(), text, offset).expect("theme should be offered");
        assert_eq!(&text[found.range.clone()], "\"the");
        let theme = found
            .items
            .iter()
            .find(|item| item.label == "\"theme\"")
            .expect("theme should be offered");
        assert_eq!(theme.new_text, "\"theme\": ");
        assert_eq!(theme.detail.as_deref(), Some("string"));
        assert_eq!(
            theme.documentation.as_deref(),
            Some("The name of the theme.")
        );
    }

    #[test]
    fn keeps_an_existing_colon() {
        let text = "{\n  \"the\": 1\n}";
        let offset = text.find("\": 1").unwrap() + 1;
        let found = suggestions(&schema(), text, offset).expect("theme should be offered");
        let theme = found
            .items
            .iter()
            .find(|item| item.label == "\"theme\"")
            .expect("theme should be offered");
        assert_eq!(theme.new_text, "\"theme\"");
    }

    #[test]
    fn ignores_comments_and_trailing_commas() {
        assert_eq!(
            labels(
                "{\n  // a comment with \" a quote and { a brace\n  \"vim_mode\": true,\n  |\n}"
            ),
            vec!["\"editor\"", "\"languages\"", "\"paths\"", "\"theme\""]
        );
    }

    #[test]
    fn says_nothing_where_the_schema_knows_nothing() {
        assert!(labels("{\n  \"nowhere\": {\n    |\n  }\n}").is_empty());
        assert!(labels("{\n  \"tab_size\": |\n}").is_empty());
    }

    #[test]
    fn matches_file_match_patterns_the_way_the_server_does() {
        assert!(matches_file_match(
            "tsconfig.json",
            "/home/user/app/tsconfig.json",
            "tsconfig.json"
        ));
        assert!(!matches_file_match(
            "tsconfig.json",
            "/home/user/app/package.json",
            "package.json"
        ));
        assert!(matches_file_match(
            "zed/settings.json",
            "/home/user/.config/zed/settings.json",
            "settings.json"
        ));
        assert!(!matches_file_match(
            "zed/settings.json",
            "/home/user/other/settings.json",
            "settings.json"
        ));
        assert!(matches_file_match(
            "*.json",
            "/home/user/app/anything.json",
            "anything.json"
        ));
        assert!(!matches_file_match(
            "*.json",
            "/home/user/app/main.rs",
            "main.rs"
        ));
        assert!(matches_file_match(
            "**/.vscode/**/*.json",
            "app/.vscode/nested/tasks.json",
            "tasks.json"
        ));
        assert!(!matches_file_match(
            "**/.vscode/**/*.json",
            "app/other/tasks.json",
            "tasks.json"
        ));
    }
}

#[cfg(test)]
mod buffer_tests {
    use fs::FakeFs;
    use gpui::TestAppContext;
    use project::{DEFAULT_COMPLETION_CONTEXT, Project};
    use serde_json::json;
    use settings::SettingsStore;
    use util::path;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            super::init(cx);
        });
    }

    async fn completions_at(
        file_name: &str,
        text_with_cursor: &str,
        cx: &mut TestAppContext,
    ) -> Vec<String> {
        init_test(cx);

        let offset = text_with_cursor
            .find('|')
            .expect("the test text must mark the cursor");
        let text = text_with_cursor.replace('|', "");

        let fs = FakeFs::new(cx.executor());
        let mut tree = serde_json::Map::new();
        tree.insert(file_name.to_string(), json!(""));
        fs.insert_tree(path!("/dir"), serde_json::Value::Object(tree))
            .await;
        let project = Project::test(fs, [path!("/dir").as_ref()], cx).await;

        let buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer(format!("{}/{file_name}", path!("/dir")), cx)
            })
            .await
            .expect("the file should open");
        buffer.update(cx, |buffer, cx| buffer.set_text(text.as_str(), cx));
        cx.executor().run_until_parked();

        let responses = project
            .update(cx, |project, cx| {
                project.completions(&buffer, offset, DEFAULT_COMPLETION_CONTEXT, cx)
            })
            .await
            .expect("completions should not fail");

        responses
            .iter()
            .flat_map(|response| &response.completions)
            .map(|completion| completion.new_text.clone())
            .collect()
    }

    #[gpui::test]
    async fn offers_schema_keys_with_no_language_server(cx: &mut TestAppContext) {
        let completions = completions_at(
            "tsconfig.json",
            "{\n  \"compilerOptions\": {\n    |\n  }\n}",
            cx,
        )
        .await;
        assert!(
            completions.contains(&"\"target\": ".to_string()),
            "compilerOptions keys should be offered, got {completions:?}"
        );
        assert!(
            completions.contains(&"\"strict\": ".to_string()),
            "compilerOptions keys should be offered, got {completions:?}"
        );
    }

    #[gpui::test]
    async fn does_not_offer_a_key_the_object_already_has(cx: &mut TestAppContext) {
        let completions = completions_at(
            "tsconfig.json",
            "{\n  \"compilerOptions\": {\n    \"strict\": true,\n    |\n  }\n}",
            cx,
        )
        .await;
        assert!(
            completions.contains(&"\"target\": ".to_string()),
            "other keys should still be offered, got {completions:?}"
        );
        assert!(
            !completions.contains(&"\"strict\": ".to_string()),
            "a key the object already has must not be offered again"
        );
    }

    #[gpui::test]
    async fn offers_enum_values_at_a_value_position(cx: &mut TestAppContext) {
        let completions = completions_at(
            "tsconfig.json",
            "{\n  \"compilerOptions\": {\n    \"target\": |\n  }\n}",
            cx,
        )
        .await;
        assert!(
            completions.contains(&"\"es5\"".to_string())
                && completions.contains(&"\"esnext\"".to_string()),
            "the target enum should be offered, got {completions:?}"
        );
    }

    #[gpui::test]
    async fn offers_booleans_at_a_boolean_value_position(cx: &mut TestAppContext) {
        let completions = completions_at(
            "tsconfig.json",
            "{\n  \"compilerOptions\": {\n    \"strict\": |\n  }\n}",
            cx,
        )
        .await;
        assert_eq!(completions, vec!["true".to_string(), "false".to_string()]);
    }

    #[gpui::test]
    async fn says_nothing_for_a_file_no_schema_covers(cx: &mut TestAppContext) {
        let completions = completions_at("notes.txt", "{\n  |\n}", cx).await;
        assert!(completions.is_empty());
    }

    #[gpui::test]
    async fn says_nothing_for_a_json_file_no_schema_covers(cx: &mut TestAppContext) {
        let completions = completions_at("whatever.json", "{\n  |\n}", cx).await;
        assert!(completions.is_empty(), "got {completions:?}");
    }
}
