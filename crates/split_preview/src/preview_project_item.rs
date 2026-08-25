use std::path::Path;

use anyhow::{Context as _, Result};
use editor::{Editor, EditorEvent};
use gpui::{App, AppContext as _, Entity, Task, WeakEntity, Window};
use html_preview::html_preview_view::HtmlPreviewView;
use language::Buffer;
use markdown_preview::markdown_preview_view::{MarkdownPreviewMode, MarkdownPreviewView};
use openapi_preview::OpenApiPreviewView;
use project::{Project, ProjectEntryId, ProjectPath};
use settings::{RegisterSetting, Settings};
use svg_preview::svg_preview_view::{SvgPreviewMode, SvgPreviewView};
use ui::prelude::*;
use workspace::invalid_item_view::InvalidItemView;
use workspace::{ItemId, Pane, Workspace, WorkspaceId};

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

/// The tab for a previewable document: the same construction whether the
/// document is being opened now or brought back from the session before it.
fn preview_tab_for(
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    buffer: Entity<Buffer>,
    kind: PreviewKind,
    layout: PreviewLayout,
    window: &mut Window,
    cx: &mut Context<SplitPreviewView>,
) -> SplitPreviewView {
    // The reader is shown the page and can step to the source from there, over
    // an editor on the same buffer, so an edit made there is the same edit the
    // page renders and the same one that has to be saved.
    let editor = cx.new(|cx| Editor::for_buffer(buffer, Some(project.clone()), window, cx));
    let language_registry = project.read(cx).languages().clone();

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
            SplitPreviewView::new(editor, preview, layout, cx).of_kind(kind)
        }
        PreviewKind::Html => {
            let preview =
                HtmlPreviewView::new(editor.clone(), workspace, language_registry, window, cx);
            SplitPreviewView::new(editor, preview, layout, cx).of_kind(kind)
        }
        PreviewKind::Svg => {
            let multi_buffer = editor.read(cx).buffer().clone();
            // A page is built on the workspace, which is already borrowed while
            // the tab for an opening file is being built, so it is taken once
            // that borrow is over.
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
                this.install_preview(preview, layout, window, cx);
            });
            SplitPreviewView::awaiting_preview(editor, cx).of_kind(kind)
        }
        PreviewKind::OpenApi => {
            let preview = OpenApiPreviewView::new(editor.clone(), window, cx);
            SplitPreviewView::new(editor, preview, layout, cx).of_kind(kind)
        }
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
        let workspace = pane
            .map(|pane| pane.workspace().clone())
            .unwrap_or_else(WeakEntity::new_invalid);
        preview_tab_for(
            project,
            workspace,
            buffer,
            kind,
            PreviewLayout::Preview,
            window,
            cx,
        )
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

impl workspace::item::SerializableItem for SplitPreviewView {
    fn serialized_item_kind() -> &'static str {
        "SplitPreviewView"
    }

    /// Restores the tab as it was left: the same document, the same preview and
    /// the same layout. The preview is read back rather than worked out from the
    /// path, because a contract's kind only shows in its contents.
    fn deserialize(
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        workspace_id: WorkspaceId,
        item_id: ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        let db = persistence::SplitPreviewDb::global(cx);
        window.spawn(cx, async move |cx| {
            let (abs_path, kind, layout) = db
                .get_preview(item_id, workspace_id)?
                .context("no split preview was saved for this tab")?;
            let kind = PreviewKind::from_db(kind).context("unknown preview kind")?;
            let layout = PreviewLayout::from_db(layout);

            // Asked before a worktree is made for it: making one succeeds even
            // for a path with nothing at it, and would leave the project holding
            // a worktree over a document that is gone.
            let fs = project.read_with(cx, |project, _| project.fs().clone());
            anyhow::ensure!(
                fs.is_file(&abs_path).await,
                "the document is no longer where it was: {abs_path:?}"
            );
            let (worktree, relative_path) = project
                .update(cx, |project, cx| {
                    project.find_or_create_worktree(abs_path.clone(), false, cx)
                })
                .await
                .context("the document could not be opened where it was")?;
            let worktree_id = worktree.read_with(cx, |worktree, _| worktree.id());
            let buffer = project
                .update(cx, |project, cx| {
                    project.open_buffer(
                        ProjectPath {
                            worktree_id,
                            path: relative_path,
                        },
                        cx,
                    )
                })
                .await?;

            cx.update(|window, cx| {
                cx.new(|cx| preview_tab_for(project, workspace, buffer, kind, layout, window, cx))
            })
        })
    }

    fn cleanup(
        workspace_id: WorkspaceId,
        alive_items: Vec<ItemId>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let db = persistence::SplitPreviewDb::global(cx);
        workspace::delete_unloaded_items(alive_items, workspace_id, "split_previews", &db, cx)
    }

    fn serialize(
        &mut self,
        workspace: &mut Workspace,
        item_id: ItemId,
        _closing: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Task<Result<()>>> {
        let workspace_id = workspace.database_id()?;
        let buffer = self.editor().read(cx).buffer().read(cx).as_singleton()?;
        let file = buffer.read(cx).file()?;
        let worktree_id = file.worktree_id(cx);
        let abs_path = workspace
            .project()
            .read(cx)
            .worktree_for_id(worktree_id, cx)?
            .read(cx)
            .absolutize(file.path());
        let kind = self.preview_kind()?.to_db();
        let layout = self.layout().to_db();
        let db = persistence::SplitPreviewDb::global(cx);
        Some(cx.background_spawn(async move {
            db.save_preview(item_id, workspace_id, abs_path, kind, layout)
                .await
        }))
    }

    /// The tab's own title changes when the layout does and when the document is
    /// saved or renamed, which is exactly when what was written down has gone
    /// stale.
    fn should_serialize(&self, event: &Self::Event) -> bool {
        matches!(
            event,
            EditorEvent::TitleChanged | EditorEvent::FileHandleChanged | EditorEvent::Saved
        )
    }
}

mod persistence {
    use std::path::PathBuf;

    use db::{
        query,
        sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection},
        sqlez_macros::sql,
    };
    use workspace::{ItemId, WorkspaceDb, WorkspaceId};

    pub struct SplitPreviewDb(ThreadSafeConnection);

    impl Domain for SplitPreviewDb {
        const NAME: &str = stringify!(SplitPreviewDb);

        const MIGRATIONS: &[&str] = &[sql!(
            CREATE TABLE split_previews (
                workspace_id INTEGER,
                item_id INTEGER,
                abs_path BLOB,
                kind INTEGER NOT NULL DEFAULT 0,
                layout INTEGER NOT NULL DEFAULT 2,

                PRIMARY KEY(workspace_id, item_id),
                FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
                ON DELETE CASCADE
            ) STRICT;
        )];
    }

    db::static_connection!(SplitPreviewDb, [WorkspaceDb]);

    impl SplitPreviewDb {
        query! {
            pub async fn save_preview(
                item_id: ItemId,
                workspace_id: WorkspaceId,
                abs_path: PathBuf,
                kind: i64,
                layout: i64
            ) -> Result<()> {
                INSERT OR REPLACE INTO split_previews(item_id, workspace_id, abs_path, kind, layout)
                VALUES (?, ?, ?, ?, ?)
            }
        }

        query! {
            pub fn get_preview(item_id: ItemId, workspace_id: WorkspaceId) -> Result<Option<(PathBuf, i64, i64)>> {
                SELECT abs_path, kind, layout
                FROM split_previews
                WHERE item_id = ? AND workspace_id = ?
            }
        }
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
    use workspace::{AppState, MultiWorkspace, Workspace, WorkspaceId};

    const NOTES: &str = "# Notes\n\nSome prose.\n\n- one\n- two\n";
    const PAGE: &str = "<h1>Notes</h1>\n<p>Some prose.</p>\n";
    const DRAWING: &str = "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"40\" height=\"40\">\
         <rect width=\"40\" height=\"40\" fill=\"red\"/></svg>\n";
    const CONTRACT: &str = "openapi: 3.0.3\ninfo:\n  title: Sample\n  version: 1.0.0\npaths: {}\n";

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

    /// A workspace the session can be written down for: an item's row points at
    /// the workspace's own, so that one has to exist first.
    async fn a_workspace_that_remembers<'a>(
        files: &[(&str, &str)],
        cx: &'a mut TestAppContext,
    ) -> (
        Entity<Workspace>,
        std::sync::Arc<dyn project::Fs>,
        &'a mut VisualTestContext,
    ) {
        let app_state = cx.update(|cx| {
            let app_state = AppState::test(cx);
            editor::init(cx);
            crate::init(cx);
            gpui::UpdateGlobal::update_global(cx, |store: &mut settings::SettingsStore, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.open_in_preview = Some(true);
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
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
        let flushing = multi_workspace.update_in(cx, |multi, window, cx| {
            multi.flush_all_serialization(window, cx)
        });
        for task in flushing {
            task.await;
        }
        (workspace, app_state.fs.clone(), cx)
    }

    /// Writes the tab down and builds it again from nothing but what was
    /// written -- which is what a restart does.
    async fn through_a_restart(
        workspace: &Entity<Workspace>,
        split: &Entity<SplitPreviewView>,
        cx: &mut VisualTestContext,
    ) -> Entity<SplitPreviewView> {
        let written_down = write_the_tab_down(workspace, split, cx).await;
        restore_the_tab(workspace, written_down, cx)
            .await
            .expect("the tab comes back")
    }

    /// Writes one tab into the session's own record, the way the workspace does.
    async fn write_the_tab_down(
        workspace: &Entity<Workspace>,
        split: &Entity<SplitPreviewView>,
        cx: &mut VisualTestContext,
    ) -> (WorkspaceId, u64, Entity<Project>) {
        let (saving, workspace_id, item_id, project) =
            workspace.update_in(cx, |workspace, window, cx| {
                let workspace_id = workspace.database_id().expect("a database id");
                let item_id = split.entity_id().as_u64();
                let project = workspace.project().clone();
                let saving = split
                    .update(cx, |split, cx| {
                        workspace::item::SerializableItem::serialize(
                            split, workspace, item_id, false, window, cx,
                        )
                    })
                    .expect("a tab over a real file has something to write down");
                (saving, workspace_id, item_id, project)
            });
        saving.await.expect("the tab is written down");
        (workspace_id, item_id, project)
    }

    /// Builds the tab again from nothing but what was written down, which is
    /// what a restart does.
    async fn restore_the_tab(
        workspace: &Entity<Workspace>,
        written_down: (WorkspaceId, u64, Entity<Project>),
        cx: &mut VisualTestContext,
    ) -> anyhow::Result<Entity<SplitPreviewView>> {
        let (workspace_id, item_id, project) = written_down;
        let restoring = cx.update(|window, cx| {
            <SplitPreviewView as workspace::item::SerializableItem>::deserialize(
                project,
                workspace.downgrade(),
                workspace_id,
                item_id,
                window,
                cx,
            )
        });
        let restored = restoring.await;
        cx.run_until_parked();
        restored
    }

    /// A tab holding a page has to be part of the session, or a reader who
    /// restarts finds the document they were reading simply gone.
    #[gpui::test]
    async fn a_previewed_document_comes_back_after_a_restart(cx: &mut TestAppContext) {
        let (workspace, _fs, cx) = a_workspace_that_remembers(&[("notes.md", NOTES)], cx).await;
        open(&workspace, "notes.md", cx).await;
        let split = split_in(&workspace, cx);
        split.update_in(cx, |split, window, cx| {
            split.set_layout(PreviewLayout::EditorAndPreview, window, cx);
        });
        cx.run_until_parked();

        let kind = workspace
            .read_with(cx, |workspace, cx| {
                workspace
                    .active_item(cx)?
                    .to_serializable_item_handle(cx)
                    .map(|handle| handle.serialized_item_kind())
            })
            .expect("the tab has to be registered as part of the session");
        assert_eq!(kind, "SplitPreviewView");

        let restored = through_a_restart(&workspace, &split, cx).await;

        restored.read_with(cx, |restored, cx| {
            assert_eq!(
                restored.layout(),
                PreviewLayout::EditorAndPreview,
                "the layout the reader left the tab in has to come back with it"
            );
            assert_eq!(restored.preview_kind(), Some(PreviewKind::Markdown));
            assert!(
                restored
                    .preview()
                    .clone()
                    .downcast::<MarkdownPreviewView>()
                    .is_ok(),
                "and the page itself, not an editor standing in for it"
            );
            let opened_file = restored
                .editor()
                .read(cx)
                .buffer()
                .read(cx)
                .as_singleton()
                .and_then(|buffer| {
                    buffer
                        .read(cx)
                        .file()
                        .map(|file| file.file_name(cx).to_string())
                });
            assert_eq!(
                opened_file,
                Some("notes.md".to_string()),
                "over the document that was open, not another one"
            );
        });
    }

    /// A document that has since been deleted cannot be restored, and trying
    /// must not leave the project holding a worktree over nothing.
    #[gpui::test]
    async fn a_document_that_is_gone_leaves_no_worktree_behind(cx: &mut TestAppContext) {
        let (workspace, fs, cx) = a_workspace_that_remembers(&[("notes.md", NOTES)], cx).await;
        open(&workspace, "notes.md", cx).await;
        let split = split_in(&workspace, cx);
        let written_down = write_the_tab_down(&workspace, &split, cx).await;

        let worktrees_before = workspace.read_with(cx, |workspace, cx| {
            workspace.project().read(cx).worktrees(cx).count()
        });
        project::Fs::remove_file(
            fs.as_ref(),
            std::path::Path::new(path!("/project/notes.md")),
            Default::default(),
        )
        .await
        .expect("the document is deleted");
        cx.run_until_parked();

        let restored = restore_the_tab(&workspace, written_down, cx).await;
        assert!(
            restored.is_err(),
            "a document that is gone cannot be brought back"
        );
        assert_eq!(
            workspace.read_with(cx, |workspace, cx| workspace
                .project()
                .read(cx)
                .worktrees(cx)
                .count()),
            worktrees_before,
            "and no worktree may be left behind over the path it used to be at"
        );
    }

    /// The one kind whose preview cannot be worked out from the path: a contract
    /// is plain YAML, so the tab has to have written down what it was showing.
    #[gpui::test]
    async fn a_contract_comes_back_as_a_contract(cx: &mut TestAppContext) {
        let (workspace, _fs, cx) = a_workspace_that_remembers(&[("api.yaml", CONTRACT)], cx).await;
        open(&workspace, "api.yaml", cx).await;

        // A contract's kind shows only in its contents, so the tab for one is
        // built the way the reader's own request builds it.
        let editor = workspace
            .read_with(cx, |workspace, cx| {
                workspace
                    .active_item(cx)
                    .and_then(|item| item.downcast::<Editor>())
            })
            .expect("a YAML file opens as an editor");
        let buffer = editor
            .read_with(cx, |editor, cx| editor.buffer().read(cx).as_singleton())
            .expect("over one document");
        let split = workspace.update_in(cx, |workspace, window, cx| {
            let project = workspace.project().clone();
            let handle = workspace.weak_handle();
            let split = cx.new(|cx| {
                preview_tab_for(
                    project,
                    handle,
                    buffer,
                    PreviewKind::OpenApi,
                    PreviewLayout::Preview,
                    window,
                    cx,
                )
            });
            workspace.active_pane().update(cx, |pane, cx| {
                pane.add_item(Box::new(split.clone()), true, true, None, window, cx);
            });
            split
        });
        cx.run_until_parked();

        let restored = through_a_restart(&workspace, &split, cx).await;

        assert_eq!(
            restored.read_with(cx, |restored, _| restored.preview_kind()),
            Some(PreviewKind::OpenApi),
            "it comes back as the contract preview, which its path alone could \
             never have told it"
        );
        assert!(
            restored
                .read_with(cx, |restored, _| restored.preview().clone())
                .downcast::<OpenApiPreviewView>()
                .is_ok(),
            "and that is the view the tab actually holds"
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
