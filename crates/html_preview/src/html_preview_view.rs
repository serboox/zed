use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use editor::{Editor, EditorEvent, MultiBufferOffset};
use gpui::{
    App, Entity, EventEmitter, FocusHandle, Focusable, ImageSource, Resource, RetainAllImageCache,
    ScrollHandle, SharedUri, Subscription, Task, WeakEntity,
};
use html5ever::driver::ParseOpts;
use html5ever::parse_document;
use html5ever::serialize::SerializeOpts;
use html5ever::tendril::TendrilSink;
use html5ever::tree_builder::TreeBuilderOpts;
use language::LanguageRegistry;
use markdown::{
    CodeBlockRenderer, CopyButtonVisibility, Markdown, MarkdownElement, MarkdownOptions,
};
use markup5ever_rcdom::{Handle, NodeData, RcDom, SerializableHandle};
use settings::Settings;
use theme_settings::ThemeSettings;
use ui::utils::WithRemSize;
use ui::{WithScrollbar, prelude::*};
use workspace::item::Item;
use workspace::{Pane, Workspace};

use crate::{OpenPreview, OpenPreviewToTheSide};

const REPARSE_DEBOUNCE: Duration = Duration::from_millis(200);

struct EditorState {
    editor: Entity<Editor>,
    _subscription: Subscription,
}

pub struct HtmlPreviewView {
    workspace: WeakEntity<Workspace>,
    active_editor: Option<EditorState>,
    focus_handle: FocusHandle,
    markdown: Entity<Markdown>,
    scroll_handle: ScrollHandle,
    image_cache: Entity<RetainAllImageCache>,
    base_directory: Option<PathBuf>,
    pending_update_task: Option<Task<Result<()>>>,
}

impl HtmlPreviewView {
    pub fn register(workspace: &mut Workspace, _window: &mut Window, _cx: &mut Context<Workspace>) {
        workspace.register_action(move |workspace, _: &OpenPreview, window, cx| {
            if let Some(editor) = Self::resolve_active_item_as_html_editor(workspace, cx) {
                let view = Self::create_html_view(workspace, editor.clone(), window, cx);
                workspace.active_pane().update(cx, |pane, cx| {
                    if let Some(existing_view_idx) =
                        Self::find_existing_preview_item_idx(pane, &editor, cx)
                    {
                        pane.activate_item(existing_view_idx, true, true, window, cx);
                    } else {
                        pane.add_item(Box::new(view.clone()), true, true, None, window, cx)
                    }
                });
                cx.notify();
            }
        });

        workspace.register_action(move |workspace, _: &OpenPreviewToTheSide, window, cx| {
            if let Some(editor) = Self::resolve_active_item_as_html_editor(workspace, cx) {
                let view = Self::create_html_view(workspace, editor.clone(), window, cx);
                let pane = workspace
                    .find_pane_in_direction(workspace::SplitDirection::Right, cx)
                    .unwrap_or_else(|| {
                        workspace.split_pane(
                            workspace.active_pane().clone(),
                            workspace::SplitDirection::Right,
                            window,
                            cx,
                        )
                    });
                pane.update(cx, |pane, cx| {
                    if let Some(existing_view_idx) =
                        Self::find_existing_preview_item_idx(pane, &editor, cx)
                    {
                        pane.activate_item(existing_view_idx, true, true, window, cx);
                    } else {
                        pane.add_item(Box::new(view.clone()), false, false, None, window, cx)
                    }
                });
                editor.focus_handle(cx).focus(window, cx);
                cx.notify();
            }
        });
    }

    pub fn resolve_active_item_as_html_editor(
        workspace: &Workspace,
        cx: &mut Context<Workspace>,
    ) -> Option<Entity<Editor>> {
        if let Some(editor) = workspace
            .active_item(cx)
            .and_then(|item| item.act_as::<Editor>(cx))
            && Self::is_html_file(&editor, cx)
        {
            return Some(editor);
        }
        None
    }

    pub fn is_html_file<V>(editor: &Entity<Editor>, cx: &mut Context<V>) -> bool {
        editor
            .read(cx)
            .buffer()
            .read(cx)
            .as_singleton()
            .and_then(|buffer| buffer.read(cx).file())
            .is_some_and(|file| {
                Path::new(file.file_name(cx))
                    .extension()
                    .is_some_and(|extension| {
                        extension.eq_ignore_ascii_case("html")
                            || extension.eq_ignore_ascii_case("htm")
                    })
            })
    }

    fn create_html_view(
        workspace: &mut Workspace,
        editor: Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<HtmlPreviewView> {
        let language_registry = workspace.project().read(cx).languages().clone();
        let workspace_handle = workspace.weak_handle();
        HtmlPreviewView::new(editor, workspace_handle, language_registry, window, cx)
    }

    fn find_existing_preview_item_idx(
        pane: &Pane,
        editor: &Entity<Editor>,
        cx: &App,
    ) -> Option<usize> {
        let target_buffer = editor.read(cx).buffer().read(cx).as_singleton()?;
        pane.items_of_type::<HtmlPreviewView>()
            .find(|view| {
                view.read(cx)
                    .active_editor
                    .as_ref()
                    .is_some_and(|active_editor| {
                        active_editor
                            .editor
                            .read(cx)
                            .buffer()
                            .read(cx)
                            .as_singleton()
                            .as_ref()
                            == Some(&target_buffer)
                    })
            })
            .and_then(|view| pane.index_for_item(&view))
    }

    pub fn new(
        active_editor: Entity<Editor>,
        workspace: WeakEntity<Workspace>,
        language_registry: Arc<LanguageRegistry>,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let markdown = cx.new(|cx| {
                Markdown::new_with_options(
                    SharedString::default(),
                    Some(language_registry),
                    None,
                    MarkdownOptions {
                        parse_html: true,
                        render_mermaid_diagrams: true,
                        parse_heading_slugs: true,
                        render_metadata_blocks: true,
                        ..Default::default()
                    },
                    cx,
                )
            });
            let mut this = Self {
                workspace,
                active_editor: None,
                focus_handle: cx.focus_handle(),
                markdown,
                scroll_handle: ScrollHandle::new(),
                image_cache: RetainAllImageCache::new(cx),
                base_directory: None,
                pending_update_task: None,
            };
            this.set_editor(active_editor, window, cx);
            this
        })
    }

    fn set_editor(&mut self, editor: Entity<Editor>, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(active) = &self.active_editor
            && active.editor == editor
        {
            return;
        }

        let subscription = cx.subscribe_in(
            &editor,
            window,
            |this, editor, event: &EditorEvent, window, cx| match event {
                EditorEvent::Edited { .. }
                | EditorEvent::BufferEdited { .. }
                | EditorEvent::DirtyChanged
                | EditorEvent::BuffersEdited { .. } => {
                    this.update_preview_from_active_editor(true, window, cx);
                }
                EditorEvent::FileHandleChanged => {
                    this.base_directory = Self::get_folder_for_active_editor(editor.read(cx), cx);
                    this.update_preview_from_active_editor(false, window, cx);
                }
                _ => {}
            },
        );

        self.base_directory = Self::get_folder_for_active_editor(editor.read(cx), cx);
        self.active_editor = Some(EditorState {
            editor,
            _subscription: subscription,
        });
        self.update_preview_from_active_editor(false, window, cx);
    }

    fn update_preview_from_active_editor(
        &mut self,
        wait_for_debounce: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(state) = &self.active_editor {
            if wait_for_debounce && self.pending_update_task.is_some() {
                return;
            }
            self.pending_update_task = Some(self.schedule_preview_update(
                wait_for_debounce,
                state.editor.clone(),
                window,
                cx,
            ));
        }
    }

    fn schedule_preview_update(
        &mut self,
        wait_for_debounce: bool,
        editor: Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        cx.spawn_in(window, async move |view, cx| {
            if wait_for_debounce {
                cx.background_executor().timer(REPARSE_DEBOUNCE).await;
            }

            let contents = view.update(cx, |view, cx| {
                let is_active_editor = view
                    .active_editor
                    .as_ref()
                    .is_some_and(|active_editor| active_editor.editor == editor);
                if !is_active_editor {
                    return None;
                }

                editor.update(cx, |editor, cx| {
                    let contents = editor
                        .buffer()
                        .read(cx)
                        .as_singleton()?
                        .read(cx)
                        .as_rope()
                        .to_string();
                    Some(contents)
                })
            })?;

            view.update(cx, move |view, cx| {
                if let Some(contents) = contents {
                    let sanitized = sanitize_html(&contents);
                    view.markdown.update(cx, |markdown, cx| {
                        markdown.reset(sanitized.into(), cx);
                    });
                }
                view.pending_update_task = None;
                cx.notify();
            })
        })
    }

    fn get_folder_for_active_editor(editor: &Editor, cx: &App) -> Option<PathBuf> {
        if let Some(file) = editor.file_at(MultiBufferOffset(0), cx) {
            if let Some(file) = file.as_local() {
                file.abs_path(cx).parent().map(|p| p.to_path_buf())
            } else {
                None
            }
        } else {
            None
        }
    }

    fn render_markdown_element(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> MarkdownElement {
        let mut workspace_directory = None;
        if let Some(workspace_entity) = self.workspace.upgrade() {
            let project = workspace_entity.read(cx).project();
            if let Some(tree) = project.read(cx).worktrees(cx).next() {
                workspace_directory = Some(tree.read(cx).abs_path().to_path_buf());
            }
        }

        let markdown_style = markdown::github_style(window, cx);

        MarkdownElement::new(self.markdown.clone(), markdown_style)
            .code_block_renderer(CodeBlockRenderer::Default {
                copy_button_visibility: CopyButtonVisibility::VisibleOnHover,
                wrap_button_visibility: markdown::WrapButtonVisibility::Hidden,
                border: false,
            })
            .scroll_handle(self.scroll_handle.clone())
            .image_resolver({
                let base_directory = self.base_directory.clone();
                move |dest_url| {
                    resolve_preview_image(
                        dest_url,
                        base_directory.as_deref(),
                        workspace_directory.as_deref(),
                    )
                }
            })
            .on_url_click(|url, _window, cx| {
                if url.starts_with("http://") || url.starts_with("https://") {
                    cx.open_url(&url);
                }
            })
    }
}

fn sanitize_html(raw: &str) -> String {
    let parse_options = ParseOpts {
        tree_builder: TreeBuilderOpts {
            drop_doctype: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bytes = raw.as_bytes();
    let Ok(dom) = parse_document(RcDom::default(), parse_options)
        .from_utf8()
        .read_from(&mut bytes)
    else {
        return raw.to_string();
    };

    let Some(body) = find_body(&dom.document) else {
        return raw.to_string();
    };
    strip_script_and_style(&body);

    let mut buffer = Vec::new();
    let serializable: SerializableHandle = body.into();
    // Default `SerializeOpts` uses `ChildrenOnly`, so this yields the body inner HTML.
    if html5ever::serialize(&mut buffer, &serializable, SerializeOpts::default()).is_err() {
        return raw.to_string();
    }
    String::from_utf8(buffer).unwrap_or_else(|_| raw.to_string())
}

// Iterative DFS: HTML nesting depth is user-controlled, so a recursive walk
// could overflow the stack on a deeply nested document.
fn find_body(document: &Handle) -> Option<Handle> {
    let mut stack = vec![document.clone()];
    while let Some(node) = stack.pop() {
        if let NodeData::Element { name, .. } = &node.data
            && name.local.to_string().eq_ignore_ascii_case("body")
        {
            return Some(node);
        }
        stack.extend(node.children.borrow().iter().cloned());
    }
    None
}

// Iterative for the same stack-safety reason as `find_body`.
fn strip_script_and_style(root: &Handle) {
    let mut stack = vec![root.clone()];
    while let Some(node) = stack.pop() {
        node.children
            .borrow_mut()
            .retain(|child| !is_script_or_style(child));
        stack.extend(node.children.borrow().iter().cloned());
    }
}

fn is_script_or_style(node: &Handle) -> bool {
    if let NodeData::Element { name, .. } = &node.data {
        let tag = name.local.to_string();
        tag.eq_ignore_ascii_case("script") || tag.eq_ignore_ascii_case("style")
    } else {
        false
    }
}

fn resolve_preview_image(
    dest_url: &str,
    base_directory: Option<&Path>,
    workspace_directory: Option<&Path>,
) -> Option<ImageSource> {
    if dest_url.starts_with("data:") {
        return None;
    }

    if dest_url.starts_with("http://") || dest_url.starts_with("https://") {
        return Some(ImageSource::Resource(Resource::Uri(SharedUri::from(
            dest_url.to_string(),
        ))));
    }

    let decoded = urlencoding::decode(dest_url)
        .map(|decoded| decoded.into_owned())
        .unwrap_or_else(|_| dest_url.to_string());

    if let Some(stripped) = ['/', '\\']
        .iter()
        .find_map(|prefix| decoded.strip_prefix(*prefix))
    {
        if let Some(root) = workspace_directory {
            let absolute_path = root.join(stripped);
            if absolute_path.exists() {
                return Some(ImageSource::Resource(Resource::Path(Arc::from(
                    absolute_path.as_path(),
                ))));
            } else {
                return None;
            }
        }
    }

    let path = if Path::new(&decoded).is_absolute() {
        PathBuf::from(decoded)
    } else {
        base_directory?.join(decoded)
    };

    path.exists()
        .then(|| ImageSource::Resource(Resource::Path(Arc::from(path.as_path()))))
}

impl Focusable for HtmlPreviewView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<()> for HtmlPreviewView {}

impl Render for HtmlPreviewView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let background = markdown::github_page_background(cx.theme().appearance());
        let preview_font_size = ThemeSettings::get_global(cx).markdown_preview_font_size(cx);
        div()
            .image_cache(self.image_cache.clone())
            .id("HtmlPreview")
            .key_context("HtmlPreview")
            .track_focus(&self.focus_handle(cx))
            .size_full()
            .min_h_0()
            .bg(background)
            .child(
                WithRemSize::new(preview_font_size).size_full().child(
                    div()
                        .id("html-preview-scroll-container")
                        .size_full()
                        .overflow_y_scroll()
                        .track_scroll(&self.scroll_handle)
                        .p_4()
                        .child(self.render_markdown_element(window, cx)),
                ),
            )
            .vertical_scrollbar_for(&self.scroll_handle, window, cx)
    }
}

impl Item for HtmlPreviewView {
    type Event = ();

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::FileDoc))
    }

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        self.active_editor
            .as_ref()
            .map(|editor_state| {
                let buffer = editor_state.editor.read(cx).buffer().read(cx);
                format!("Preview {}", buffer.title(cx)).into()
            })
            .unwrap_or_else(|| SharedString::from("HTML Preview"))
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("HTML Preview Opened")
    }

    fn to_item_events(_event: &Self::Event, _f: &mut dyn FnMut(workspace::item::ItemEvent)) {}
}
