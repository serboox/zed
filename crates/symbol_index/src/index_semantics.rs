use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use anyhow::Result;
use collections::{HashMap, HashSet};
use editor::{Editor, SemanticsProvider};
use futures::future::Shared;
use gpui::{App, AppContext as _, Entity, Task, WeakEntity, Window};
use language::{Buffer, BufferId, BufferRow};
use project::{
    DocumentHighlight, InlayHint, InvalidationStrategy, LocationLink, Project, ProjectTransaction,
    lsp_store::{BufferSemanticTokens, CacheInlayHints, RefreshForServer},
};
use semantic_index::definitions::Definition;
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

    /// Where the index says the name under the cursor is declared, worked out
    /// while the buffer and the index are both in hand.
    ///
    /// The same gate as everywhere else here: nothing for a name the index will
    /// not resolve, and nothing for a name it declares more than once. Landing
    /// a reader in the wrong file is worse than not moving them.
    fn where_the_index_says(
        &self,
        buffer: &Entity<Buffer>,
        position: text::Anchor,
        cx: &mut App,
    ) -> Option<Declared> {
        let index = self.index.upgrade()?;
        let snapshot = buffer.read(cx).snapshot();
        let offset = position.to_offset(&snapshot);
        let (range, name) = word_at(&snapshot, offset)?;

        let (path, declared) = {
            let index = index.read(cx);
            let WhatItMeans::TheseAre(_) = index.what_a_name_means(&name)? else {
                return None;
            };
            index.where_declared(&name)?
        };
        Some(Declared {
            path,
            declared,
            origin: language::Location {
                buffer: buffer.clone(),
                range: snapshot.anchor_before(range.start)..snapshot.anchor_after(range.end),
            },
        })
    }

    /// What the index can say about the name under the cursor: the line that
    /// declares it, the comment written above that line, and where it lives.
    ///
    /// Not a type -- nothing here infers one, and section 05 of the plan says
    /// why that is not coming for every language. It is the rest of what a
    /// reader hovers a name to find out, and today a reader with no language
    /// server running gets none of it.
    fn declaration_card(
        &self,
        buffer: &Entity<Buffer>,
        position: text::Anchor,
        cx: &mut App,
    ) -> Option<Task<Option<Vec<project::Hover>>>> {
        let index = self.index.upgrade()?;
        let snapshot = buffer.read(cx).snapshot();
        let offset = position.to_offset(&snapshot);
        let (range, name) = word_at(&snapshot, offset)?;

        let (path, declared) = {
            let index = index.read(cx);
            // The same gate the underlining goes through, and for the same
            // reason: the index declines a name that is also bound locally
            // somewhere, or is a member of a type, because it cannot tell this
            // occurrence of it from one of those. A card for the project's
            // `run` shown over a local named `run` is a confident wrong answer,
            // which is the only kind worth refusing outright.
            let WhatItMeans::TheseAre(_) = index.what_a_name_means(&name)? else {
                return None;
            };
            index.where_declared(&name)?
        };

        // The store holds what the file said when it was last read from disk.
        // Where the declaration is in the very buffer being edited, its line
        // number has moved and the card would quote the wrong line.
        let open_path = buffer
            .read(cx)
            .file()
            .map(|file| file.path().to_string().replace('\\', "/"));
        if buffer.read(cx).is_dirty() && open_path.as_deref() == Some(declared.path.as_str()) {
            return None;
        }

        let at = snapshot.anchor_before(range.start)..snapshot.anchor_after(range.end);
        Some(cx.background_spawn(async move {
            let contents = std::fs::read_to_string(&path).ok()?;
            let card = card_for(&contents, &declared)?;
            Some(vec![project::Hover {
                contents: card,
                range: Some(at),
                language: None,
            }])
        }))
    }
}

/// The blocks a declaration card is made of: the declaration as it is written,
/// the comment above it if there is one, and a last line saying what kind of
/// thing it is and where.
fn card_for(contents: &str, declared: &Definition) -> Option<Vec<project::HoverBlock>> {
    let lines: Vec<&str> = contents.lines().collect();
    // `Definition::line` is one-based, as a reader counts lines.
    let at = declared.line.checked_sub(1)? as usize;
    let declaration = lines.get(at)?.trim();
    if declaration.is_empty() {
        return None;
    }

    // Loaded once: `load_config` parses the language's config file, and the card
    // asks it two questions.
    let config = config_of(&declared.language);
    let mut blocks = vec![project::HoverBlock {
        text: declaration.to_string(),
        kind: match &config {
            // The name the language is registered under, which is what the
            // markdown fence this block becomes has to say for it to be
            // highlighted: `C#`, not `csharp`, and `Visual Basic`, not `vb6`.
            // The index records the directory name, which is neither.
            Some(config) => project::HoverBlockKind::Code {
                language: config.name.to_string(),
            },
            None => project::HoverBlockKind::PlainText,
        },
    }];
    if let Some(comment) = config
        .as_ref()
        .and_then(|config| comment_above(&lines, at, &config.line_comments))
    {
        blocks.push(project::HoverBlock {
            text: comment,
            kind: project::HoverBlockKind::Markdown,
        });
    }
    blocks.push(project::HoverBlock {
        text: format!("{} · {}:{}", declared.kind, declared.path, declared.line),
        kind: project::HoverBlockKind::PlainText,
    });
    Some(blocks)
}

/// The name of the call the cursor is inside, read backwards from it: past the
/// arguments already typed, to the `(` that opened the call, to the word before
/// that.
///
/// `chars` is the file's text ending at the cursor, in reverse -- which is the
/// only direction this can be answered in without parsing, and the only one a
/// buffer gives cheaply.
fn callee_before(chars: impl Iterator<Item = char>) -> Option<String> {
    // Far enough to cross a long argument list written over several lines, and
    // short enough that a cursor outside any call does not walk the whole file
    // on every keystroke.
    const AS_FAR_BACK_AS_A_CALL_IS_WORTH_LOOKING: usize = 2000;

    let mut depth = 0usize;
    let mut walked = 0usize;
    let mut chars = chars.peekable();
    loop {
        walked += 1;
        if walked > AS_FAR_BACK_AS_A_CALL_IS_WORTH_LOOKING {
            return None;
        }
        match chars.next()? {
            ')' => depth += 1,
            '(' => {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            // A statement cannot be half of a call: a `;` or a `}` before any
            // unclosed `(` means the cursor is not inside one at all.
            ';' | '}' | '{' => return None,
            _ => {}
        }
    }

    // Whitespace between the name and its `(` is allowed in most languages.
    while chars
        .peek()
        .is_some_and(|character| character.is_whitespace())
    {
        chars.next();
    }
    let mut backwards: Vec<char> = Vec::new();
    while chars
        .peek()
        .is_some_and(|character| character.is_alphanumeric() || *character == '_')
    {
        backwards.push(chars.next()?);
    }
    if backwards.is_empty() {
        return None;
    }
    Some(backwards.into_iter().rev().collect())
}

/// The label a signature popover shows for a declaration, and the comment
/// above it as its documentation.
fn signature_of(contents: &str, declared: &Definition) -> Option<(String, Option<String>)> {
    let lines: Vec<&str> = contents.lines().collect();
    let at = declared.line.checked_sub(1)? as usize;
    let declaration = lines.get(at)?.trim();
    if declaration.is_empty() {
        return None;
    }
    let documentation = config_of(&declared.language)
        .and_then(|config| comment_above(&lines, at, &config.line_comments));
    Some((declaration.to_string(), documentation))
}

/// A language's own config, or nothing for a name this binary has no directory
/// for -- which is what a row written by an older build of the index can hold,
/// and what would otherwise panic, since `load_config` does.
fn config_of(language: &str) -> Option<language::LanguageConfig> {
    grammars::embedded_languages()
        .iter()
        .any(|known| known.as_str() == language)
        .then(|| grammars::load_config(language))
}

/// The run of comment lines written directly above `at`, with their markers
/// taken off, or nothing where there is no comment there.
///
/// Which markers count comes from the language's own config -- the same one the
/// editor toggles comments with -- rather than from a list written here, so a
/// language whose comments start with `--` or `*>` is read as correctly as one
/// whose comments start with `//`.
fn comment_above(lines: &[&str], at: usize, markers: &[Arc<str>]) -> Option<String> {
    if markers.is_empty() {
        return None;
    }
    // The longest marker that fits, not the first: Rust's markers are `//`,
    // `///` and `//!` in that order, and stripping `///` with `//` leaves a
    // stray slash at the front of every doc comment a card shows.
    let strip = |line: &str| -> Option<String> {
        let trimmed = line.trim_start();
        markers
            .iter()
            .map(|marker| marker.trim_end())
            .filter(|marker| trimmed.starts_with(marker))
            .max_by_key(|marker| marker.len())
            .map(|marker| trimmed[marker.len()..].trim_start().to_string())
    };

    let mut collected: Vec<String> = Vec::new();
    let mut above = at;
    while above > 0 {
        above -= 1;
        match strip(lines.get(above)?) {
            Some(text) => collected.push(text),
            None => break,
        }
    }
    if collected.is_empty() {
        return None;
    }
    collected.reverse();
    let joined = collected.join("\n").trim().to_string();
    (!joined.is_empty()).then_some(joined)
}

/// Where the index says the name under the cursor is declared, and the place
/// the reader asked from, kept together until it is known whether the file has
/// to be opened at all.
struct Declared {
    path: std::path::PathBuf,
    declared: Definition,
    origin: language::Location,
}

/// Opens the file the index named and points at the name on the line it
/// recorded, falling back to the start of that line where the name is no
/// longer written there.
async fn open_the_declaration(
    project: WeakEntity<Project>,
    where_it_is: Declared,
    cx: &mut gpui::AsyncApp,
) -> Result<Option<Vec<LocationLink>>> {
    let Declared {
        path,
        declared,
        origin,
    } = where_it_is;
    let opened = project
        .update(cx, |project, cx| project.open_local_buffer(&path, cx))?
        .await?;
    // The closure is handed the buffer itself, not the handle to it, so the
    // range is worked out inside and the handle is put beside it out here.
    let at = opened.read_with(cx, |opened, _| {
        let snapshot = opened.snapshot();
        // `Definition::line` is one-based, as a reader counts lines.
        let row = declared.line.saturating_sub(1);
        let at = name_on_line(&snapshot, row, &declared.name)
            .or_else(|| point_of(&snapshot, row, 0).map(|start| start..start))?;
        Some(snapshot.anchor_before(at.start)..snapshot.anchor_after(at.end))
    });
    let target = at.map(|range| language::Location {
        buffer: opened.clone(),
        range,
    });
    let Some(target) = target else {
        // The file has moved under the index and no longer has that line.
        // Answering nothing sends the reader nowhere, which is the right place.
        return Ok(None);
    };
    Ok(Some(vec![LocationLink {
        origin: Some(origin),
        target,
    }]))
}

/// How many places the index may be asked to open buffers for. A name written
/// ten thousand times is not a list anybody reads, and opening a buffer per
/// file to build it is the expensive part.
const MOST_PLACES_WORTH_OPENING: usize = 1000;

/// The offset a row and column stand for, or nothing where the file has moved
/// under the index and that place is no longer in it.
fn point_of(snapshot: &language::BufferSnapshot, row: u32, column: u32) -> Option<usize> {
    let point = language::Point::new(row, column);
    if point > snapshot.max_point() {
        return None;
    }
    Some(snapshot.point_to_offset(point))
}

/// Where `name` sits on `row`, so that going to a definition lands on the name
/// rather than on the indentation in front of it.
///
/// Bounded: a generated file can hold a line of any length, and reading one to
/// find a name that a caller already has another place to put is not worth it.
fn name_on_line(snapshot: &language::BufferSnapshot, row: u32, name: &str) -> Option<Range<usize>> {
    const A_LINE_WORTH_SEARCHING: usize = 2000;
    let start = point_of(snapshot, row, 0)?;
    let mut line = String::new();
    for character in snapshot.chars_at(start) {
        if character == '\n' || line.len() >= A_LINE_WORTH_SEARCHING {
            break;
        }
        line.push(character);
    }
    // Not the first substring: on `func (s *Server) Serve() {`, the name
    // `Serve` first appears inside `Server`, and a reader sent there lands in
    // the receiver type rather than on the method.
    let is_word = |character: char| character.is_alphanumeric() || character == '_';
    let mut from = 0;
    while let Some(at) = line[from..].find(name) {
        let at = from + at;
        let before = line[..at].chars().next_back();
        let after = line[at + name.len()..].chars().next();
        if !before.is_some_and(is_word) && !after.is_some_and(is_word) {
            return Some(start + at..start + at + name.len());
        }
        // Past the whole name: `at + 1` is not always a character boundary,
        // and slicing there panics.
        from = at + name.len();
    }
    None
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
        let from_the_server = self.project.hover(buffer, position, cx);
        let Some(card) = self.declaration_card(buffer, position, cx) else {
            return from_the_server;
        };
        let Some(from_the_server) = from_the_server else {
            return Some(card);
        };
        // The server first, always: it knows the type, and the index does not.
        // The card is what fills the silence where no server was started, which
        // in this fork is the ordinary case rather than the exception.
        Some(cx.background_spawn(async move {
            match from_the_server.await {
                Some(answered) if !answered.iter().all(project::Hover::is_empty) => Some(answered),
                _ => card.await,
            }
        }))
    }

    fn references(
        &self,
        buffer: &Entity<Buffer>,
        position: text::Anchor,
        cx: &mut App,
    ) -> Option<Task<Result<Option<Vec<language::Location>>>>> {
        let index = self.index.upgrade()?;
        // The weak handle is kept rather than upgraded: it is the one whose
        // `update` answers with a `Result`, which is what a task running after
        // the project may have gone needs.
        let project = self.project.clone();
        let snapshot = buffer.read(cx).snapshot();
        let offset = position.to_offset(&snapshot);
        let (_, name) = word_at(&snapshot, offset)?;
        // The same refusal the underlining makes, and for the same reason: an
        // unsaved edit has moved every row below it, and a stale row that lands
        // on another occurrence of the same name passes the text check below
        // and is reported as a place the reader never asked about.
        if buffer.read(cx).is_dirty() {
            return None;
        }

        let places = {
            let index = index.read(cx);
            let WhatItMeans::TheseAre(places) = index.what_a_name_means(&name)? else {
                return None;
            };
            places
        };
        if places.is_empty() || places.len() > MOST_PLACES_WORTH_OPENING {
            return None;
        }
        let root = index.read(cx).root().to_path_buf();

        Some(cx.spawn(async move |cx| {
            let mut found = Vec::new();
            let mut opened: HashMap<String, Entity<Buffer>> = HashMap::default();
            for place in places {
                let buffer = match opened.get(&place.path) {
                    Some(buffer) => buffer.clone(),
                    None => {
                        let full = root.join(&place.path);
                        // A file the index recorded and the tree no longer has
                        // is one place lost, not the whole answer: the rest are
                        // still where they were said to be.
                        let Ok(opening) =
                            project.update(cx, |project, cx| project.open_local_buffer(&full, cx))
                        else {
                            continue;
                        };
                        let Ok(buffer) = opening.await else {
                            continue;
                        };
                        opened.insert(place.path.clone(), buffer.clone());
                        buffer
                    }
                };
                let range = buffer.read_with(cx, |buffer, _| {
                    let snapshot = buffer.snapshot();
                    let from = point_of(&snapshot, place.row, place.column)?;
                    let to = point_of(&snapshot, place.row, place.column + name.len() as u32)?;
                    // The store holds what the file said when it was last read.
                    // A place whose text is no longer the name is a row that has
                    // moved, and pointing a reader at it would send them to a
                    // word they did not ask about.
                    (snapshot.text_for_range(from..to).collect::<String>() == name)
                        .then(|| snapshot.anchor_before(from)..snapshot.anchor_after(to))
                });
                let Some(range) = range else {
                    continue;
                };
                found.push(language::Location { buffer, range });
            }
            // Every place having moved is a store that has fallen behind the
            // files, and an empty answer reads as "no references" rather than
            // "ask somebody else"; `None` is the one that says the latter.
            if found.is_empty() {
                return Ok(None);
            }
            Ok(Some(found))
        }))
    }

    fn signature_help(
        &self,
        buffer: &Entity<Buffer>,
        position: text::Anchor,
        cx: &mut App,
    ) -> Option<Task<Option<Vec<project::lsp_command::SignatureHelp>>>> {
        let index = self.index.upgrade()?;
        let project = self.project.upgrade()?;
        let snapshot = buffer.read(cx).snapshot();
        let offset = position.to_offset(&snapshot);
        let name = callee_before(snapshot.reversed_chars_at(offset))?;

        let (path, declared) = {
            let index = index.read(cx);
            let WhatItMeans::TheseAre(_) = index.what_a_name_means(&name)? else {
                return None;
            };
            index.where_declared(&name)?
        };
        let languages = project.read(cx).languages().clone();

        Some(cx.spawn(async move |cx| {
            let contents = cx
                .background_spawn(async move { std::fs::read_to_string(&path).ok() })
                .await?;
            let (label, documentation) = signature_of(&contents, &declared)?;
            cx.update(|cx| {
                project::lsp_command::SignatureHelp::new(
                    lsp::SignatureHelp {
                        signatures: vec![lsp::SignatureInformation {
                            label,
                            documentation: documentation.map(lsp::Documentation::String),
                            parameters: None,
                            active_parameter: None,
                        }],
                        active_signature: Some(0),
                        active_parameter: None,
                    },
                    Some(languages),
                    None,
                    cx,
                )
                .map(|help| vec![help])
            })
        }))
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
        let from_the_server = self.project.definitions(buffer, position, kind, cx);
        // Only "where is this declared". A type's definition, an
        // implementation, and a declaration held apart from its definition are
        // all questions about types, and the index knows none of them.
        if kind != editor::GotoDefinitionKind::Symbol {
            return from_the_server;
        }
        // Worked out now, while the buffer and the index are both in hand, but
        // the file it names is opened only if the server has nothing: opening a
        // buffer per lookup in a project that has a server is work started and
        // thrown away.
        let Some(where_it_is) = self.where_the_index_says(buffer, position, cx) else {
            // Nothing of our own to say, and dropping the task here would drop
            // the server's answer with it.
            return from_the_server;
        };
        let project = self.project.clone();
        let Some(from_the_server) = from_the_server else {
            return Some(
                cx.spawn(async move |cx| open_the_declaration(project, where_it_is, cx).await),
            );
        };
        Some(cx.spawn(async move |cx| match from_the_server.await {
            Ok(Some(found)) if !found.is_empty() => Ok(Some(found)),
            _ => open_the_declaration(project, where_it_is, cx).await,
        }))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn declared(line: u32, language: &str) -> Definition {
        Definition {
            path: "src/one.rs".to_string(),
            name: "work".to_string(),
            kind: "function_item".to_string(),
            line,
            language: language.to_string(),
        }
    }

    #[gpui::test]
    fn the_name_on_a_line_is_found_where_it_actually_sits(cx: &mut gpui::TestAppContext) {
        let buffer = cx.new(|cx| language::Buffer::local("mod one;\n\n    pub fn work() {}\n", cx));
        let snapshot = buffer.read_with(cx, |buffer, _| buffer.snapshot());

        let at = name_on_line(&snapshot, 2, "work").expect("the name is on that line");
        assert_eq!(
            snapshot.text_for_range(at.clone()).collect::<String>(),
            "work"
        );
        // The name's own column, not the start of the line it is indented on --
        // which is what a reader would be sent to otherwise.
        let line_starts_at = point_of(&snapshot, 2, 0).expect("the row is in the file");
        assert!(at.start > line_starts_at, "{at:?} against {line_starts_at}");

        assert!(
            name_on_line(&snapshot, 99, "work").is_none(),
            "a row the file no longer has"
        );
        assert!(
            name_on_line(&snapshot, 0, "work").is_none(),
            "a name that is not on that line"
        );
    }

    #[gpui::test]
    fn a_name_inside_a_longer_word_is_not_the_name(cx: &mut gpui::TestAppContext) {
        // The receiver type holds `Serve` inside `Server`, before the method
        // of that name -- a reader sent to the first substring lands in the
        // wrong token on the right line.
        let buffer = cx.new(|cx| language::Buffer::local("func (s *Server) Serve() {\n", cx));
        let snapshot = buffer.read_with(cx, |buffer, _| buffer.snapshot());

        let at = name_on_line(&snapshot, 0, "Serve").expect("the method is on that line");
        assert_eq!(
            snapshot.text_for_range(at.clone()).collect::<String>(),
            "Serve"
        );
        let text = snapshot.text();
        assert_eq!(
            at.start,
            text.rfind("Serve").expect("the method"),
            "the method, not the receiver type it is spelled inside"
        );
    }

    #[test]
    fn a_card_quotes_the_declaration_its_comment_and_where_it_lives() {
        let file = "\
use std::io;

/// Does the work.
/// Twice, if asked.
pub fn work(times: u32) -> io::Result<()> {
    Ok(())
}
";
        let card = card_for(file, &declared(5, "rust")).expect("a card");
        assert_eq!(card[0].text, "pub fn work(times: u32) -> io::Result<()> {");
        assert_eq!(
            card[0].kind,
            project::HoverBlockKind::Code {
                language: "Rust".to_string()
            }
        );
        assert_eq!(card[1].text, "Does the work.\nTwice, if asked.");
        assert_eq!(card[1].kind, project::HoverBlockKind::Markdown);
        assert_eq!(card[2].text, "function_item · src/one.rs:5");
    }

    #[test]
    fn a_declaration_with_no_comment_above_it_is_still_a_card() {
        let file = "pub fn work() {}\n";
        let card = card_for(file, &declared(1, "rust")).expect("a card");
        assert_eq!(
            card.len(),
            2,
            "the declaration and where it lives: {card:?}"
        );
        assert_eq!(card[0].text, "pub fn work() {}");
    }

    #[test]
    fn a_line_the_file_no_longer_has_is_no_card_rather_than_a_panic() {
        let file = "pub fn work() {}\n";
        assert!(card_for(file, &declared(9, "rust")).is_none());
        assert!(card_for(file, &declared(0, "rust")).is_none());
    }

    fn markers_of(language: &str) -> Vec<Arc<str>> {
        config_of(language)
            .expect("a language this binary ships")
            .line_comments
    }

    #[test]
    fn the_comment_marker_comes_from_the_language_and_not_from_a_list_here() {
        // Two languages whose comments start with neither `//` nor `#`.
        let sql = vec!["-- Every customer we bill.", "CREATE TABLE customers ("];
        assert_eq!(
            comment_above(&sql, 1, &markers_of("sql")).as_deref(),
            Some("Every customer we bill.")
        );
        let bash = vec!["# Prepares the tree.", "prepare() {"];
        assert_eq!(
            comment_above(&bash, 1, &markers_of("bash")).as_deref(),
            Some("Prepares the tree.")
        );
        // The same lines read as a language whose comments start with `//`.
        assert_eq!(comment_above(&sql, 1, &markers_of("rust")), None);
    }

    #[test]
    fn only_the_run_directly_above_the_declaration_counts() {
        let lines = vec![
            "// About something else entirely.",
            "",
            "// The one that belongs to it.",
            "pub fn work() {}",
        ];
        assert_eq!(
            comment_above(&lines, 3, &markers_of("rust")).as_deref(),
            Some("The one that belongs to it.")
        );
    }

    #[test]
    fn a_language_this_binary_no_longer_ships_is_a_plain_card_rather_than_a_panic() {
        assert!(config_of("a-language-that-was-removed").is_none());
        let card = card_for(
            "pub fn work() {}\n",
            &declared(1, "a-language-that-was-removed"),
        )
        .expect("a card");
        assert_eq!(card[0].kind, project::HoverBlockKind::PlainText);
    }

    fn callee_in(text: &str) -> Option<String> {
        callee_before(text.chars().rev())
    }

    #[test]
    fn the_call_the_cursor_is_inside_is_named_by_reading_backwards() {
        assert_eq!(callee_in("let x = work(").as_deref(), Some("work"));
        assert_eq!(callee_in("let x = work(1, ").as_deref(), Some("work"));
        // Whitespace between the name and its bracket, and a call written over
        // several lines.
        assert_eq!(
            callee_in("let x = work (\n    1,\n    ").as_deref(),
            Some("work")
        );
        // An argument that is itself a finished call does not become the answer.
        assert_eq!(
            callee_in("let x = work(other(1), ").as_deref(),
            Some("work")
        );
        // The innermost unclosed call wins.
        assert_eq!(callee_in("let x = work(other(").as_deref(), Some("other"));
    }

    #[test]
    fn a_cursor_that_is_not_inside_a_call_names_nothing() {
        assert_eq!(callee_in("let x = 1 + 2"), None);
        // A finished statement before any unclosed bracket.
        assert_eq!(callee_in("work(1);\nlet x = "), None);
        // A bracket with no name in front of it is not a call.
        assert_eq!(callee_in("let x = ("), None);
    }

    #[test]
    fn a_signature_is_the_declaration_and_the_comment_above_it() {
        let file = "\
/// Does the work.
pub fn work(times: u32) {}
";
        let (label, documentation) = signature_of(file, &declared(2, "rust")).expect("a signature");
        assert_eq!(label, "pub fn work(times: u32) {}");
        assert_eq!(documentation.as_deref(), Some("Does the work."));
    }
}
