use std::sync::Arc;

use anyhow::Result;
use gpui::{App, Entity, Task};
use language::{Buffer, BufferSnapshot, CodeLabel};
use project::{
    Completion, CompletionContext, CompletionDocumentation, CompletionSource, InProcessCompletions,
    Project,
};
use text::{PointUtf16, ToOffset as _};

/// Offers the names the project itself declares, out of the index, so that a
/// reader with no language server running still completes what the project
/// contains.
pub fn init(cx: &mut App) {
    project::register_in_process_completions(Arc::new(ProjectSymbols), cx);
}

struct ProjectSymbols;

/// Below this a prefix says too little: two letters match thousands of names in
/// a project this size, and a menu of thousands is noise rather than help.
const SHORTEST_PREFIX_WORTH_ANSWERING: usize = 3;

/// How many names the index is asked for. More than a menu shows, so that the
/// ones reaching it are the best matches rather than the first found.
const MOST_THE_INDEX_IS_ASKED_FOR: usize = 100;

impl InProcessCompletions for ProjectSymbols {
    fn completions(
        &self,
        project: &Entity<Project>,
        buffer: &Entity<Buffer>,
        position: PointUtf16,
        _context: &CompletionContext,
        cx: &mut App,
    ) -> Task<Result<Vec<Completion>>> {
        let nothing = || Task::ready(Ok(Vec::new()));
        let Some(index) = crate::of_project(project, cx) else {
            return nothing();
        };
        let snapshot = buffer.read(cx).snapshot();
        let offset = position.to_offset(&snapshot);
        let Some((start, prefix)) = word_before(&snapshot, offset) else {
            return nothing();
        };
        if prefix.chars().count() < SHORTEST_PREFIX_WORTH_ANSWERING {
            return nothing();
        }

        let found = index
            .read(cx)
            .candidates(&prefix, MOST_THE_INDEX_IS_ASKED_FOR);
        // The prefix itself is replaced, not appended to, so accepting the
        // suggestion leaves the name once rather than twice.
        let range = snapshot.anchor_before(start)..snapshot.anchor_after(offset);
        Task::ready(Ok(completions_for(found, range)))
    }
}

/// The word being typed, ending at the cursor, and where it starts.
///
/// Only what is behind the cursor: a completion replaces what has been typed,
/// and taking the rest of the word as well would swallow the name a reader is
/// editing the front of.
fn word_before(snapshot: &BufferSnapshot, offset: usize) -> Option<(usize, String)> {
    let mut start = offset;
    for character in snapshot.reversed_chars_at(offset) {
        if !(character.is_alphanumeric() || character == '_') {
            break;
        }
        start -= character.len_utf8();
    }
    if start == offset {
        return None;
    }
    Some((start, snapshot.text_for_range(start..offset).collect()))
}

/// One entry per name rather than one per declaration: the menu is a list of
/// names to type, and the same name declared in four files is one thing to
/// type, not four lines that look identical.
fn completions_for(
    found: Vec<crate::Definition>,
    range: std::ops::Range<text::Anchor>,
) -> Vec<Completion> {
    let mut seen: collections::HashSet<String> = collections::HashSet::default();
    let mut completions = Vec::new();
    for definition in found {
        if !seen.insert(definition.name.clone()) {
            continue;
        }
        let name_len = definition.name.len();
        let label = format!("{}  {}", definition.name, definition.kind);
        completions.push(Completion {
            replace_range: range.clone(),
            new_text: definition.name.clone(),
            label: CodeLabel::filtered(label, name_len, None, Vec::new()),
            documentation: Some(CompletionDocumentation::MultiLineMarkdown(
                format!(
                    "`{}` · {}:{}",
                    definition.kind, definition.path, definition.line
                )
                .into(),
            )),
            source: CompletionSource::Custom,
            icon_path: None,
            icon_color: None,
            match_start: Some(range.start),
            snippet_deduplication_key: None,
            insert_text_mode: None,
            confirm: None,
            group: None,
        });
    }
    completions
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Definition;

    fn declared(name: &str, kind: &str, path: &str) -> Definition {
        Definition {
            path: path.to_string(),
            name: name.to_string(),
            kind: kind.to_string(),
            line: 7,
            language: "rust".to_string(),
        }
    }

    fn anywhere() -> std::ops::Range<text::Anchor> {
        let buffer = text::BufferId::new(1).expect("a buffer id");
        text::Anchor::min_for_buffer(buffer)..text::Anchor::max_for_buffer(buffer)
    }

    #[test]
    fn a_name_declared_in_several_files_is_one_thing_to_type() {
        let completions = completions_for(
            vec![
                declared("read_one", "function_item", "src/one.rs"),
                declared("read_one", "function_item", "src/two.rs"),
                declared("read_two", "function_item", "src/two.rs"),
            ],
            anywhere(),
        );
        let typed: Vec<&str> = completions
            .iter()
            .map(|completion| completion.new_text.as_str())
            .collect();
        assert_eq!(typed, vec!["read_one", "read_two"]);
    }

    #[test]
    fn what_is_typed_is_the_name_and_what_is_shown_says_what_it_is() {
        let completions = completions_for(
            vec![declared("Thing", "struct_item", "src/one.rs")],
            anywhere(),
        );
        let only = completions.first().expect("one completion");
        assert_eq!(only.new_text, "Thing");
        assert!(
            only.label.text().starts_with("Thing"),
            "the name comes first in {:?}",
            only.label.text()
        );
        assert!(
            only.label.text().contains("struct_item"),
            "what it is is shown beside it in {:?}",
            only.label.text()
        );
    }
}
