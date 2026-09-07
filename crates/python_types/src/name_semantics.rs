use std::ops::Range;
use std::panic::AssertUnwindSafe;

use gpui::{App, AppContext as _, Entity, Task};
use language::Buffer;
use project::{InProcessSemantics, InProcessSemanticsContext, InProcessSpan};
use ruff_db::files::{File, system_path_to_file};
use ruff_db::source::source_text;
use ruff_text_size::TextRange;
use text::PointUtf16;
use ty_project::{ProjectDatabase, SemanticDb as _};

use crate::{Asked, PythonProject, TypesFromTy, what_was_asked};

/// Runs one query against a project's database with the open buffers' text in
/// place, and returns nothing rather than propagating a panic.
///
/// A resolution query that falls over is one lookup that has no answer, and
/// must not be the end of the editor the reader is working in; the caller then
/// falls through to the symbol index, which is what answered before this.
fn asking<Answer>(
    project: &PythonProject,
    asked: &Asked,
    ask: impl FnOnce(&ProjectDatabase, File) -> Option<Answer>,
) -> Option<Answer> {
    let mut database = project.database.lock().ok()?;
    if project.open.record(&asked.path, asked.text.clone()) {
        File::sync_path(&mut *database, &asked.path);
    }
    let database = &*database;

    std::panic::catch_unwind(AssertUnwindSafe(|| {
        let file = system_path_to_file(database, &asked.path).ok()?;
        ask(database, file)
    }))
    .ok()
    .flatten()
}

/// The span a file and a range name, with the text that range held when `ty`
/// read it.
///
/// Nothing for a range in a file that has no path on this system -- the
/// standard library `ty` carries inside the binary is read from there, and a
/// reader cannot be sent to a file that is not on disk.
fn span_of(database: &ProjectDatabase, file: File, range: TextRange) -> Option<InProcessSpan> {
    let path = file
        .path(database)
        .as_system_path()?
        .as_std_path()
        .to_path_buf();
    let range = usize::from(range.start())..usize::from(range.end());
    let source = source_text(database, file);
    let text = source.as_str().get(range.clone())?.to_string();
    Some(InProcessSpan { path, range, text })
}

/// Every span `ty` named, or nothing where it named none this editor can open.
fn spans_of(
    database: &ProjectDatabase,
    named: impl IntoIterator<Item = (File, TextRange)>,
) -> Option<Vec<InProcessSpan>> {
    let spans: Vec<InProcessSpan> = named
        .into_iter()
        .filter_map(|(file, range)| span_of(database, file, range))
        .collect();
    (!spans.is_empty()).then_some(spans)
}

pub(crate) fn definitions_of(project: &PythonProject, asked: &Asked) -> Option<Vec<InProcessSpan>> {
    asking(project, asked, |database, file| {
        let found = ty_ide::goto_definition(database, database.program_file(file), asked.offset)?;
        spans_of(
            database,
            found
                .value
                .into_iter()
                .map(|target| (target.file(), target.focus_range())),
        )
    })
}

pub(crate) fn references_of(project: &PythonProject, asked: &Asked) -> Option<Vec<InProcessSpan>> {
    asking(project, asked, |database, file| {
        let found =
            ty_ide::find_references(database, database.program_file(file), asked.offset, true)?;
        spans_of(
            database,
            found
                .into_iter()
                .map(|target| (target.file(), target.range())),
        )
    })
}

fn rename_range_of(project: &PythonProject, asked: &Asked) -> Option<Range<usize>> {
    asking(project, asked, |database, file| {
        let range = ty_ide::can_rename(database, database.program_file(file), asked.offset)?;
        Some(usize::from(range.start())..usize::from(range.end()))
    })
}

pub(crate) fn rename_of(
    project: &PythonProject,
    asked: &Asked,
    new_name: &str,
) -> Option<Vec<InProcessSpan>> {
    asking(project, asked, |database, file| {
        let found = ty_ide::rename(
            database,
            database.program_file(file),
            asked.offset,
            new_name,
        )?;
        spans_of(
            database,
            found
                .into_iter()
                .map(|target| (target.file(), target.range())),
        )
    })
}

impl TypesFromTy {
    /// The project holding that buffer, and everything about the question that
    /// can only be read on the foreground thread.
    fn about(
        &self,
        context: &InProcessSemanticsContext,
        buffer: &Entity<Buffer>,
        position: PointUtf16,
        cx: &App,
    ) -> Option<(std::sync::Arc<PythonProject>, Asked)> {
        let asked = what_was_asked(&context.worktree_roots, buffer, position, cx)?;
        let project = self.project_for(&asked.root)?;
        Some((project, asked))
    }
}

impl InProcessSemantics for TypesFromTy {
    fn definitions(
        &self,
        context: &InProcessSemanticsContext,
        buffer: &Entity<Buffer>,
        position: PointUtf16,
        cx: &mut App,
    ) -> Task<Option<Vec<InProcessSpan>>> {
        let Some((project, asked)) = self.about(context, buffer, position, cx) else {
            return Task::ready(None);
        };
        cx.background_spawn(async move { definitions_of(&project, &asked) })
    }

    fn references(
        &self,
        context: &InProcessSemanticsContext,
        buffer: &Entity<Buffer>,
        position: PointUtf16,
        cx: &mut App,
    ) -> Task<Option<Vec<InProcessSpan>>> {
        let Some((project, asked)) = self.about(context, buffer, position, cx) else {
            return Task::ready(None);
        };
        cx.background_spawn(async move { references_of(&project, &asked) })
    }

    fn rename_range(
        &self,
        context: &InProcessSemanticsContext,
        buffer: &Entity<Buffer>,
        position: PointUtf16,
        cx: &mut App,
    ) -> Task<Option<Range<usize>>> {
        let Some((project, asked)) = self.about(context, buffer, position, cx) else {
            return Task::ready(None);
        };
        cx.background_spawn(async move { rename_range_of(&project, &asked) })
    }

    fn rename(
        &self,
        context: &InProcessSemanticsContext,
        buffer: &Entity<Buffer>,
        position: PointUtf16,
        new_name: String,
        cx: &mut App,
    ) -> Task<Option<Vec<InProcessSpan>>> {
        let Some((project, asked)) = self.about(context, buffer, position, cx) else {
            return Task::ready(None);
        };
        cx.background_spawn(async move { rename_of(&project, &asked, &new_name) })
    }
}
