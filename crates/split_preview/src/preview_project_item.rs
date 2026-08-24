use std::path::Path;

use anyhow::Result;
use editor::Editor;
use gpui::{App, AppContext as _, Entity, Task, WeakEntity, Window};
use html_preview::html_preview_view::HtmlPreviewView;
use language::Buffer;
use markdown_preview::markdown_preview_view::{MarkdownPreviewMode, MarkdownPreviewView};
use openapi_preview::OpenApiPreviewView;
use project::{Project, ProjectEntryId, ProjectPath};
use settings::{RegisterSetting, Settings};
use svg_preview::svg_preview_view::{SvgPreviewMode, SvgPreviewView};
use ui::prelude::*;
use workspace::Pane;
use workspace::invalid_item_view::InvalidItemView;

use crate::open_split_preview::{PreviewKind, preview_kind_for_path};
use crate::split_preview_view::{PreviewLayout, SplitPreviewView};

/// Whether a file with a rendered view opens showing it. The reader's own
/// answer, since it decides what every such file opens as.
#[derive(Clone, Copy, Debug, RegisterSetting)]
pub struct SplitPreviewSettings {
    pub open_in_preview: bool,
}

impl Settings for SplitPreviewSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        Self {
            open_in_preview: content.open_in_preview.unwrap_or(true),
        }
    }
}

pub fn opens_in_preview(cx: &App) -> bool {
    SplitPreviewSettings::get_global(cx).open_in_preview
}

/// A document opened by way of its rendered view. It carries the buffer the
/// editor half works on, and which preview the reader is going to see: that has
/// to be settled from the path, because the path is all there is before the file
/// has been read.
pub struct PreviewableDocument {
    buffer: Entity<Buffer>,
    kind: PreviewKind,
}

impl project::ProjectItem for PreviewableDocument {
    fn try_open(
        project: &Entity<Project>,
        path: &ProjectPath,
        cx: &mut App,
    ) -> Option<Task<Result<Entity<Self>>>> {
        if !opens_in_preview(cx) {
            return None;
        }
        let kind = preview_kind_for_path(project, path, cx)?;
        let buffer = project.update(cx, |project, cx| project.open_buffer(path.clone(), cx));
        Some(cx.spawn(async move |cx| {
            let buffer = buffer.await?;
            anyhow::Ok(cx.new(|_| PreviewableDocument { buffer, kind }))
        }))
    }

    fn entry_id(&self, cx: &App) -> Option<ProjectEntryId> {
        project::ProjectItem::entry_id(self.buffer.read(cx), cx)
    }

    fn project_path(&self, cx: &App) -> Option<ProjectPath> {
        project::ProjectItem::project_path(self.buffer.read(cx), cx)
    }

    /// Whether the document has unsaved edits is the buffer's own answer, and the
    /// tab gives it through the editor holding that buffer. This handle is never
    /// the one asked.
    fn is_dirty(&self) -> bool {
        false
    }
}

impl workspace::item::ProjectItem for SplitPreviewView {
    type Item = PreviewableDocument;

    fn for_project_item(
        project: Entity<Project>,
        pane: Option<&Pane>,
        document: Entity<PreviewableDocument>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (buffer, kind) = {
            let document = document.read(cx);
            (document.buffer.clone(), document.kind)
        };
        // The reader is shown the page and can step to the source from there,
        // over an editor on the same buffer, so an edit made there is the same
        // edit the page renders and the same one that has to be saved.
        let editor = cx.new(|cx| Editor::for_buffer(buffer, Some(project.clone()), window, cx));
        let language_registry = project.read(cx).languages().clone();
        let workspace = pane
            .map(|pane| pane.workspace().clone())
            .unwrap_or_else(WeakEntity::new_invalid);

        match kind {
            PreviewKind::Markdown => {
                let preview = MarkdownPreviewView::new(
                    MarkdownPreviewMode::Default,
                    editor.clone(),
                    workspace,
                    language_registry,
                    window,
                    cx,
                );
                Self::new(editor, preview, PreviewLayout::Preview, cx)
            }
            PreviewKind::Html => {
                let preview =
                    HtmlPreviewView::new(editor.clone(), workspace, language_registry, window, cx);
                Self::new(editor, preview, PreviewLayout::Preview, cx)
            }
            PreviewKind::Svg => {
                let multi_buffer = editor.read(cx).buffer().clone();
                // A page is built on the workspace, which is already borrowed
                // while the tab for an opening file is being built, so it is
                // taken once that borrow is over.
                cx.defer_in(window, move |this, window, cx| {
                    let Ok(preview) = workspace.update(cx, |workspace, cx| {
                        SvgPreviewView::new(
                            SvgPreviewMode::Default,
                            multi_buffer,
                            workspace.weak_handle(),
                            window,
                            cx,
                        )
                    }) else {
                        return;
                    };
                    this.install_preview(preview, PreviewLayout::Preview, window, cx);
                });
                Self::awaiting_preview(editor, cx)
            }
            PreviewKind::OpenApi => {
                let preview = OpenApiPreviewView::new(editor.clone(), window, cx);
                Self::new(editor, preview, PreviewLayout::Preview, cx)
            }
        }
    }

    /// A document that could not be read is not one to preview, and the reader
    /// gets the same explanation an editor would have given them.
    fn for_broken_project_item(
        abs_path: &Path,
        is_local: bool,
        error: &anyhow::Error,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<InvalidItemView> {
        Some(InvalidItemView::new(abs_path, is_local, error, window, cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ShowEditorOnly;
    use gpui::{TestAppContext, VisualTestContext};
    use serde_json::json;
    use util::path;
    use util::rel_path::rel_path;
    use workspace::{AppState, Workspace};

    const NOTES: &str = "# Notes\n\nSome prose.\n\n- one\n- two\n";
    const PAGE: &str = "<h1>Notes</h1>\n<p>Some prose.</p>\n";
    const DRAWING: &str = "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"40\" height=\"40\">\
         <rect width=\"40\" height=\"40\" fill=\"red\"/></svg>\n";

    /// A workspace over real files, with nothing open in it yet.
    async fn workspace_with<'a>(
        files: &[(&str, &str)],
        open_in_preview: bool,
        cx: &'a mut TestAppContext,
    ) -> (Entity<Workspace>, &'a mut VisualTestContext) {
        let app_state = cx.update(|cx| {
            let app_state = AppState::test(cx);
            editor::init(cx);
            crate::init(cx);
            gpui::UpdateGlobal::update_global(cx, |store: &mut settings::SettingsStore, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.open_in_preview = Some(open_in_preview);
                });
            });
            app_state
        });
        let tree = files
            .iter()
            .map(|(name, contents)| (name.to_string(), json!(contents)))
            .collect();
        app_state
            .fs
            .as_fake()
            .insert_tree(path!("/project"), serde_json::Value::Object(tree))
            .await;
        let project = Project::test(app_state.fs.clone(), [path!("/project").as_ref()], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        (workspace, cx)
    }

    /// Opens a file the way a reader opens one: through the project, so that what
    /// the tab holds is decided by the registry rather than by the test.
    async fn open(workspace: &Entity<Workspace>, name: &str, cx: &mut VisualTestContext) {
        let worktree_id = workspace.read_with(cx, |workspace, cx| {
            workspace
                .project()
                .read(cx)
                .worktrees(cx)
                .next()
                .expect("the project has a worktree")
                .read(cx)
                .id()
        });
        let opened = workspace.update_in(cx, |workspace, window, cx| {
            workspace.open_path((worktree_id, rel_path(name)), None, true, window, cx)
        });
        opened.await.expect("the file opens");
        cx.run_until_parked();
        draw(cx);
    }

    async fn open_file<'a>(
        name: &str,
        contents: &str,
        cx: &'a mut TestAppContext,
    ) -> (Entity<Workspace>, &'a mut VisualTestContext) {
        let (workspace, cx) = workspace_with(&[(name, contents)], true, cx).await;
        open(&workspace, name, cx).await;
        (workspace, cx)
    }

    /// Paints a frame without running the executor, so an assertion sees the
    /// layout that is actually on screen rather than internal state.
    fn draw(cx: &mut VisualTestContext) {
        cx.update(|window, cx| {
            window.refresh();
            window.draw(cx).clear();
        });
    }

    fn split_in(
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) -> Entity<SplitPreviewView> {
        workspace
            .read_with(cx, |workspace, cx| {
                workspace
                    .active_item(cx)
                    .and_then(|item| item.downcast::<SplitPreviewView>())
            })
            .expect("the file has to open in a tab holding its preview")
    }

    fn opens_as_a_plain_editor(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
        workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_item(cx)
                .and_then(|item| item.downcast::<Editor>())
                .is_some()
        })
    }

    /// The rendered document is what the reader is given, and the source is not on
    /// screen at all. Both halves are measured: a tab painting both would satisfy
    /// either check on its own.
    fn assert_only_the_preview_is_painted(cx: &mut VisualTestContext) {
        let preview = cx
            .debug_bounds("split-preview-preview")
            .expect("the preview has to be painted");
        assert!(
            preview.size.width > px(0.) && preview.size.height > px(0.),
            "the preview has to occupy real screen area, got {preview:?}"
        );
        assert!(
            cx.debug_bounds("split-preview-editor").is_none(),
            "the source must not be on screen: the picture comes first"
        );
    }

    #[gpui::test]
    async fn a_markdown_file_opens_showing_the_page(cx: &mut TestAppContext) {
        let (workspace, cx) = open_file("notes.md", NOTES, cx).await;

        let split = split_in(&workspace, cx);
        assert_eq!(
            split.read_with(cx, |split, _| split.layout()),
            PreviewLayout::Preview,
            "a document with a rendered view opens on that view"
        );
        assert_only_the_preview_is_painted(cx);

        // The page is parsed off the foreground, so it reaches the screen a frame
        // after the tab does.
        cx.run_until_parked();
        draw(cx);
        let content = cx
            .debug_bounds("markdown-preview-content")
            .expect("the page itself has to be painted, not an empty pane");
        assert!(
            content.size.height > px(20.),
            "the painted page has to have real height, got {content:?}"
        );
    }

    #[gpui::test]
    async fn a_page_opens_showing_the_page(cx: &mut TestAppContext) {
        let (workspace, cx) = open_file("page.html", PAGE, cx).await;

        let split = split_in(&workspace, cx);
        assert!(
            split
                .read_with(cx, |split, _| split.preview().clone())
                .downcast::<HtmlPreviewView>()
                .is_ok(),
            "an HTML file has to be given the page preview"
        );
        assert_only_the_preview_is_painted(cx);
    }

    #[gpui::test]
    async fn a_drawing_opens_showing_the_drawing(cx: &mut TestAppContext) {
        let (workspace, cx) = open_file("logo.svg", DRAWING, cx).await;

        let split = split_in(&workspace, cx);
        assert!(
            split
                .read_with(cx, |split, _| split.preview().clone())
                .downcast::<SvgPreviewView>()
                .is_ok(),
            "an SVG file has to end up with the drawing itself, \
             not the stand-in its tab starts with"
        );
        assert_eq!(
            split.read_with(cx, |split, _| split.layout()),
            PreviewLayout::Preview,
            "the drawing is what the tab shows once it has arrived"
        );
        assert_only_the_preview_is_painted(cx);
    }

    /// The way back to the source, from a tab that opened on the preview.
    #[gpui::test]
    async fn the_source_is_one_keystroke_away(cx: &mut TestAppContext) {
        let (workspace, cx) = open_file("notes.md", NOTES, cx).await;
        let split = split_in(&workspace, cx);

        cx.dispatch_action(ShowEditorOnly);
        cx.run_until_parked();
        draw(cx);

        assert_eq!(
            split.read_with(cx, |split, _| split.layout()),
            PreviewLayout::Editor
        );
        let editor = cx
            .debug_bounds("split-preview-editor")
            .expect("the source has to be painted once it is asked for");
        assert!(editor.size.width > px(0.) && editor.size.height > px(0.));
        assert!(
            cx.debug_bounds("split-preview-preview").is_none(),
            "the preview gives the screen up to the source"
        );
    }

    /// The same way back, from the one tab whose preview is not there when the
    /// tab is built: the drawing arrives a beat later, and the keystroke has to
    /// reach the tab all the same.
    #[gpui::test]
    async fn the_source_is_one_keystroke_away_from_a_drawing(cx: &mut TestAppContext) {
        let (workspace, cx) = open_file("logo.svg", DRAWING, cx).await;
        let split = split_in(&workspace, cx);

        cx.dispatch_action(ShowEditorOnly);
        cx.run_until_parked();
        draw(cx);

        assert_eq!(
            split.read_with(cx, |split, _| split.layout()),
            PreviewLayout::Editor
        );
        assert!(
            cx.debug_bounds("split-preview-editor").is_some(),
            "the source has to be painted once it is asked for"
        );
        assert!(
            cx.debug_bounds("split-preview-preview").is_none(),
            "the drawing gives the screen up to the source"
        );
    }

    #[gpui::test]
    async fn an_edit_typed_into_the_source_marks_the_tab_dirty(cx: &mut TestAppContext) {
        let (workspace, cx) = open_file("notes.md", NOTES, cx).await;
        let split = split_in(&workspace, cx);

        cx.dispatch_action(ShowEditorOnly);
        cx.run_until_parked();
        draw(cx);
        assert!(
            cx.debug_bounds("split-preview-editor").is_some(),
            "the source has to be on screen for typing to reach it"
        );

        cx.simulate_input("Z");
        cx.run_until_parked();
        draw(cx);

        assert!(
            split
                .read_with(cx, |split, cx| split.editor().read(cx).text(cx))
                .contains('Z'),
            "the keystroke has to land in the document the tab is holding"
        );
        assert_eq!(
            workspace.read_with(cx, |workspace, cx| workspace
                .active_item(cx)
                .map(|item| item.is_dirty(cx))),
            Some(true),
            "an edit made through the preview's own editor has to mark the tab dirty, \
             which only holds while the buffer is the file's own"
        );
    }

    #[gpui::test]
    async fn with_the_setting_off_a_document_opens_in_its_editor(cx: &mut TestAppContext) {
        let files = [
            ("notes.md", NOTES),
            ("page.html", PAGE),
            ("logo.svg", DRAWING),
        ];
        let (workspace, cx) = workspace_with(&files, false, cx).await;

        for (name, _) in files {
            open(&workspace, name, cx).await;
            assert!(
                opens_as_a_plain_editor(&workspace, cx),
                "with the setting off, {name} has to open as a plain editor"
            );
            assert!(
                cx.debug_bounds("split-preview-preview").is_none(),
                "with the setting off, nothing may be previewed for {name}"
            );
        }
    }

    #[gpui::test]
    async fn a_document_with_no_rendered_view_is_left_alone(cx: &mut TestAppContext) {
        let (workspace, cx) = open_file("main.rs", "fn main() {}\n", cx).await;

        assert!(
            opens_as_a_plain_editor(&workspace, cx),
            "source code has no rendered view, so its tab is the editor it always was"
        );
        assert!(cx.debug_bounds("split-preview-preview").is_none());
    }
}
