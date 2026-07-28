use editor::Editor;
use gpui::{App, Entity, Window};
use html_preview::html_preview_view::HtmlPreviewView;
use markdown_preview::markdown_preview_view::{MarkdownPreviewMode, MarkdownPreviewView};
use openapi_preview::{OpenApiPreviewView, looks_like_openapi};
use ui::prelude::*;
use workspace::Workspace;

use crate::split_preview_view::{PreviewLayout, SplitPreviewView};
use crate::{OpenSplitPreview, split_preview_view};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewKind {
    Markdown,
    Html,
    OpenApi,
}

/// Which preview, if any, can render the document the editor is showing.
/// The language decides for Markdown and HTML; an OpenAPI contract is a plain
/// YAML or JSON file, so its content has to be inspected as well.
pub fn preview_kind_for(editor: &Entity<Editor>, cx: &App) -> Option<PreviewKind> {
    let multi_buffer = editor.read(cx).buffer().read(cx);
    let buffer = multi_buffer.as_singleton()?;
    let language_name = buffer.read(cx).language().map(|language| language.name());

    match language_name.as_ref().map(|name| name.as_ref()) {
        Some("Markdown") => Some(PreviewKind::Markdown),
        Some("HTML") => Some(PreviewKind::Html),
        Some("YAML") | Some("JSON") | Some("JSONC") => {
            looks_like_openapi(&buffer.read(cx).text()).then_some(PreviewKind::OpenApi)
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
    let Some(kind) = preview_kind_for(&source_editor, cx) else {
        return;
    };

    let pane = workspace.active_pane().clone();
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
            cx.new(|cx| SplitPreviewView::new(editor, preview, PreviewLayout::EditorAndPreview, cx))
        }
        PreviewKind::Html => {
            let preview = HtmlPreviewView::new(
                editor.clone(),
                workspace_handle,
                language_registry,
                window,
                cx,
            );
            cx.new(|cx| SplitPreviewView::new(editor, preview, PreviewLayout::EditorAndPreview, cx))
        }
        PreviewKind::OpenApi => {
            let preview = OpenApiPreviewView::new(editor.clone(), window, cx);
            cx.new(|cx| SplitPreviewView::new(editor, preview, PreviewLayout::EditorAndPreview, cx))
        }
    };

    pane.update(cx, |pane, cx| {
        pane.add_item(Box::new(view), true, true, None, window, cx);
    });
}

fn existing_index_for(
    pane: &Entity<workspace::Pane>,
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
