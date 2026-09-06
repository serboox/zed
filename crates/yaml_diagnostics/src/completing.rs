use std::sync::Arc;

use anyhow::Result;
use gpui::{App, Entity, Task};
use language::{Buffer, CodeLabel, PointUtf16, ToOffset as _};
use lsp::CompletionContext;
use project::lsp_store::CompletionDocumentation;
use project::{Completion, CompletionSource, InProcessCompletions, InProcessProject};
use schema_documents::{Association, CursorContext, Place, Segment, Style, schema_covering};

pub fn init(cx: &mut App) {
    project::register_in_process_completions(Arc::new(YamlSchemaCompletions), cx);
}

struct YamlSchemaCompletions;

impl InProcessCompletions for YamlSchemaCompletions {
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
            Some("yaml") | Some("yml")
        ) {
            return nothing();
        }

        let languages = project.languages.clone();
        let rules = json_schema_store::all_schema_file_associations(&languages, None, cx);
        let Some(uri) = serde_json::from_value::<Vec<Association>>(rules)
            .ok()
            .and_then(|rules| schema_covering(&rules, &path))
        else {
            return nothing();
        };
        let lsp_store = project.lsp_store.clone();
        let offset = position.to_offset(&snapshot);

        cx.spawn(async move |cx| {
            let schema =
                json_schema_store::schema_completions::schema_document(lsp_store, uri, cx).await?;
            let executor = cx.background_executor().clone();
            executor
                .spawn(async move {
                    let text = snapshot.text();
                    let Some(context) = analyze(&text, offset) else {
                        return Ok(Vec::new());
                    };
                    let Some(found) =
                        schema_documents::suggestions_for(&schema, &text, context, Style::Yaml)
                    else {
                        return Ok(Vec::new());
                    };
                    let start = snapshot.anchor_before(found.range.start);
                    let end = snapshot.anchor_after(found.range.end);
                    Ok(found
                        .items
                        .into_iter()
                        .map(|suggestion| {
                            let label_len = suggestion.label.len();
                            let text = match suggestion.detail {
                                Some(detail) => format!("{}  {detail}", suggestion.label),
                                None => suggestion.label,
                            };
                            Completion {
                                replace_range: start..end,
                                new_text: suggestion.new_text,
                                label: CodeLabel::filtered(text, label_len, None, Vec::new()),
                                documentation: suggestion.documentation.map(|text| {
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

/// Reads the document down to the cursor and reports where in the schema the
/// cursor is.
///
/// It reads lines and their indentation rather than parsing, because the
/// document is nearly always broken while a key is being typed -- and in
/// block YAML the indentation is the structure, so lines are the honest unit.
pub fn analyze(text: &str, offset: usize) -> Option<CursorContext> {
    if offset > text.len() || !text.is_char_boundary(offset) {
        return None;
    }
    let line_start = text[..offset].rfind('\n').map_or(0, |at| at + 1);
    let head = &text[line_start..offset];
    if in_a_comment(head) || opens_a_flow_collection(head) {
        return None;
    }

    let mut path = Steps::default();
    for line in text[..line_start].lines() {
        path.read(line);
    }
    let indent = head.len() - head.trim_start_matches([' ', '\t']).len();
    if path.still_inside_something_that_is_not_structure(indent) {
        return None;
    }

    let mut at = line_start + indent;
    let mut wrote_a_dash = false;
    while let Some(width) = dash_width(&text[at..offset]) {
        path.enter_a_sequence(at - line_start);
        at += width;
        at += text[at..offset].len() - text[at..offset].trim_start_matches(' ').len();
        wrote_a_dash = true;
    }

    let column = at - line_start;
    let rest = &text[at..offset];

    if let Some(colon) = colon_ending_a_key(rest) {
        let name = rest[..colon].trim().to_string();
        path.enter_a_mapping(column);
        let mut value_at = at + colon + 1;
        value_at += text[value_at..offset].len() - text[value_at..offset].trim_start().len();
        return Some(CursorContext {
            path: path.finish(Some(Segment::Key(name))),
            place: Place::Value,
            token: value_at..word_end(text, offset),
        });
    }

    let existing = path.enter_a_mapping(column);
    let token = at..word_end(text, offset);
    let place = if wrote_a_dash {
        // `- ` takes a key where the items are objects and a value where they
        // are not, and nothing but the schema can say which.
        Place::KeyOrValue
    } else {
        Place::Key { existing }
    };
    Some(CursorContext {
        path: path.finish(None),
        place,
        token,
    })
}

/// How wide the `- ` at the start of this text is, where there is one.
fn dash_width(text: &str) -> Option<usize> {
    if text == "-" {
        return Some(1);
    }
    text.starts_with("- ").then_some(2)
}

/// Where a key's own colon is, for a line that has written one.
///
/// A colon inside a value -- `url: http://example.com` -- is not one, which
/// is why the first colon followed by a space or by nothing is the one that
/// counts.
fn colon_ending_a_key(rest: &str) -> Option<usize> {
    let at = rest.find(':')?;
    let after = &rest[at + 1..];
    (after.is_empty() || after.starts_with([' ', '\t'])).then_some(at)
}

fn in_a_comment(head: &str) -> bool {
    let trimmed = head.trim_start();
    trimmed.starts_with('#') || head.contains(" #") || head.contains("\t#")
}

/// Whether a flow collection is open where the cursor is. Flow style inside a
/// line is not read here, and answering out of the surrounding block context
/// would be answering about the wrong place.
fn opens_a_flow_collection(head: &str) -> bool {
    let mut depth = 0i32;
    for byte in head.bytes() {
        match byte {
            b'[' | b'{' => depth += 1,
            b']' | b'}' => depth -= 1,
            _ => {}
        }
    }
    depth > 0
}

/// The end of the word the cursor stands in, so that a completion replaces
/// what is written rather than doubling it.
fn word_end(text: &str, offset: usize) -> usize {
    let rest = &text[offset..];
    let end = rest
        .find(|character: char| matches!(character, ':' | '#' | '\n' | '\r'))
        .unwrap_or(rest.len());
    offset + rest[..end].trim_end().len()
}

/// The chain of keys and items leading from the root of the document to the
/// line being read, worked out from indentation alone.
#[derive(Default)]
struct Steps {
    steps: Vec<Step>,
    /// The indent a block scalar's body is under, while one is open. Its
    /// lines are text, not structure, and reading them as keys would invent
    /// a shape the document does not have.
    inside_a_block_scalar: Option<usize>,
    /// How many flow collections are open across the lines read so far.
    flow_depth: i32,
}

struct Step {
    indent: usize,
    segment: Segment,
    /// The keys of this step's own mapping that were written before it.
    siblings: Vec<String>,
}

impl Steps {
    /// Whether the line the cursor is on is still inside something that is
    /// not structure: a flow collection left open on an earlier line, or a
    /// block scalar's body.
    ///
    /// A block scalar's body is everything indented past the key that opened
    /// it, so the first line back at that indentation ends it -- and that is
    /// the line the reader is typing a new key on.
    fn still_inside_something_that_is_not_structure(&mut self, indent: usize) -> bool {
        if self.flow_depth > 0 {
            return true;
        }
        match self.inside_a_block_scalar {
            Some(under) if indent > under => true,
            Some(_) => {
                self.inside_a_block_scalar = None;
                false
            }
            None => false,
        }
    }

    /// Reads one whole line that stands before the cursor's own.
    fn read(&mut self, line: &str) {
        let indent = line.len() - line.trim_start_matches([' ', '\t']).len();
        if let Some(under) = self.inside_a_block_scalar {
            if line.trim().is_empty() || indent > under {
                return;
            }
            self.inside_a_block_scalar = None;
        }
        if self.flow_depth > 0 {
            self.flow_depth += flow_change(line);
            return;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            return;
        }
        if trimmed.starts_with("---") || trimmed.starts_with("...") {
            // A new document in the same stream starts from nothing.
            self.steps.clear();
            return;
        }

        let mut at = indent;
        while let Some(width) = dash_width(&line[at..]) {
            self.enter_a_sequence(at);
            at += width;
            at += line[at..].len() - line[at..].trim_start_matches(' ').len();
        }
        let rest = &line[at..];
        let Some(colon) = colon_ending_a_key(rest) else {
            self.flow_depth += flow_change(rest);
            return;
        };
        let name = rest[..colon].trim().to_string();
        let siblings = self.enter_a_mapping(at);
        let after = rest[colon + 1..].trim();
        if after.starts_with(['|', '>']) {
            self.inside_a_block_scalar = Some(at);
        } else {
            self.flow_depth += flow_change(after);
        }
        self.steps.push(Step {
            indent: at,
            segment: Segment::Key(name),
            siblings,
        });
    }

    /// Closes everything that ends at or before this column and reports the
    /// keys already written in the mapping the column belongs to.
    fn enter_a_mapping(&mut self, column: usize) -> Vec<String> {
        let mut siblings = Vec::new();
        while let Some(last) = self.steps.last() {
            if last.indent < column {
                break;
            }
            let closed = self.steps.pop().expect("just looked at it");
            if closed.indent == column
                && let Segment::Key(name) = closed.segment
            {
                siblings = closed.siblings;
                siblings.push(name);
            }
        }
        siblings
    }

    /// Closes everything nested deeper than this column, and the sibling item
    /// that ends where this one starts.
    ///
    /// A block sequence may sit at its own key's indentation -- `tags:` on
    /// one line and `- a` at column zero on the next is ordinary YAML -- so
    /// a dash closes only what is nested strictly deeper, plus the previous
    /// item at its own column.
    fn enter_a_sequence(&mut self, column: usize) {
        while self.steps.last().is_some_and(|last| last.indent > column) {
            self.steps.pop();
        }
        if self
            .steps
            .last()
            .is_some_and(|last| last.indent == column && last.segment == Segment::Item)
        {
            self.steps.pop();
        }
        self.steps.push(Step {
            indent: column,
            segment: Segment::Item,
            siblings: Vec::new(),
        });
    }

    fn finish(self, last: Option<Segment>) -> Vec<Segment> {
        self.steps
            .into_iter()
            .map(|step| step.segment)
            .chain(last)
            .collect()
    }
}

fn flow_change(text: &str) -> i32 {
    text.bytes().fold(0, |depth, byte| match byte {
        b'[' | b'{' => depth + 1,
        b']' | b'}' => depth - 1,
        _ => depth,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use schema_documents::{Suggestions, suggestions_for};
    use serde_json::{Value, json};

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "theme": {
                    "description": "The name of the theme.",
                    "type": "string",
                    "enum": ["One Dark", "One Light"]
                },
                "vim_mode": { "type": "boolean" },
                "script": { "type": "string" },
                "editor": {
                    "type": "object",
                    "properties": {
                        "tab_size": { "type": "integer" },
                        "soft_wrap": { "type": "string", "enum": ["none", "editor_width"] }
                    }
                },
                "steps": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string" },
                            "run": { "type": "string" }
                        }
                    }
                },
                "paths": {
                    "type": "array",
                    "items": { "type": "string", "enum": ["a", "b"] }
                }
            }
        })
    }

    /// Reads a text with the cursor marked by `|`, and answers what would be
    /// offered there along with the text the cursor was taken out of.
    fn at(marked: &str) -> (String, Option<Suggestions>) {
        let offset = marked
            .find('@')
            .expect("the test text must mark the cursor");
        let text = marked.replace('@', "");
        let found = analyze(&text, offset)
            .and_then(|context| suggestions_for(&schema(), &text, context, Style::Yaml));
        (text, found)
    }

    fn labels(marked: &str) -> Vec<String> {
        at(marked)
            .1
            .map(|found| found.items.into_iter().map(|item| item.label).collect())
            .unwrap_or_default()
    }

    #[test]
    fn offers_the_keys_the_schema_allows_without_quoting_them() {
        assert_eq!(
            labels("@"),
            vec!["editor", "paths", "script", "steps", "theme", "vim_mode"]
        );
    }

    #[test]
    fn does_not_offer_a_key_the_document_already_has() {
        let offered = labels("theme: One Dark\n@");
        assert!(!offered.contains(&"theme".to_string()), "{offered:?}");
        assert!(offered.contains(&"editor".to_string()), "{offered:?}");
    }

    #[test]
    fn offers_the_values_the_schema_lists_at_a_value_position() {
        assert_eq!(labels("theme: @"), vec!["One Dark", "One Light"]);
        assert_eq!(labels("vim_mode: @"), vec!["true", "false"]);
    }

    #[test]
    fn descends_into_a_nested_mapping_by_indentation() {
        assert_eq!(labels("editor:\n  @"), vec!["soft_wrap", "tab_size"]);
        assert_eq!(
            labels("editor:\n  soft_wrap: @"),
            vec!["none", "editor_width"]
        );
    }

    /// `- ` takes a key where the schema's items are objects and a value
    /// where they are not, and only the schema can say which.
    #[test]
    fn a_sequence_item_offers_keys_for_objects_and_values_for_anything_else() {
        assert_eq!(labels("steps:\n  - @"), vec!["name", "run"]);
        assert_eq!(labels("paths:\n  - @"), vec!["a", "b"]);

        // And the keys of the item already being written are its siblings,
        // so none of them is offered a second time.
        assert_eq!(labels("steps:\n  - name: a\n    @"), vec!["run"]);
    }

    /// A block sequence may sit at its own key's indentation. Reading that as
    /// a step back out of the key would answer about the wrong place.
    #[test]
    fn a_sequence_at_its_own_keys_indentation_is_still_inside_it() {
        assert_eq!(labels("paths:\n- @"), vec!["a", "b"]);
        assert_eq!(labels("paths:\n- a\n- @"), vec!["a", "b"]);
    }

    #[test]
    fn replaces_the_partial_key_being_typed_and_writes_the_colon() {
        let (text, found) = at("the@");
        let found = found.expect("theme should be offered");
        assert_eq!(&text[found.range.clone()], "the");
        let theme = found
            .items
            .iter()
            .find(|item| item.label == "theme")
            .expect("theme should be offered");
        assert_eq!(theme.new_text, "theme: ");
        assert_eq!(theme.detail.as_deref(), Some("string"));
        assert_eq!(
            theme.documentation.as_deref(),
            Some("The name of the theme.")
        );
    }

    #[test]
    fn keeps_a_colon_that_is_already_written() {
        let (text, found) = at("the@: One Dark");
        let found = found.expect("theme should be offered");
        assert_eq!(&text[found.range.clone()], "the");
        let theme = found
            .items
            .iter()
            .find(|item| item.label == "theme")
            .expect("theme should be offered");
        assert_eq!(theme.new_text, "theme");
    }

    /// A value with a space in it is one value, so the whole of what is
    /// written after the colon is what a completion replaces.
    #[test]
    fn replaces_the_whole_partial_value_and_not_just_its_last_word() {
        let (text, found) = at("theme: One D@");
        let found = found.expect("the theme values should be offered");
        assert_eq!(&text[found.range], "One D");
    }

    /// A block scalar's body is text, not structure. Reading its lines as
    /// keys would invent a shape the document does not have and answer about
    /// somewhere that is not in the schema at all.
    #[test]
    fn the_body_of_a_block_scalar_is_not_read_as_structure() {
        let offered = labels("script: |\n  editor:\n    tab_size: 2\n@");
        assert!(offered.contains(&"theme".to_string()), "{offered:?}");
        assert!(!offered.contains(&"tab_size".to_string()), "{offered:?}");
    }

    #[test]
    fn a_document_separator_starts_the_next_one_from_nothing() {
        let offered = labels("editor:\n  tab_size: 2\n---\n@");
        assert!(offered.contains(&"theme".to_string()), "{offered:?}");
        assert!(!offered.contains(&"soft_wrap".to_string()), "{offered:?}");
    }

    /// Flow style within a line is not read here, and answering out of the
    /// block context around it would be answering about the wrong place.
    #[test]
    fn says_nothing_inside_a_comment_or_a_flow_collection() {
        assert!(labels("# @").is_empty());
        assert!(labels("theme: One Dark # @").is_empty());
        assert!(labels("editor: {@").is_empty());
        assert!(labels("paths: [a, @").is_empty());
    }

    #[test]
    fn says_nothing_where_the_schema_knows_nothing() {
        assert!(labels("nowhere:\n  @").is_empty());
        assert!(labels("script: x@").is_empty());
    }
}
