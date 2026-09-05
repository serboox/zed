use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use anyhow::Result;
use collections::{HashMap, HashSet};
use editor::{Editor, SemanticsProvider};
use futures::future::Shared;
use gpui::{App, Entity, Task, WeakEntity, Window};
use language::{Buffer, BufferId, BufferRow};
use project::{
    DocumentHighlight, InlayHint, InvalidationStrategy, LocationLink, Project, ProjectTransaction,
    lsp_store::{BufferSemanticTokens, CacheInlayHints, RefreshForServer},
};
use semantic_index::resolution::WhatItMeans;
use text::ToOffset as _;

/// Answers what a language server would, where the project's own index knows,
/// and hands everything else to the server.
///
/// One question is answered here today: which occurrences of the name under the
/// cursor to underline. It is asked on every cursor move, it needs nothing but
/// the file already open, and the index answers it from rows it worked out when
/// the file was last read -- so a reader who never opens a language server
/// still sees the highlight, and one who does spares the server a request per
/// keystroke.
///
/// Everything else is the project's, unchanged. A provider that answered half a
/// question would be worse than one that answers none: the editor cannot tell
/// an empty answer from an answer of nothing.
pub struct IndexFirst {
    project: WeakEntity<Project>,
    index: WeakEntity<crate::SymbolIndex>,
}

impl IndexFirst {
    /// The occurrences the index calls this name's, in the file that is open.
    ///
    /// `None` where the index will not say -- an ambiguous name, a member of a
    /// type, an index that has not been built -- and the caller then asks the
    /// server, which is what it did before this existed.
    fn highlights_from_the_index(
        &self,
        buffer: &Entity<Buffer>,
        position: text::Anchor,
        cx: &mut App,
    ) -> Option<Vec<DocumentHighlight>> {
        let index = self.index.upgrade()?;
        // What the store holds is what the file said when it was last read from
        // disk. A buffer with unsaved edits has moved every row below the edit,
        // so answering from the store would underline the wrong words -- and
        // underlining the wrong words is worse than asking the server, which is
        // what happens instead.
        if buffer.read(cx).is_dirty() {
            return None;
        }
        let snapshot = buffer.read(cx).snapshot();
        let offset = position.to_offset(&snapshot);
        let (_, name) = word_at(&snapshot, offset)?;
        let path = buffer
            .read(cx)
            .file()
            .map(|file| file.path().to_string().replace('\\', "/"))?;

        let answer = index.read(cx).what_a_symbol_means(Some(&path), &name)?;
        let WhatItMeans::TheseAre(places) = answer else {
            return None;
        };

        // Columns are byte offsets on both sides -- tree-sitter counts them
        // that way and so does the buffer -- so the end is the start plus the
        // name's length in bytes. Counting characters instead cuts a name with
        // any multi-byte letter in it short, and lands the anchor inside a
        // character rather than after it.
        let width = name.len() as u32;
        let mut highlights = Vec::new();
        for place in places {
            if place.path != path {
                continue;
            }
            let (Some(from), Some(to)) = (
                point_of(&snapshot, place.row, place.column),
                point_of(&snapshot, place.row, place.column + width),
            ) else {
                // A place the file no longer has is a store that has fallen
                // behind the file. One such place makes every place suspect, so
                // the answer is withheld rather than half given.
                return None;
            };
            highlights.push(DocumentHighlight {
                range: snapshot.anchor_before(from)..snapshot.anchor_after(to),
                kind: lsp::DocumentHighlightKind::TEXT,
            });
        }
        Some(highlights)
    }
}

/// The offset a row and column stand for, or nothing where the file has moved
/// under the index and that place is no longer in it.
fn point_of(snapshot: &language::BufferSnapshot, row: u32, column: u32) -> Option<usize> {
    let point = language::Point::new(row, column);
    if point > snapshot.max_point() {
        return None;
    }
    Some(snapshot.point_to_offset(point))
}

/// The word the cursor is in, and where it starts and ends.
fn word_at(snapshot: &language::BufferSnapshot, offset: usize) -> Option<(Range<usize>, String)> {
    let mut start = offset;
    let mut end = offset;
    let is_word = |character: char| character.is_alphanumeric() || character == '_';
    for character in snapshot.reversed_chars_at(start) {
        if !is_word(character) {
            break;
        }
        start -= character.len_utf8();
    }
    for character in snapshot.chars_at(end) {
        if !is_word(character) {
            break;
        }
        end += character.len_utf8();
    }
    if start == end {
        return None;
    }
    let name: String = snapshot.text_for_range(start..end).collect();
    Some((start..end, name))
}

impl SemanticsProvider for IndexFirst {
    fn document_highlights(
        &self,
        buffer: &Entity<Buffer>,
        position: text::Anchor,
        cx: &mut App,
    ) -> Option<Task<Result<Vec<DocumentHighlight>>>> {
        if let Some(found) = self.highlights_from_the_index(buffer, position, cx) {
            return Some(Task::ready(Ok(found)));
        }
        self.project.document_highlights(buffer, position, cx)
    }

    fn hover(
        &self,
        buffer: &Entity<Buffer>,
        position: text::Anchor,
        cx: &mut App,
    ) -> Option<Task<Option<Vec<project::Hover>>>> {
        self.project.hover(buffer, position, cx)
    }

    fn inline_values(
        &self,
        buffer_handle: Entity<Buffer>,
        range: Range<text::Anchor>,
        cx: &mut App,
    ) -> Option<Task<anyhow::Result<Vec<InlayHint>>>> {
        self.project.inline_values(buffer_handle, range, cx)
    }

    fn applicable_inlay_chunks(
        &self,
        buffer: &Entity<Buffer>,
        ranges: &[Range<text::Anchor>],
        cx: &mut App,
    ) -> Vec<Range<BufferRow>> {
        self.project.applicable_inlay_chunks(buffer, ranges, cx)
    }

    fn invalidate_inlay_hints(&self, for_buffers: &HashSet<BufferId>, cx: &mut App) {
        self.project.invalidate_inlay_hints(for_buffers, cx)
    }

    fn inlay_hints(
        &self,
        invalidate: InvalidationStrategy,
        buffer: Entity<Buffer>,
        ranges: Vec<Range<text::Anchor>>,
        known_chunks: Option<(clock::Global, HashSet<Range<BufferRow>>)>,
        cx: &mut App,
    ) -> Option<HashMap<Range<BufferRow>, Task<Result<CacheInlayHints>>>> {
        self.project
            .inlay_hints(invalidate, buffer, ranges, known_chunks, cx)
    }

    fn semantic_tokens(
        &self,
        buffer: Entity<Buffer>,
        refresh: Option<RefreshForServer>,
        cx: &mut App,
    ) -> Option<Shared<Task<std::result::Result<BufferSemanticTokens, Arc<anyhow::Error>>>>> {
        self.project.semantic_tokens(buffer, refresh, cx)
    }

    fn supports_inlay_hints(&self, buffer: &Entity<Buffer>, cx: &mut App) -> bool {
        self.project.supports_inlay_hints(buffer, cx)
    }

    fn supports_semantic_tokens(&self, buffer: &Entity<Buffer>, cx: &mut App) -> bool {
        self.project.supports_semantic_tokens(buffer, cx)
    }

    fn definitions(
        &self,
        buffer: &Entity<Buffer>,
        position: text::Anchor,
        kind: editor::GotoDefinitionKind,
        cx: &mut App,
    ) -> Option<Task<Result<Option<Vec<LocationLink>>>>> {
        self.project.definitions(buffer, position, kind, cx)
    }

    fn range_for_rename(
        &self,
        buffer: &Entity<Buffer>,
        position: text::Anchor,
        cx: &mut App,
    ) -> Task<Result<Option<Range<text::Anchor>>>> {
        self.project.range_for_rename(buffer, position, cx)
    }

    fn perform_rename(
        &self,
        buffer: &Entity<Buffer>,
        position: text::Anchor,
        new_name: String,
        cx: &mut App,
    ) -> Option<Task<Result<ProjectTransaction>>> {
        self.project.perform_rename(buffer, position, new_name, cx)
    }
}

/// Puts the index in front of the server for the questions it can answer.
///
/// Wired where every editor passes rather than at each place an editor is made:
/// there are many of those, and one of them forgetting would be a difference
/// nobody could see.
pub fn init(cx: &mut App) {
    cx.observe_new(|editor: &mut Editor, _window: Option<&mut Window>, cx| {
        let Some(project) = editor.project().cloned() else {
            return;
        };
        let Some(index) = crate::of_project(&project, cx) else {
            return;
        };
        editor.set_semantics_provider(Some(Rc::new(IndexFirst {
            project: project.downgrade(),
            index: index.downgrade(),
        })));
    })
    .detach();
}
