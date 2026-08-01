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
use ui::{Tooltip, WithScrollbar, prelude::*};
use workspace::item::Item;
use workspace::{Pane, Workspace};

use crate::{OpenPreview, OpenPreviewToTheSide};

const REPARSE_DEBOUNCE: Duration = Duration::from_millis(200);
/// How long the page's driver waits before looking anyway. The engine says when
/// it has work, so this is only a safety net -- for a wake-up that was missed,
/// and for a view that was resized without the page hearing about it.
#[cfg(feature = "servo")]
const HEARTBEAT: Duration = Duration::from_millis(250);
/// The same, for a preview nobody is looking at.
#[cfg(feature = "servo")]
const UNSEEN_HEARTBEAT: Duration = Duration::from_millis(1000);
/// How often the page is asked where it stands. Asking runs a script in the
/// page, so it is worth doing often enough for the scrollbar to keep up with a
/// reader's thumb and no more.
#[cfg(feature = "servo")]
const WHERE_THE_PAGE_STANDS: Duration = Duration::from_millis(100);

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
    /// The page's own buffer, when the window can draw it where it lies. While
    /// this is set, no frame is ever copied through memory.
    #[cfg(all(feature = "servo", target_os = "linux"))]
    shared_frame: Option<std::sync::Arc<gpui::SharedFrame>>,
    #[cfg(feature = "servo")]
    pump: Option<Task<()>>,
    /// Where the page is painted, so a click can be told where it landed and a
    /// resize can be passed on to the engine.
    #[cfg(feature = "servo")]
    page_bounds: std::rc::Rc<std::cell::Cell<gpui::Bounds<gpui::Pixels>>>,
    /// Which button, if any, was pressed on the page and is still down. A drag
    /// that began elsewhere belongs to whoever started it, and a button let go
    /// is only the one that was held.
    #[cfg(feature = "servo")]
    page_pressed: std::rc::Rc<std::cell::Cell<Option<gpui::MouseButton>>>,
    /// Whether this preview is the one its pane is showing. A page nobody is
    /// looking at is held back, and this is the pane saying so -- silence from
    /// the renderer would not do, because a page that has settled stops being
    /// redrawn while still very much on screen.
    #[cfg(feature = "servo")]
    on_screen: bool,
    /// Text the page has been asked to hand over for the clipboard. The engine
    /// answers on a later turn of its own loop, so the answer is left here.
    #[cfg(feature = "servo")]
    pending_copy: std::rc::Rc<std::cell::RefCell<Option<String>>>,
    /// Where the page stands, for the scrollbar to show and for a drag of its
    /// thumb to change. The engine scrolls the page itself, so this is the only
    /// thing the editor knows about it.
    #[cfg(feature = "servo")]
    page_scroll: crate::page_scroll::PageScrollHandle,
    /// When the page was last asked where it stands. Asking runs a script, so it
    /// is asked at a pace of its own rather than on every frame.
    #[cfg(feature = "servo")]
    asked_where: Option<std::time::Instant>,
    /// Where the page is, and where the reader may type to send it elsewhere.
    #[cfg(feature = "servo")]
    address: Entity<Editor>,
    /// The address last put into the bar, so a page that has gone somewhere of
    /// its own accord is noticed without asking the engine every frame.
    #[cfg(feature = "servo")]
    showing_address: Option<String>,
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
                #[cfg(all(feature = "servo", target_os = "linux"))]
                shared_frame: None,
                #[cfg(feature = "servo")]
                pump: None,
                #[cfg(feature = "servo")]
                page_bounds: std::rc::Rc::new(std::cell::Cell::new(gpui::Bounds::default())),
                #[cfg(feature = "servo")]
                page_pressed: std::rc::Rc::new(std::cell::Cell::new(None)),
                #[cfg(feature = "servo")]
                on_screen: true,
                #[cfg(feature = "servo")]
                pending_copy: std::rc::Rc::new(std::cell::RefCell::new(None)),
                #[cfg(feature = "servo")]
                page_scroll: crate::page_scroll::PageScrollHandle::default(),
                #[cfg(feature = "servo")]
                asked_where: None,
                #[cfg(feature = "servo")]
                address: address_bar(window, cx),
                #[cfg(feature = "servo")]
                showing_address: None,
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
            page_scale(window, cx),
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
    ///
    /// It is not turned on a timer. The engine says when it has work -- a load,
    /// a script, an animation frame, an event we handed it -- and a page at rest
    /// says nothing at all, which is what makes a still page free. A preview
    /// nobody is looking at is told to hold back its animations and timers.
    #[cfg(feature = "servo")]
    fn start_pumping(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.page.as_ref().map(|page| page.engine()) else {
            return;
        };
        self.pump = Some(cx.spawn(async move |view, cx| {
            let mut unseen = false;
            loop {
                // Whichever comes first: the engine saying it has work, or the
                // safety net going off. Nothing here asks the engine again and
                // again -- a page at rest wakes nobody and costs nothing.
                let heartbeat = if unseen { UNSEEN_HEARTBEAT } else { HEARTBEAT };
                smol::future::or(
                    engine.wait_for_work(),
                    cx.background_executor().timer(heartbeat),
                )
                .await;
                let turn = view.update_in(cx, |view, window, cx| {
                    let out_of_sight = !view.on_screen;
                    let Some(page) = view.page.as_mut() else {
                        return None;
                    };
                    page.set_throttled(out_of_sight);
                    page.set_dark(reading_in_the_dark(cx));
                    if out_of_sight {
                        return Some(true);
                    }
                    let bounds = view.page_bounds.get();
                    if bounds.size.width > gpui::px(64.) {
                        page.resize(bounds.size, page_scale(window, cx));
                    }
                    let painted = page.pump();
                    if let Some(url) = page.take_link_for_new_tab() {
                        log::debug!("the page navigated to {url}");
                    }
                    // Where the page has got to, in the bar. Only when the
                    // reader is not typing there: an address half typed is not
                    // to be taken away.
                    let now_at = page.address().map(|address| address.to_string());
                    if now_at != view.showing_address
                        && !view.address.focus_handle(cx).contains_focused(window, cx)
                    {
                        view.showing_address = now_at.clone();
                        let showing = now_at.unwrap_or_default();
                        view.address.update(cx, |address, cx| {
                            address.set_text(showing, window, cx);
                        });
                    }
                    // A drag of the scrollbar's thumb is a request to the page,
                    // which only the engine can carry out.
                    if let Some(down) = view.page_scroll.take_request() {
                        page.scroll_to(down);
                    }
                    // Where the page stands is a script's answer, which arrives
                    // on a later turn, so it is asked for at a pace of its own
                    // rather than on every frame. Nothing has to be redrawn when
                    // the answer lands: a page that is moving is painting, and a
                    // page that is still has not moved.
                    let due = view
                        .asked_where
                        .is_none_or(|asked| asked.elapsed() >= WHERE_THE_PAGE_STANDS);
                    if due {
                        view.asked_where = Some(std::time::Instant::now());
                        let scroll = view.page_scroll.clone();
                        page.scroll_position(move |down, document, view| {
                            scroll.stands_at(crate::page_scroll::PageScroll {
                                down,
                                document,
                                view,
                            });
                        });
                    }
                    // The page answers a copy request on a turn of its own, so
                    // the answer is collected here rather than where it was
                    // asked for. It belongs to every path through this loop.
                    if let Some(text) = view.pending_copy.borrow_mut().take()
                        && !text.is_empty()
                    {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                    }
                    #[cfg(target_os = "linux")]
                    if view
                        .shared_frame
                        .as_ref()
                        .is_some_and(|shared| shared.is_refused())
                    {
                        // The window could not draw the page's own buffer after
                        // all, and the page has gone back to copying frames.
                        view.shared_frame = None;
                    }
                    #[cfg(target_os = "linux")]
                    if painted && let Some(shared) = page.shared_frame() {
                        view.shared_frame = Some(shared);
                        // What was copied before is not what is drawn now.
                        if let Some(superseded) = view.frame.take()
                            && let Err(error) = window.drop_image(superseded)
                        {
                            log::debug!("a superseded frame could not be released: {error:#}");
                        }
                        cx.notify();
                        return Some(false);
                    }
                    if painted {
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
                    Some(false)
                });
                match turn {
                    Ok(Some(out_of_sight)) => unseen = out_of_sight,
                    // The page is gone, or the view is: either way there is
                    // nothing left to drive.
                    Ok(None) | Err(_) => break,
                }
            }
        }));
    }

    /// Whether the engine's page is what the reader is looking at.
    #[cfg(all(feature = "servo", target_os = "linux"))]
    fn showing_live_page(&self) -> bool {
        self.frame.is_some() || self.shared_frame.is_some()
    }

    #[cfg(all(feature = "servo", not(target_os = "linux")))]
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
        root.on_key_down(cx.listener(|view, event: &gpui::KeyDownEvent, window, cx| {
            let Some(page) = view.page.as_ref() else {
                return;
            };
            // What is being typed into the address bar is not for the page.
            if view.address.focus_handle(cx).contains_focused(window, cx) {
                return;
            }
            // Copying is the editor's job, not the page's: the page is asked
            // what is selected and the answer goes to the clipboard.
            if asks_to_copy(&event.keystroke) {
                let waiting = view.pending_copy.clone();
                page.selected_text(move |text| {
                    *waiting.borrow_mut() = Some(text);
                });
                cx.stop_propagation();
                return;
            }
            page.key(servo_key(
                &event.keystroke,
                keyboard_types::KeyState::Down,
                event.is_held,
            ));
        }))
        .on_key_up(cx.listener(|view, event: &gpui::KeyUpEvent, window, cx| {
            if view.address.focus_handle(cx).contains_focused(window, cx) {
                return;
            }
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
        #[cfg(target_os = "linux")]
        let shared = self.shared_frame.clone();
        #[cfg(not(target_os = "linux"))]
        let shared: Option<()> = None;
        let frame = self.frame.clone();
        if frame.is_none() && shared.is_none() {
            if self.page.is_some() {
                // The engine has the document and is drawing it. Showing the
                // Markdown rendering of the source in the meantime, and
                // replacing it with the page a moment later, looks like two
                // previews arriving one after the other; an empty pane is what
                // a browser shows while a page is on its way.
                return div()
                    .size_full()
                    .bg(cx.theme().colors().editor_background)
                    .into_any_element();
            }
            return self.render_markdown_element(window, cx).into_any_element();
        }
        let bounds_cell = self.page_bounds.clone();
        let holding = self.page_pressed.clone();
        let view = cx.entity().downgrade();
        let scroll = self.page_scroll.clone();
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
            .when_some(frame, |this, frame| {
                this.child(gpui::img(gpui::ImageSource::Render(frame)).size_full())
            })
            .child(
                gpui::canvas(
                    |_, _, _| (),
                    move |painted_bounds, _, window, _| {
                        // The page's own buffer, drawn where the graphics card
                        // already holds it.
                        #[cfg(target_os = "linux")]
                        if let Some(shared) = shared.clone() {
                            window.paint_shared_frame(painted_bounds, shared);
                        }
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
                                if !inside(event.position) && holding.get().is_none() {
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
                            move |event: &gpui::MouseDownEvent, phase, window, cx| {
                                if phase != gpui::DispatchPhase::Bubble || !inside(event.position) {
                                    return;
                                }
                                let Some(button) = servo_button(event.button) else {
                                    return;
                                };
                                holding.set(Some(event.button));
                                view.update(cx, |view, cx| {
                                    if let Some(page) = view.page.as_ref() {
                                        page.mouse_down(page_point(event.position), button);
                                    }
                                    // Whoever is clicked on is who types next. A
                                    // page that takes the mouse but leaves the
                                    // keyboard with the source is a page the
                                    // reader cannot type into.
                                    view.focus_handle.focus(window, cx);
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
                                let Some(button) = servo_button(event.button) else {
                                    return;
                                };
                                // Only the button the page itself is holding is
                                // released to it, and only that button: a drag
                                // held with one button and let go with another
                                // would otherwise leave the first one down for
                                // ever. A click that began in the editor must not
                                // finish inside the page either.
                                let held = holding.get() == Some(event.button);
                                if held {
                                    holding.set(None);
                                } else if !inside(event.position) {
                                    return;
                                }
                                view.update(cx, |view, _| {
                                    if let Some(page) = view.page.as_ref() {
                                        page.mouse_up(page_point(event.position), button);
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
            // The page scrolls inside the engine, so there is no scroll
            // container here to hang a scrollbar on. This one is a decoration
            // over the picture: it shows where the page says it stands, and a
            // drag of its thumb asks the page to go elsewhere.
            .custom_scrollbars(
                ui::Scrollbars::for_settings::<PageScrollbarSetting>()
                    .show_along(ui::ScrollAxes::Vertical)
                    .id("html-preview-page")
                    .tracked_scroll_handle(&scroll),
                window,
                cx,
            )
            .into_any_element()
    }

    /// The line above the page: where it is, where it has been, and where the
    /// reader would like to go. What is typed here is an address if it looks
    /// like one and a search if it does not.
    #[cfg(feature = "servo")]
    fn render_address_bar(&self, _window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let (behind, ahead) = self
            .page
            .as_ref()
            .map(|page| page.can_go())
            .unwrap_or((false, false));
        let colors = cx.theme().colors();
        h_flex()
            .id("html-preview-address")
            .key_context("HtmlPreviewAddress")
            .on_action(cx.listener(|view, _: &menu::Confirm, window, cx| {
                view.go_where_the_bar_says(window, cx);
            }))
            .w_full()
            .flex_none()
            .gap_1()
            .px_1p5()
            .py_1()
            .border_b_1()
            .border_color(colors.border)
            .bg(colors.toolbar_background)
            .child(
                IconButton::new("html-preview-back", IconName::ArrowLeft)
                    .icon_size(IconSize::Small)
                    .disabled(!behind)
                    .tooltip(Tooltip::text("Back"))
                    .on_click(cx.listener(|view, _, _, cx| {
                        if let Some(page) = view.page.as_ref() {
                            page.go_back();
                        }
                        cx.notify();
                    })),
            )
            .child(
                IconButton::new("html-preview-forward", IconName::ArrowRight)
                    .icon_size(IconSize::Small)
                    .disabled(!ahead)
                    .tooltip(Tooltip::text("Forward"))
                    .on_click(cx.listener(|view, _, _, cx| {
                        if let Some(page) = view.page.as_ref() {
                            page.go_forward();
                        }
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .px_1p5()
                    .py_0p5()
                    .rounded_sm()
                    .bg(colors.editor_background)
                    .border_1()
                    .border_color(colors.border_variant)
                    .child(self.address.clone()),
            )
            .into_any_element()
    }

    /// Takes the page wherever the address bar now says, and puts what it
    /// arrives at back into the bar.
    #[cfg(feature = "servo")]
    fn go_where_the_bar_says(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let typed = self.address.read(cx).text(cx);
        let Some(going) =
            crate::html_preview_settings::HtmlPreviewSettings::get_global(cx).where_to_go(&typed)
        else {
            return;
        };
        let showing = going.to_string();
        if let Some(page) = self.page.as_mut() {
            page.go_to(going);
        }
        self.address.update(cx, |address, cx| {
            address.set_text(showing, window, cx);
        });
        self.focus_handle.focus(window, cx);
        cx.notify();
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

/// Whether a keystroke is the copy command. The page never sees it: what is
/// selected there belongs on the editor's clipboard.
#[cfg(feature = "servo")]
fn asks_to_copy(keystroke: &gpui::Keystroke) -> bool {
    let modifiers = &keystroke.modifiers;
    keystroke.key == "c" && (modifiers.control || modifiers.platform) && !modifiers.alt
}

/// gpui and the engine name the mouse buttons separately. A button the engine
/// has no name for is not passed on at all: a back or forward button announced
/// to a page as a left click starts selecting text from wherever the pointer
/// happens to be.
#[cfg(feature = "servo")]
fn servo_button(button: gpui::MouseButton) -> Option<html_render::MouseButton> {
    match button {
        gpui::MouseButton::Left => Some(html_render::MouseButton::Left),
        gpui::MouseButton::Right => Some(html_render::MouseButton::Right),
        gpui::MouseButton::Middle => Some(html_render::MouseButton::Middle),
        gpui::MouseButton::Navigate(_) => None,
    }
}

impl Render for HtmlPreviewView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let background = markdown::github_page_background(cx.theme().appearance());
        let preview_font_size = ThemeSettings::get_global(cx).markdown_preview_font_size(cx);
        #[cfg(feature = "servo")]
        {
            self.on_screen = true;
        }
        let live_page = self.showing_live_page();
        let root = div()
            // The retain-all cache belongs to the Markdown rendering's images.
            // A live page replaces its frame many times a second, and keeping
            // every one of those is how a preview eats a machine's memory.
            .when(!live_page, |this| {
                this.image_cache(self.image_cache.clone())
            })
            .id("HtmlPreview")
            .key_context("HtmlPreview")
            .track_focus(&self.focus_handle(cx))
            .size_full()
            .min_h_0()
            .bg(background);
        let root = Self::with_page_keys(root, cx);
        // A live page scrolls itself and draws its own scrollbar. Wrapping it in
        // a scroll container gives one document two scrollers, and they fight:
        // the wheel moves both, and the picture slides under the pointer.
        if live_page {
            return root
                .child(
                    v_flex()
                        .size_full()
                        .child(self.render_address_bar(window, cx))
                        .child(
                            div()
                                .size_full()
                                .min_h_0()
                                .child(self.render_page(window, cx)),
                        ),
                )
                .into_any_element();
        }
        root.child(
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
        .into_any_element()
    }
}

impl Item for HtmlPreviewView {
    type Event = ();

    /// The pane has moved on to another tab. Whatever page this preview holds is
    /// now behind it, and is told to hold back until it is looked at again.
    #[cfg(feature = "servo")]
    fn deactivated(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.on_screen = false;
    }

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

/// The address bar's own little editor: one line, no gutter, and a hint of what
/// it is for when it is empty.
#[cfg(feature = "servo")]
fn address_bar(window: &mut Window, cx: &mut App) -> Entity<Editor> {
    cx.new(|cx| {
        let mut editor = Editor::single_line(window, cx);
        editor.set_placeholder_text("Address, or something to search for", window, cx);
        editor
    })
}

/// Whether a page should read as dark. The reader's choice for previews comes
/// first -- prose is often easier light while the code around it stays dark --
/// and the editor's own theme answers when no choice has been made.
#[cfg(feature = "servo")]
fn reading_in_the_dark(cx: &gpui::App) -> bool {
    use workspace::preview_appearance::preview_appearance;

    let appearance = preview_appearance(cx)
        .resolve()
        .unwrap_or_else(|| cx.theme().appearance());
    !appearance.is_light()
}

/// The scrollbar beside a page shows itself when the editor's own do: a preview
/// that kept its bar while the editor hid theirs would look like another
/// application.
#[cfg(feature = "servo")]
#[derive(Default)]
struct PageScrollbarSetting;

#[cfg(feature = "servo")]
impl ui::scrollbars::ScrollbarVisibility for PageScrollbarSetting {
    fn visibility(&self, cx: &gpui::App) -> ui::scrollbars::ShowScrollbar {
        editor::EditorSettings::get_global(cx).scrollbar.show
    }
}

/// How many pixels the page is drawn with for each of the editor's own: the
/// display's, unless the reader has asked for fewer to buy smoothness.
#[cfg(feature = "servo")]
fn page_scale(window: &gpui::Window, cx: &gpui::App) -> f32 {
    use crate::html_preview_settings::HtmlPreviewSettings;

    HtmlPreviewSettings::get_global(cx).scale_in(window.scale_factor())
}
