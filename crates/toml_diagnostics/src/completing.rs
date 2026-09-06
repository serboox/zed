use std::sync::Arc;

use anyhow::Result;
use gpui::{App, Entity, Task};
use language::{Buffer, CodeLabel, PointUtf16, ToOffset as _};
use lsp::CompletionContext;
use project::lsp_store::CompletionDocumentation;
use project::{Completion, CompletionSource, InProcessCompletions, InProcessProject};
use schema_documents::{Association, CursorContext, Place, Segment, Style, schema_covering};

pub fn init(cx: &mut App) {
    project::register_in_process_completions(Arc::new(TomlSchemaCompletions), cx);
}

struct TomlSchemaCompletions;

impl InProcessCompletions for TomlSchemaCompletions {
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
        if path.extension().and_then(|extension| extension.to_str()) != Some("toml") {
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
                        schema_documents::suggestions_for(&schema, &text, context, Style::Toml)
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
/// It reads lines rather than parsing, because the document is nearly always
/// broken while a key is being typed -- and in TOML the last table header
/// above the cursor is the structure, so lines are the honest unit.
pub fn analyze(text: &str, offset: usize) -> Option<CursorContext> {
    if offset > text.len() || !text.is_char_boundary(offset) {
        return None;
    }
    let line_start = text[..offset].rfind('\n').map_or(0, |at| at + 1);
    let head = text.get(line_start..offset)?;
    let written = head.trim_start_matches([' ', '\t']);
    if written.starts_with('#') {
        return None;
    }
    // A table header names a place in the document rather than a value in it,
    // and what belongs there is the set of tables the schema describes --
    // which is not something this scanner is able to say.
    if written.starts_with('[') {
        return None;
    }

    let (mut path, existing) = walked_to(text.get(..line_start)?);

    match written.split_once('=') {
        Some((name, value)) => {
            for step in steps_of(name.trim()) {
                path.push(Segment::Key(step));
            }
            let typed = value.trim_start_matches([' ', '\t']);
            Some(CursorContext {
                path,
                place: Place::Value,
                token: (offset - typed.len())..offset,
            })
        }
        // A dotted key half written: everything before the last dot is a step
        // deeper into the schema, and the tail is the word being completed.
        None => {
            let (deeper, tail) = match written.rfind('.') {
                Some(dot) => (&written[..dot], &written[dot + 1..]),
                None => ("", written),
            };
            for step in steps_of(deeper) {
                path.push(Segment::Key(step));
            }
            Some(CursorContext {
                path,
                place: Place::Key { existing },
                token: (offset - tail.len())..offset,
            })
        }
    }
}

/// The table the cursor is in, and the keys already written in it.
///
/// The keys are collected only since the last header, because that is the
/// table the cursor is in -- a key of an earlier table is not one this table
/// already has, and leaving it in would hide it from the suggestions.
fn walked_to(before: &str) -> (Vec<Segment>, Vec<String>) {
    let mut path = Vec::new();
    let mut existing = Vec::new();
    for line in before.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(header) = line.strip_prefix("[[") {
            path = header_steps(header);
            path.push(Segment::Item);
            existing.clear();
        } else if let Some(header) = line.strip_prefix('[') {
            path = header_steps(header);
            existing.clear();
        } else if let Some((name, _)) = line.split_once('=')
            && let Some(first) = steps_of(name.trim()).into_iter().next()
        {
            existing.push(first);
        }
    }
    (path, existing)
}

fn header_steps(header: &str) -> Vec<Segment> {
    let named = header.split(']').next().unwrap_or(header);
    steps_of(named).into_iter().map(Segment::Key).collect()
}

/// A dotted key as its separate steps, with each step's quotes taken off.
///
/// The split respects quotes, so a key written `"a.b"` is one step and not
/// two -- which is the difference between a table and a key holding a dot.
fn steps_of(key: &str) -> Vec<String> {
    let mut steps = Vec::new();
    let mut step = String::new();
    let mut quote = None;
    for character in key.chars() {
        match quote {
            Some(open) if character == open => quote = None,
            Some(_) => step.push(character),
            None if character == '"' || character == '\'' => quote = Some(character),
            None if character == '.' => {
                steps.push(std::mem::take(&mut step));
                continue;
            }
            None => step.push(character),
        }
    }
    steps.push(step);
    steps
        .into_iter()
        .map(|step| step.trim().to_string())
        .filter(|step| !step.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use std::ops::Range;

    use super::*;
    use pretty_assertions::assert_eq;

    fn at_the_end_of(text: &str) -> Option<CursorContext> {
        analyze(text, text.len())
    }

    fn keys(path: &[Segment]) -> Vec<String> {
        path.iter()
            .map(|segment| match segment {
                Segment::Key(key) => key.clone(),
                Segment::Item => "[]".to_string(),
            })
            .collect()
    }

    fn token(text: &str, range: Range<usize>) -> &str {
        &text[range]
    }

    #[test]
    fn a_word_at_the_top_of_the_file_is_a_key_of_the_whole_document() {
        let text = "na";
        let context = at_the_end_of(text).expect("a key is being typed");
        assert!(keys(&context.path).is_empty());
        assert_eq!(context.place, Place::Key { existing: vec![] });
        assert_eq!(token(text, context.token), "na");
    }

    /// The header above the cursor is the table it is in, so the schema is
    /// walked into that table before anything is suggested.
    #[test]
    fn a_word_under_a_header_is_a_key_of_that_table() {
        let text = "[package]\nname = \"zed\"\ned";
        let context = at_the_end_of(text).expect("a key is being typed");
        assert_eq!(keys(&context.path), vec!["package".to_string()]);
        assert_eq!(
            context.place,
            Place::Key {
                existing: vec!["name".to_string()]
            },
            "`name` is written already, so it is not offered again"
        );
        assert_eq!(token(text, context.token), "ed");
    }

    /// An array of tables is an array in the schema, so a step into its items
    /// stands between the array and the keys of one entry.
    #[test]
    fn a_word_under_a_double_header_walks_through_the_arrays_items() {
        let text = "[[bin]]\npa";
        let context = at_the_end_of(text).expect("a key is being typed");
        assert_eq!(
            keys(&context.path),
            vec!["bin".to_string(), "[]".to_string()]
        );
    }

    #[test]
    fn what_stands_after_an_equals_sign_is_a_value_of_that_key() {
        let text = "[package]\nedition = \"20";
        let context = at_the_end_of(text).expect("a value is being typed");
        assert_eq!(
            keys(&context.path),
            vec!["package".to_string(), "edition".to_string()]
        );
        assert_eq!(context.place, Place::Value);
        assert_eq!(token(text, context.token), "\"20");
    }

    /// A dotted key reaches the same place a header would, so the same
    /// suggestions belong at the end of it.
    #[test]
    fn a_dotted_key_walks_as_deep_as_the_dots_it_has() {
        let text = "package.na";
        let context = at_the_end_of(text).expect("a key is being typed");
        assert_eq!(keys(&context.path), vec!["package".to_string()]);
        assert_eq!(token(text, context.token), "na");
    }

    #[test]
    fn nothing_is_suggested_inside_a_comment_or_inside_a_header() {
        for text in ["# na", "[package]\n# na", "[pack", "[[bi"] {
            assert!(at_the_end_of(text).is_none(), "{text:?}");
        }
    }

    /// A key with a dot inside its quotes is one step. Reading it as two
    /// would walk into a table the schema does not have and offer nothing.
    #[test]
    fn a_quoted_key_holding_a_dot_is_a_single_step() {
        assert_eq!(
            steps_of("\"a.b\".c"),
            vec!["a.b".to_string(), "c".to_string()]
        );
        assert_eq!(steps_of("a.b"), vec!["a".to_string(), "b".to_string()]);
        assert_eq!(steps_of(""), Vec::<String>::new());
    }

    /// The keys of an earlier table are not keys of this one.
    #[test]
    fn only_the_keys_of_the_table_the_cursor_is_in_count_as_already_written() {
        let text = "[a]\nname = 1\n\n[b]\nna";
        let context = at_the_end_of(text).expect("a key is being typed");
        assert_eq!(keys(&context.path), vec!["b".to_string()]);
        assert_eq!(context.place, Place::Key { existing: vec![] });
    }
}
