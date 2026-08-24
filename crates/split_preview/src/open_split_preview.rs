use std::path::Path;

use editor::Editor;
use gpui::{App, Entity, Window};
use html_preview::html_preview_view::HtmlPreviewView;
use markdown_preview::markdown_preview_view::{MarkdownPreviewMode, MarkdownPreviewView};
use openapi_preview::{OpenApiPreviewView, looks_like_openapi};
use project::{Project, ProjectPath};
use svg_preview::svg_preview_view::{SvgPreviewMode, SvgPreviewView};
use ui::prelude::*;
use workspace::{Pane, Workspace};

use crate::split_preview_view::{PreviewLayout, SplitPreviewView};
use crate::{OpenSplitPreview, split_preview_view};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewKind {
    Markdown,
    Html,
    Svg,
    OpenApi,
}

/// How much of a document is read to tell whether it is an OpenAPI contract.
/// Matches the limit the check itself applies.
const SNIFF_CHARS: usize = 8 * 1024;

/// Which preview a file with this extension can be rendered by, if any. Only the
/// extension is read, so this answers for a file that has not been opened yet.
pub fn preview_kind_for_extension(extension: &str) -> Option<PreviewKind> {
    match extension.to_ascii_lowercase().as_str() {
        "md" | "markdown" => Some(PreviewKind::Markdown),
        "html" | "htm" => Some(PreviewKind::Html),
        "svg" => Some(PreviewKind::Svg),
        _ => None,
    }
}

fn preview_kind_for_name(name: &Path) -> Option<PreviewKind> {
    preview_kind_for_extension(name.extension()?.to_str()?)
}

/// Which preview, if any, can render the file at this path.
///
/// The name is all there is to go on: which item opens a file is settled before
/// a byte of it has been read, so a document whose kind shows only in its
/// contents -- an OpenAPI contract, which is a plain YAML or JSON file -- is not
/// one of these.
///
/// A file opened on its own, from the command line say, becomes a worktree of
/// itself, and then the path inside that worktree is empty and carries no name
/// at all. The worktree's own path is the file in that case, so it answers.
pub fn preview_kind_for_path(
    project: &Entity<Project>,
    path: &ProjectPath,
    cx: &App,
) -> Option<PreviewKind> {
    path.path
        .extension()
        .and_then(preview_kind_for_extension)
        .or_else(|| {
            let worktree_path = project
                .read(cx)
                .worktree_for_id(path.worktree_id, cx)?
                .read(cx)
                .abs_path();
            preview_kind_for_name(&worktree_path)
        })
}

/// Which preview, if any, can render the document the editor is showing.
/// The name decides for a document that has no language of its own -- an SVG is
/// XML as far as the editor is concerned -- and the language decides for
/// Markdown and HTML; an OpenAPI contract is a plain YAML or JSON file, so its
/// content has to be inspected as well.
pub fn preview_kind_for(editor: &Entity<Editor>, cx: &App) -> Option<PreviewKind> {
    let multi_buffer = editor.read(cx).buffer().read(cx);
    let buffer = multi_buffer.as_singleton()?;
    if let Some(file) = buffer.read(cx).file()
        && let Some(kind) = preview_kind_for_name(Path::new(file.file_name(cx)))
    {
        return Some(kind);
    }
    let language_name = buffer.read(cx).language().map(|language| language.name());

    match language_name.as_ref().map(|name| name.as_ref()) {
        Some("Markdown") => Some(PreviewKind::Markdown),
        Some("HTML") => Some(PreviewKind::Html),
        Some("YAML") | Some("JSON") | Some("JSONC") => {
            // Only the head of the file: the check itself reads no further, and
            // this runs from a render, where copying a whole document out of the
            // buffer costs that much on every frame.
            let head: String = buffer.read(cx).chars_at(0).take(SNIFF_CHARS).collect();
            looks_like_openapi(&head).then_some(PreviewKind::OpenApi)
        }
        _ => None,
    }
}

pub fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &OpenSplitPreview, window, cx| {
        open(workspace, window, cx);
    });
}

fn open(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(source_editor) = workspace
        .active_item(cx)
        .and_then(|item| item.act_as::<Editor>(cx))
    else {
        return;
    };
    let pane = workspace.active_pane().clone();
    open_for_editor(
        workspace,
        &pane,
        &source_editor,
        PreviewLayout::EditorAndPreview,
        window,
        cx,
    );
}

/// Opens `source_editor`'s document next to its preview, in place of the tab the
/// document is already in, so the reader stays on the same tab instead of
/// gaining a second one for the same file.
///
/// `pane` is the pane holding that tab. It is passed in rather than taken from
/// the workspace, because the document being previewed does not have to live in
/// the pane that currently has focus.
pub fn open_for_editor(
    workspace: &mut Workspace,
    pane: &Entity<Pane>,
    source_editor: &Entity<Editor>,
    layout: PreviewLayout,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(kind) = preview_kind_for(source_editor, cx) else {
        return;
    };
    let source_editor = source_editor.clone();

    let pane = pane.clone();
    // Reactivate an existing split preview for the same buffer instead of
    // stacking a second one on top of it.
    if let Some(existing) = existing_index_for(&pane, &source_editor, cx) {
        pane.update(cx, |pane, cx| {
            pane.activate_item(existing, true, true, window, cx);
        });
        return;
    }

    let Some(buffer) = source_editor.read(cx).buffer().read(cx).as_singleton() else {
        return;
    };
    let project = workspace.project().clone();
    // A separate editor over the same buffer: rendering one editor entity in
    // two tabs at once would make them fight over focus and scroll state, while
    // sharing the buffer keeps edits, undo history and the dirty marker in sync.
    let editor = cx.new(|cx| Editor::for_buffer(buffer, Some(project), window, cx));
    let language_registry = workspace.project().read(cx).languages().clone();
    let workspace_handle = workspace.weak_handle();

    // The tab the document is in now, so the split can take its place.
    let replaced = pane.read(cx).active_item().and_then(|item| {
        let index = pane.read(cx).index_for_item(item.as_ref())?;
        Some((item.item_id(), index))
    });

    let view = match kind {
        PreviewKind::Markdown => {
            let preview = MarkdownPreviewView::new(
                MarkdownPreviewMode::Default,
                editor.clone(),
                workspace_handle,
                language_registry,
                window,
                cx,
            );
            cx.new(|cx| SplitPreviewView::new(editor, preview, layout, cx))
        }
        PreviewKind::Html => {
            let preview = HtmlPreviewView::new(
                editor.clone(),
                workspace_handle,
                language_registry,
                window,
                cx,
            );
            cx.new(|cx| SplitPreviewView::new(editor, preview, layout, cx))
        }
        PreviewKind::Svg => {
            let multi_buffer = editor.read(cx).buffer().clone();
            let preview = SvgPreviewView::new(
                SvgPreviewMode::Default,
                multi_buffer,
                workspace_handle,
                window,
                cx,
            );
            cx.new(|cx| SplitPreviewView::new(editor, preview, layout, cx))
        }
        PreviewKind::OpenApi => {
            let preview = OpenApiPreviewView::new(editor.clone(), window, cx);
            cx.new(|cx| SplitPreviewView::new(editor, preview, layout, cx))
        }
    };

    // The pane counts a second tab for the same file as a duplicate and
    // activates the tab it already has instead of adding the new one, so the tab
    // being replaced has to be gone first. It is removed rather than closed:
    // closing runs the save path, which reloads a saveable buffer from disk and
    // would throw away edits the reader has not saved yet. Nothing is lost by
    // removing it -- the buffer itself lives on in the split.
    pane.update(cx, |pane, cx| {
        if let Some((item_id, _)) = replaced {
            pane.remove_item(item_id, false, false, window, cx);
        }
        pane.add_item(
            Box::new(view),
            true,
            true,
            replaced.map(|(_, index)| index),
            window,
            cx,
        );
    });
}

fn existing_index_for(
    pane: &Entity<Pane>,
    source_editor: &Entity<Editor>,
    cx: &App,
) -> Option<usize> {
    let target_buffer = source_editor.read(cx).buffer().read(cx).as_singleton()?;
    let pane = pane.read(cx);
    pane.items_of_type::<split_preview_view::SplitPreviewView>()
        .find(|view| {
            view.read(cx)
                .editor()
                .read(cx)
                .buffer()
                .read(cx)
                .as_singleton()
                .as_ref()
                == Some(&target_buffer)
        })
        .and_then(|view| pane.index_for_item(&view))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_document_is_recognised_whatever_the_spelling_of_its_extension() {
        assert_eq!(
            preview_kind_for_extension("md"),
            Some(PreviewKind::Markdown)
        );
        assert_eq!(
            preview_kind_for_extension("MD"),
            Some(PreviewKind::Markdown),
            "a name is not spelled one way"
        );
        assert_eq!(
            preview_kind_for_extension("markdown"),
            Some(PreviewKind::Markdown)
        );
        assert_eq!(preview_kind_for_extension("html"), Some(PreviewKind::Html));
        assert_eq!(preview_kind_for_extension("htm"), Some(PreviewKind::Html));
        assert_eq!(preview_kind_for_extension("SVG"), Some(PreviewKind::Svg));
    }

    #[test]
    fn nothing_else_is_taken_for_a_document_with_a_rendered_view() {
        assert_eq!(preview_kind_for_extension("rs"), None);
        assert_eq!(
            preview_kind_for_extension("yaml"),
            None,
            "a contract is only visible in what the file says, not in its name"
        );
        assert_eq!(preview_kind_for_extension("mdx"), None);
        assert_eq!(
            preview_kind_for_name(Path::new("Makefile")),
            None,
            "a file with no extension is not one"
        );
    }

    /// A file opened on its own becomes a worktree of itself, and the name of
    /// that worktree is the only name there is.
    #[test]
    fn a_file_opened_on_its_own_is_judged_by_the_worktree_it_became() {
        assert_eq!(
            preview_kind_for_name(Path::new("/home/reader/notes.md")),
            Some(PreviewKind::Markdown)
        );
    }
}
