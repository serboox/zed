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
/// How often the engine is given a turn. Thirty frames a second is plenty for
/// reading a page, and every frame costs a full copy of it.
#[cfg(feature = "servo")]
const FRAME_INTERVAL: Duration = Duration::from_millis(33);

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
    /// The live page, when this build has an engine: a real document that lays
    /// itself out, runs its scripts and answers the mouse. Without one the
    /// Markdown rendering below is what the reader sees.
    #[cfg(feature = "servo")]
    page: Option<html_render::HtmlPage>,
    #[cfg(feature = "servo")]
    frame: Option<std::sync::Arc<gpui::RenderImage>>,
    #[cfg(feature = "servo")]
    pump: Option<Task<()>>,
    /// Where the page is painted, so a click can be told where it landed and a
    /// resize can be passed on to the engine.
    #[cfg(feature = "servo")]
    page_bounds: std::rc::Rc<std::cell::Cell<gpui::Bounds<gpui::Pixels>>>,
    /// Whether the button now held down was pressed on the page. A drag that
    /// began elsewhere belongs to whoever started it.
    #[cfg(feature = "servo")]
    page_pressed: std::rc::Rc<std::cell::Cell<bool>>,
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
                #[cfg(feature = "servo")]
                page: None,
                #[cfg(feature = "servo")]
                frame: None,
                #[cfg(feature = "servo")]
                pump: None,
                #[cfg(feature = "servo")]
                page_bounds: std::rc::Rc::new(std::cell::Cell::new(gpui::Bounds::default())),
                #[cfg(feature = "servo")]
                page_pressed: std::rc::Rc::new(std::cell::Cell::new(false)),
            };
            #[cfg(feature = "servo")]
            cx.on_release_in(window, |this: &mut Self, window, _| {
                // The last frame a closed preview painted would otherwise sit
                // in the sprite atlas for as long as the window lives.
                if let Some(frame) = this.frame.take()
                    && let Err(error) = window.drop_image(frame)
                {
                    log::warn!("the page's last frame could not be released: {error:#}");
                }
            })
            .detach();
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

            view.update_in(cx, move |view, window, cx| {
                if let Some(contents) = contents {
                    let sanitized = sanitize_html(&contents);
                    view.markdown.update(cx, |markdown, cx| {
                        markdown.reset(sanitized.into(), cx);
                    });
                    #[cfg(feature = "servo")]
                    view.show_in_engine(contents, window, cx);
                    #[cfg(not(feature = "servo"))]
                    let _ = window;
                }
                view.pending_update_task = None;
                cx.notify();
            })
        })
    }

    /// Opens the document in the engine, or reloads the page already open, and
    /// keeps the frames coming.
    #[cfg(feature = "servo")]
    fn show_in_engine(&mut self, contents: String, window: &mut Window, cx: &mut Context<Self>) {
        let base_directory = self.base_directory.clone();
        let contents: gpui::SharedString = contents.into();
        if let Some(page) = self.page.as_mut() {
            if let Err(error) = page.reload(contents, base_directory.as_deref()) {
                log::warn!("the page could not be reloaded: {error:#}");
            }
            return;
        }

        let bounds = self.page_bounds.get();
        let size = match bounds.size.width > gpui::px(64.) {
            true => bounds.size,
            // Before the first paint there is nothing to measure; the page is
            // resized as soon as there is.
            false => gpui::size(gpui::px(900.), gpui::px(700.)),
        };
        match html_render::HtmlPage::open(
            contents,
            base_directory.as_deref(),
            size,
            window.scale_factor(),
            cx,
        ) {
            Ok(page) => {
                self.page = Some(page);
                self.start_pumping(cx);
            }
            Err(error) => log::warn!("the HTML engine did not open the page: {error:#}"),
        }
    }

    /// Drives the engine while the page is open. Servo lays out, runs script and
    /// paints only while its event loop is being turned, so this is what makes
    /// the page live rather than a photograph.
    #[cfg(feature = "servo")]
    fn start_pumping(&mut self, cx: &mut Context<Self>) {
        self.pump = Some(cx.spawn(async move |view, cx| {
            loop {
                cx.background_executor().timer(FRAME_INTERVAL).await;
                let carried_on = view.update_in(cx, |view, window, cx| {
                    let Some(page) = view.page.as_mut() else {
                        return false;
                    };
                    let bounds = view.page_bounds.get();
                    if bounds.size.width > gpui::px(64.) {
                        page.resize(bounds.size, window.scale_factor());
                    }
                    if page.pump() {
                        let superseded = view.frame.take();
                        view.frame = page.frame();
                        // Dropping the handle is not enough: the texture stays in
                        // the sprite atlas until the window is told to let it go,
                        // and a page hands over a new one many times a second.
                        // That rate is also why a failure here is only noted:
                        // thirty warnings a second would drown the log.
                        if let Some(superseded) = superseded
                            && let Err(error) = window.drop_image(superseded)
                        {
                            log::debug!("a superseded frame could not be released: {error:#}");
                        }
                        cx.notify();
                    }
                    if let Some(url) = page.take_link_for_new_tab() {
                        log::debug!("the page navigated to {url}");
                    }
                    true
                });
                match carried_on {
                    Ok(true) => {}
                    _ => break,
                }
            }
        }));
    }

    /// Whether the engine's page is what the reader is looking at.
    #[cfg(feature = "servo")]
    fn showing_live_page(&self) -> bool {
        self.frame.is_some()
    }

    #[cfg(not(feature = "servo"))]
    fn showing_live_page(&self) -> bool {
        false
    }

    /// Typing reaches the page from the element that holds focus. A handler on
    /// the picture itself sits below the focus path and never hears a key, so
    /// the keys are taken where focus actually lands and passed down.
    #[cfg(feature = "servo")]
    fn with_page_keys(
        root: gpui::Stateful<gpui::Div>,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        root.on_key_down(cx.listener(|view, event: &gpui::KeyDownEvent, _, _| {
            if let Some(page) = view.page.as_ref() {
                page.key(servo_key(
                    &event.keystroke,
                    keyboard_types::KeyState::Down,
                    event.is_held,
                ));
            }
        }))
        .on_key_up(cx.listener(|view, event: &gpui::KeyUpEvent, _, _| {
            if let Some(page) = view.page.as_ref() {
                page.key(servo_key(
                    &event.keystroke,
                    keyboard_types::KeyState::Up,
                    false,
                ));
            }
        }))
    }

    #[cfg(not(feature = "servo"))]
    fn with_page_keys(
        root: gpui::Stateful<gpui::Div>,
        _: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        root
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

    /// The live page when the engine has one, and the Markdown rendering of the
    /// document otherwise.
    ///
    /// The page's own input is delivered through window-level listeners rather
    /// than the element's: an element only hears about the mouse while it is
    /// hovered, and a drag -- which is how text is selected -- is exactly the
    /// case where that stops being true.
    #[cfg(feature = "servo")]
    fn render_page(&self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(frame) = self.frame.clone() else {
            return self.render_markdown_element(window, cx).into_any_element();
        };
        let bounds_cell = self.page_bounds.clone();
        let holding = self.page_pressed.clone();
        let view = cx.entity().downgrade();
        div()
            .size_full()
            .relative()
            // The picture's own bounds are what a click is measured against: an
            // absolutely positioned overlay has no size of its own to offer, and
            // a hit test against nothing lets nothing through.
            .on_children_prepainted({
                let bounds_cell = bounds_cell.clone();
                move |children, _, _| {
                    if let Some(page) = children.first() {
                        bounds_cell.set(*page);
                    }
                }
            })
            .child(gpui::img(gpui::ImageSource::Render(frame)).size_full())
            .child(
                gpui::canvas(
                    |_, _, _| (),
                    move |_, _, window, _| {
                        let bounds = bounds_cell.get();
                        let page_point = move |position: gpui::Point<gpui::Pixels>| {
                            gpui::point(position.x - bounds.origin.x, position.y - bounds.origin.y)
                        };
                        let inside = move |position: gpui::Point<gpui::Pixels>| {
                            // Before the first paint there is nothing to measure
                            // against, and a page that hears nothing is worse
                            // than one that hears too much.
                            bounds.size.width <= gpui::px(1.) || bounds.contains(&position)
                        };

                        window.on_mouse_event({
                            let view = view.clone();
                            let holding = holding.clone();
                            move |event: &gpui::MouseMoveEvent, phase, _, cx| {
                                if phase != gpui::DispatchPhase::Bubble {
                                    return;
                                }
                                // A drag that started on the page keeps steering it
                                // even when the pointer wanders off the edge, which
                                // is what selecting to the end of a line does. A
                                // drag that started elsewhere is somebody else's.
                                if !inside(event.position) && !holding.get() {
                                    return;
                                }
                                view.update(cx, |view, _| {
                                    if let Some(page) = view.page.as_ref() {
                                        page.mouse_moved(page_point(event.position));
                                    }
                                })
                                .ok();
                            }
                        });
                        window.on_mouse_event({
                            let view = view.clone();
                            let holding = holding.clone();
                            move |event: &gpui::MouseDownEvent, phase, _, cx| {
                                if phase != gpui::DispatchPhase::Bubble || !inside(event.position) {
                                    return;
                                }
                                holding.set(true);
                                view.update(cx, |view, _| {
                                    if let Some(page) = view.page.as_ref() {
                                        page.mouse_down(
                                            page_point(event.position),
                                            servo_button(event.button),
                                        );
                                    }
                                })
                                .ok();
                            }
                        });
                        window.on_mouse_event({
                            let view = view.clone();
                            let holding = holding.clone();
                            move |event: &gpui::MouseUpEvent, phase, _, cx| {
                                if phase != gpui::DispatchPhase::Bubble {
                                    return;
                                }
                                // Only the button the page itself is holding is
                                // released to it; a click that began in the editor
                                // must not finish inside the page.
                                if !holding.replace(false) && !inside(event.position) {
                                    return;
                                }
                                view.update(cx, |view, _| {
                                    if let Some(page) = view.page.as_ref() {
                                        page.mouse_up(
                                            page_point(event.position),
                                            servo_button(event.button),
                                        );
                                    }
                                })
                                .ok();
                            }
                        });
                        window.on_mouse_event({
                            move |event: &gpui::ScrollWheelEvent, phase, window, cx| {
                                if phase != gpui::DispatchPhase::Bubble || !inside(event.position) {
                                    return;
                                }
                                let delta = event.delta.pixel_delta(window.line_height());
                                view.update(cx, |view, _| {
                                    if let Some(page) = view.page.as_ref() {
                                        page.scrolled(page_point(event.position), delta);
                                    }
                                })
                                .ok();
                            }
                        });
                    },
                )
                .absolute()
                .size_full(),
            )
            .into_any_element()
    }

    #[cfg(not(feature = "servo"))]
    fn render_page(&self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        self.render_markdown_element(window, cx).into_any_element()
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

/// gpui and the engine name the keys separately. Anything that produces a
/// character is passed through as that character; the rest are named keys a
/// page can act on.
#[cfg(feature = "servo")]
fn servo_key(
    keystroke: &gpui::Keystroke,
    state: keyboard_types::KeyState,
    repeat: bool,
) -> keyboard_types::KeyboardEvent {
    use keyboard_types::{Code, Key, Location, Modifiers, NamedKey};

    let named = |name| Key::Named(name);
    let key = match keystroke.key.as_str() {
        "enter" => named(NamedKey::Enter),
        "escape" => named(NamedKey::Escape),
        "backspace" => named(NamedKey::Backspace),
        "delete" => named(NamedKey::Delete),
        "insert" => named(NamedKey::Insert),
        "tab" => named(NamedKey::Tab),
        "space" => Key::Character(" ".into()),
        "up" => named(NamedKey::ArrowUp),
        "down" => named(NamedKey::ArrowDown),
        "left" => named(NamedKey::ArrowLeft),
        "right" => named(NamedKey::ArrowRight),
        "home" => named(NamedKey::Home),
        "end" => named(NamedKey::End),
        "pageup" => named(NamedKey::PageUp),
        "pagedown" => named(NamedKey::PageDown),
        "shift" => named(NamedKey::Shift),
        "control" => named(NamedKey::Control),
        "alt" => named(NamedKey::Alt),
        "cmd" | "super" | "win" | "platform" => named(NamedKey::Meta),
        "capslock" => named(NamedKey::CapsLock),
        other => {
            // A page reads a function key by name, so `f7` has to arrive as F7
            // rather than as the two characters it is spelled with.
            let function_key = other
                .strip_prefix('f')
                .and_then(|number| number.parse::<u8>().ok())
                .filter(|number| (1..=12).contains(number));
            match function_key {
                Some(1) => named(NamedKey::F1),
                Some(2) => named(NamedKey::F2),
                Some(3) => named(NamedKey::F3),
                Some(4) => named(NamedKey::F4),
                Some(5) => named(NamedKey::F5),
                Some(6) => named(NamedKey::F6),
                Some(7) => named(NamedKey::F7),
                Some(8) => named(NamedKey::F8),
                Some(9) => named(NamedKey::F9),
                Some(10) => named(NamedKey::F10),
                Some(11) => named(NamedKey::F11),
                Some(12) => named(NamedKey::F12),
                _ => match keystroke.key_char.clone() {
                    Some(character) => Key::Character(character),
                    None if other.chars().count() == 1 => Key::Character(other.to_string()),
                    None => named(NamedKey::Unidentified),
                },
            }
        }
    };

    let mut modifiers = Modifiers::empty();
    if keystroke.modifiers.shift {
        modifiers |= Modifiers::SHIFT;
    }
    if keystroke.modifiers.control {
        modifiers |= Modifiers::CONTROL;
    }
    if keystroke.modifiers.alt {
        modifiers |= Modifiers::ALT;
    }
    if keystroke.modifiers.platform {
        modifiers |= Modifiers::META;
    }

    keyboard_types::KeyboardEvent {
        state,
        key,
        code: Code::Unidentified,
        location: Location::Standard,
        modifiers,
        repeat,
        is_composing: false,
    }
}

/// gpui and the engine name the mouse buttons separately.
#[cfg(feature = "servo")]
fn servo_button(button: gpui::MouseButton) -> html_render::MouseButton {
    match button {
        gpui::MouseButton::Right => html_render::MouseButton::Right,
        gpui::MouseButton::Middle => html_render::MouseButton::Middle,
        _ => html_render::MouseButton::Left,
    }
}

impl Render for HtmlPreviewView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let background = markdown::github_page_background(cx.theme().appearance());
        let preview_font_size = ThemeSettings::get_global(cx).markdown_preview_font_size(cx);
        let root = div()
            // The retain-all cache belongs to the Markdown rendering's images.
            // A live page replaces its frame many times a second, and keeping
            // every one of those is how a preview eats a machine's memory.
            .when(!self.showing_live_page(), |this| {
                this.image_cache(self.image_cache.clone())
            })
            .id("HtmlPreview")
            .key_context("HtmlPreview")
            .track_focus(&self.focus_handle(cx))
            .size_full()
            .min_h_0()
            .bg(background);
        Self::with_page_keys(root, cx)
            .child(
                WithRemSize::new(preview_font_size).size_full().child(
                    div()
                        .id("html-preview-scroll-container")
                        .size_full()
                        .overflow_y_scroll()
                        .track_scroll(&self.scroll_handle)
                        .p_4()
                        .child(self.render_page(window, cx)),
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
