use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use editor::{Editor, EditorEvent, MultiBufferOffset};
use gpui::{
    App, ClipboardItem, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, ImageSource,
    MouseButton, MouseDownEvent, Pixels, Point, Resource, RetainAllImageCache, ScrollHandle,
    SharedUri, Subscription, Task, WeakEntity, anchored, canvas, deferred,
};
use html5ever::driver::ParseOpts;
use html5ever::parse_document;
use html5ever::serialize::SerializeOpts;
use html5ever::tendril::TendrilSink;
use html5ever::tree_builder::TreeBuilderOpts;
use language::{Buffer, LanguageRegistry};
use markdown::{
    CodeBlockRenderer, CopyButtonVisibility, Markdown, MarkdownElement, MarkdownOptions,
};
use markup5ever_rcdom::{Handle, NodeData, RcDom, SerializableHandle};
use settings::Settings;
use theme_settings::ThemeSettings;
use ui::utils::WithRemSize;
use ui::{ContextMenu, Tooltip, WithScrollbar, prelude::*};
use workspace::item::Item;
use workspace::{Pane, Workspace};

#[cfg(feature = "servo")]
use crate::{FindInPage, FindNextInPage, FindPreviousInPage, NewBrowserTab, StopFindingInPage};
use crate::{OpenPreview, OpenPreviewToTheSide};

const REPARSE_DEBOUNCE: Duration = Duration::from_millis(200);
/// Where a new browser tab starts. The search engine sends this straight on to
/// its plain address, so that is what the address bar reads, and on the way it
/// leaves behind the mark that stops it redirecting to whichever country the
/// request came from -- which is how the same tab came up in a different
/// language in every country.
const NEW_TAB_PAGE: &str = "https://www.google.com/ncr";
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

/// What the page's own menu offers. Named here because the tests look the items
/// up by the words the reader sees.
const OPEN_LINK: &str = "Open Link in New Browser Tab";
const COPY_LINK: &str = "Copy Link Address";
const OPEN_IMAGE: &str = "Open Image in New Browser Tab";
const COPY_IMAGE: &str = "Copy Image Address";
const COPY_SELECTION: &str = "Copy";
const SEARCH_THE_WEB: &str = "Search the Web for the Selection";
const SELECT_ALL: &str = "Select All";
const SAVE_PAGE: &str = "Save Page As…";
const VIEW_SOURCE: &str = "View Page Source";
const INSPECT: &str = "Inspect";

/// What the tab holding a page's own HTML is called.
const SOURCE_TITLE: &str = "Page Source";

/// What a saved page is called when the reader has not said otherwise.
const SAVED_PAGE: &str = "page.html";

/// How the area the page is drawn in is found when its painted bounds are being
/// measured. A no-op outside a test build.
const PAGE_AREA: &str = "html-preview-page-area";

/// What the page has under the pointer, as the page itself reports it. Every
/// field is absent when there is nothing of that kind there, which is what
/// decides whether the menu offers anything about it.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Deserialize)]
struct PageTarget {
    /// The address of the nearest link the pointer is inside.
    #[serde(default)]
    link: Option<String>,
    /// The address of the image under the pointer.
    #[serde(default)]
    image: Option<String>,
    /// What the reader has picked in the page, as much of it as is worth
    /// carrying. The whole of it is asked for again when it is copied.
    #[serde(default)]
    selection: Option<String>,
}

impl PageTarget {
    /// What the page answered. Anything empty is read as nothing at all: a link
    /// with no address is not a link.
    #[cfg(any(test, feature = "servo"))]
    fn parse(answer: &str) -> Self {
        let target = match serde_json::from_str::<Self>(answer) {
            Ok(target) => target,
            Err(error) => {
                log::warn!("the page did not say what is under the pointer: {error:#}");
                Self::default()
            }
        };
        let something = |text: Option<String>| text.filter(|text| !text.trim().is_empty());
        Self {
            link: something(target.link),
            image: something(target.image),
            selection: something(target.selection),
        }
    }
}

/// Where the page's own HTML is headed, once the page has handed it over. The
/// page answers on a later turn, by which time nothing else says what it was
/// asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SourceGoes {
    /// Into an editor tab, to be read and searched.
    ToATab,
    /// Onto disk, wherever the reader says.
    ToAFile,
}

/// What a menu item asks of the page. Only the engine can carry any of these
/// out, so they are named here and handed over in one place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PageRequest {
    GoBack,
    GoForward,
    Reload,
    SelectAll,
    CopySelection,
}

/// What the page would answer if there were one. No engine starts in a test --
/// it needs a rendering surface and there is none -- so the answers a right
/// click depends on are seeded here instead of asked for.
#[cfg(test)]
#[derive(Clone, Default)]
struct PageStandIn {
    under: PageTarget,
    behind_and_ahead: (bool, bool),
    source: String,
}

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
    /// Where the page is painted, so a click can be told where it landed and a
    /// resize can be passed on to the engine.
    page_bounds: std::rc::Rc<std::cell::Cell<gpui::Bounds<gpui::Pixels>>>,
    /// The menu the reader opened on the page, and the window position they
    /// opened it at, so it is drawn where they clicked.
    context_menu: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
    /// What the page answered about the point last right-clicked, and where
    /// that click was. The engine answers on a later turn of its own loop, so
    /// the menu is opened from here rather than where it was asked for.
    #[cfg(any(test, feature = "servo"))]
    what_is_under_the_pointer: std::rc::Rc<std::cell::RefCell<Option<(Point<Pixels>, PageTarget)>>>,
    /// The live page, when this build has an engine: a real document that lays
    /// itself out, runs its scripts and answers the mouse. Without one the
    /// Markdown rendering below is what the reader sees.
    #[cfg(feature = "servo")]
    page: Option<html_render::HtmlPage>,
    #[cfg(feature = "servo")]
    frame: Option<std::sync::Arc<gpui::RenderImage>>,
    /// The page's own buffer, when the window can draw it where it lies. While
    /// this is set, no frame is ever copied through memory.
    #[cfg(all(feature = "servo", any(target_os = "linux", target_os = "windows")))]
    shared_frame: Option<std::sync::Arc<gpui::SharedFrame>>,
    #[cfg(feature = "servo")]
    pump: Option<Task<()>>,
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
    /// The page's own HTML, once it has handed it over, and where it is headed.
    /// Left here for the same reason as the text above.
    #[cfg(feature = "servo")]
    page_source: std::rc::Rc<std::cell::RefCell<Option<(SourceGoes, String)>>>,
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
    /// Where the reader types what to look for in the page, once they have asked
    /// to look for anything.
    #[cfg(feature = "servo")]
    looking_for: Option<Entity<Editor>>,
    /// Which of the places the words appear the reader is at, and how many there
    /// are, as the page last answered.
    #[cfg(feature = "servo")]
    found: (usize, usize),
    /// Where the page leaves that answer, since it arrives on a turn of the
    /// engine's own.
    #[cfg(feature = "servo")]
    answer_from_the_page: std::rc::Rc<std::cell::Cell<Option<(usize, usize)>>>,
    /// Kept so a change to the reading theme reaches this preview at once. The
    /// page would otherwise hear about it on the pump's next turn, which is why
    /// the button looked as though it took two presses.
    #[cfg(feature = "servo")]
    _appearance: Subscription,
    /// Stands in for the page in tests, where no engine can start.
    #[cfg(test)]
    stand_in: Option<PageStandIn>,
    /// What the menu asked of the page, for a test to look at. Only the seam
    /// above ever puts anything here.
    #[cfg(test)]
    asked_of_the_page: std::rc::Rc<std::cell::RefCell<Vec<PageRequest>>>,
    /// The point in the page's own coordinates that the last right click was
    /// turned into, so a test can check the conversion the engine would be
    /// given.
    #[cfg(test)]
    asked_about_the_point: std::rc::Rc<std::cell::Cell<Option<Point<Pixels>>>>,
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

        #[cfg(feature = "servo")]
        workspace.register_action(move |workspace, _: &NewBrowserTab, window, cx| {
            let view = Self::create_browser_tab(workspace, window, cx);
            workspace.active_pane().update(cx, |pane, cx| {
                pane.add_item(Box::new(view.clone()), true, true, None, window, cx);
            });
            // The tab is shown before the page is asked for. Starting the engine
            // is the slow part and it happens on this thread, so opening first
            // left the window still for long enough that the press looked lost.
            window.defer(cx, move |window, cx| {
                view.update(cx, |view, cx| {
                    view.start_a_browser_tab(window, cx);
                });
            });
            cx.notify();
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

    /// The live page, for whoever needs to ask it something -- the developer's
    /// tools in the dock, and nothing else.
    #[cfg(feature = "servo")]
    pub fn page(&self) -> Option<&html_render::HtmlPage> {
        self.page.as_ref()
    }

    /// A page with nothing behind it: the reader types where to go. It is the
    /// same preview in every other way -- the same engine, the same address bar,
    /// the same page -- only without a document of the editor's to follow.
    #[cfg(feature = "servo")]
    fn create_browser_tab(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let language_registry = workspace.project().read(cx).languages().clone();
        let weak = workspace.weak_handle();
        Self::new_empty(weak, language_registry, window, cx)
    }

    /// Sends a fresh browser tab to its first page. Kept apart from building the
    /// tab so the tab can be on screen before the engine is asked for anything.
    #[cfg(feature = "servo")]
    fn start_a_browser_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Sent straight to the engine rather than through the address bar's own
        // reading of what is typed, which would have searched for the address as
        // words.
        self.open_the_page(url::Url::parse(NEW_TAB_PAGE).ok(), window, cx);
        self.address.focus_handle(cx).focus(window, cx);
    }

    pub fn new(
        active_editor: Entity<Editor>,
        workspace: WeakEntity<Workspace>,
        language_registry: Arc<LanguageRegistry>,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        Self::build(
            Some(active_editor),
            workspace,
            language_registry,
            window,
            cx,
        )
    }

    fn build(
        active_editor: Option<Entity<Editor>>,
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
                page_bounds: std::rc::Rc::new(std::cell::Cell::new(gpui::Bounds::default())),
                context_menu: None,
                #[cfg(any(test, feature = "servo"))]
                what_is_under_the_pointer: std::rc::Rc::new(std::cell::RefCell::new(None)),
                #[cfg(feature = "servo")]
                page: None,
                #[cfg(feature = "servo")]
                frame: None,
                #[cfg(all(feature = "servo", any(target_os = "linux", target_os = "windows")))]
                shared_frame: None,
                #[cfg(feature = "servo")]
                pump: None,
                #[cfg(feature = "servo")]
                page_pressed: std::rc::Rc::new(std::cell::Cell::new(None)),
                #[cfg(feature = "servo")]
                on_screen: true,
                #[cfg(feature = "servo")]
                pending_copy: std::rc::Rc::new(std::cell::RefCell::new(None)),
                #[cfg(feature = "servo")]
                page_source: std::rc::Rc::new(std::cell::RefCell::new(None)),
                #[cfg(feature = "servo")]
                page_scroll: crate::page_scroll::PageScrollHandle::default(),
                #[cfg(feature = "servo")]
                asked_where: None,
                #[cfg(feature = "servo")]
                address: address_bar(window, cx),
                #[cfg(feature = "servo")]
                showing_address: None,
                #[cfg(feature = "servo")]
                looking_for: None,
                #[cfg(feature = "servo")]
                found: (0, 0),
                #[cfg(feature = "servo")]
                answer_from_the_page: std::rc::Rc::new(std::cell::Cell::new(None)),
                #[cfg(feature = "servo")]
                _appearance: workspace::preview_appearance::observe_preview_appearance(cx),
                #[cfg(test)]
                stand_in: None,
                #[cfg(test)]
                asked_of_the_page: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
                #[cfg(test)]
                asked_about_the_point: std::rc::Rc::new(std::cell::Cell::new(None)),
            };
            #[cfg(feature = "servo")]
            cx.on_release_in(window, |this: &mut Self, window, _| {
                // In this order: nothing may turn the engine over once the page
                // is going, and the window must let go of the page's buffer
                // before the page lets go of the memory behind it.
                this.pump.take();
                #[cfg(any(target_os = "linux", target_os = "windows"))]
                this.shared_frame.take();
                this.page.take();
                // The last frame a closed preview painted would otherwise sit
                // in the sprite atlas for as long as the window lives.
                if let Some(frame) = this.frame.take()
                    && let Err(error) = window.drop_image(frame)
                {
                    log::warn!("the page's last frame could not be released: {error:#}");
                }
            })
            .detach();
            if let Some(active_editor) = active_editor {
                this.set_editor(active_editor, window, cx);
            }
            this
        })
    }

    /// The same preview with no document behind it, for a page the reader goes
    /// to rather than one the editor holds.
    pub fn new_empty(
        workspace: WeakEntity<Workspace>,
        language_registry: Arc<LanguageRegistry>,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        Self::build(None, workspace, language_registry, window, cx)
    }

    /// Sends the page somewhere, starting the engine if this preview has not
    /// needed it yet.
    #[cfg(feature = "servo")]
    fn open_the_page(
        &mut self,
        going: Option<url::Url>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(going) = going else {
            return;
        };
        if let Some(page) = self.page.as_mut() {
            page.go_to(going);
            return;
        }
        let bounds = self.page_bounds.get();
        let size = match bounds.size.width > gpui::px(64.) {
            true => bounds.size,
            false => gpui::size(gpui::px(900.), gpui::px(700.)),
        };
        pass_engine_options(cx);
        match html_render::HtmlPage::open_at(going, size, page_scale(window, cx), cx) {
            Ok(page) => {
                self.page = Some(page);
                self.start_pumping(cx);
                cx.notify();
            }
            Err(error) => log::warn!("the HTML engine did not open the page: {error:#}"),
        }
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
        pass_engine_options(cx);
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
            let mut frames = HowTheFramesGo::default();
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
                frames.turned();
                let turn = view.update_in(cx, |view, window, cx| {
                    let out_of_sight = !view.on_screen;
                    let Some(page) = view.page.as_mut() else {
                        return None;
                    };
                    page.set_throttled(out_of_sight);
                    page.set_dark(reading_in_the_dark(cx));
                    if out_of_sight {
                        return Some((true, None));
                    }
                    let bounds = view.page_bounds.get();
                    let scale = page_scale(window, cx);
                    if bounds.size.width > gpui::px(64.) {
                        page.resize(bounds.size, scale);
                    }
                    let painted = page.pump();
                    let drew = painted.then_some((bounds.size, scale));
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
                    if let Some(found) = view.answer_from_the_page.take() {
                        view.found = found;
                        cx.notify();
                    }
                    // A drag of the scrollbar's thumb is a request to the page,
                    // which only the engine can carry out.
                    if let Some(down) = view.page_scroll.take_request() {
                        page.scroll_to(down);
                    }
                    // Where the page stands is a script's answer, which arrives
                    // on a later turn. A page on the move is asked on every turn:
                    // told only ten times a second, the bar arrives in steps
                    // behind a page gliding under the wheel. A page at rest has
                    // not moved, so the slower pace is enough for it. Nothing has
                    // to be redrawn when the answer lands: a page that is moving
                    // is painting.
                    let often_enough = match view.page_scroll.moving() {
                        true => Duration::ZERO,
                        false => WHERE_THE_PAGE_STANDS,
                    };
                    let due = view
                        .asked_where
                        .is_none_or(|asked| asked.elapsed() >= often_enough);
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
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                    }
                    // The same goes for what a right click asked about and for
                    // the page's own HTML: both are answers to a script, and
                    // both belong to every path through this loop. Both are
                    // acted on once this update is over -- opening the menu
                    // takes the view, and opening a tab tells this one it has
                    // been left, neither of which can happen from inside here.
                    take_what_is_under_the_pointer(&view.what_is_under_the_pointer, window, cx);
                    let source = view.page_source.borrow_mut().take();
                    if let Some((goes, source)) = source {
                        let workspace = view.workspace.clone();
                        // Deferred without the view: a new tab tells the one it
                        // replaces that it has been left, and this preview may
                        // well be that one.
                        window.defer(cx, move |window, cx| match goes {
                            SourceGoes::ToATab => {
                                show_the_page_source(&workspace, source, window, cx)
                            }
                            SourceGoes::ToAFile => {
                                if let Some(files) = the_projects_files(&workspace, cx) {
                                    save_the_page_source(source, files, cx);
                                }
                            }
                        });
                    }
                    #[cfg(any(target_os = "linux", target_os = "windows"))]
                    if view
                        .shared_frame
                        .as_ref()
                        .is_some_and(|shared| shared.is_refused())
                    {
                        // The window could not draw the page's own buffer after
                        // all, and the page has gone back to copying frames.
                        view.shared_frame = None;
                    }
                    #[cfg(any(target_os = "linux", target_os = "windows"))]
                    if painted && let Some(shared) = page.shared_frame() {
                        view.shared_frame = Some(shared);
                        // What was copied before is not what is drawn now.
                        if let Some(superseded) = view.frame.take()
                            && let Err(error) = window.drop_image(superseded)
                        {
                            log::debug!("a superseded frame could not be released: {error:#}");
                        }
                        cx.notify();
                        return Some((false, drew));
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
                    Some((false, drew))
                });
                match turn {
                    Ok(Some((out_of_sight, drew))) => {
                        unseen = out_of_sight;
                        if let Some((at, scale)) = drew {
                            frames.painted(at, scale);
                        }
                    }
                    // The page is gone, or the view is: either way there is
                    // nothing left to drive.
                    Ok(None) | Err(_) => break,
                }
            }
        }));
    }

    /// Whether the engine's page is what the reader is looking at.
    fn showing_live_page(&self) -> bool {
        #[cfg(test)]
        if self.stand_in.is_some() {
            return true;
        }
        #[cfg(feature = "servo")]
        {
            if self.frame.is_some() {
                return true;
            }
            #[cfg(any(target_os = "linux", target_os = "windows"))]
            if self.shared_frame.is_some() {
                return true;
            }
        }
        false
    }

    /// Whether there is anywhere behind where the page is now, and anywhere
    /// ahead of it.
    fn can_go(&self) -> (bool, bool) {
        #[cfg(test)]
        if let Some(stand_in) = self.stand_in.as_ref() {
            return stand_in.behind_and_ahead;
        }
        #[cfg(feature = "servo")]
        if let Some(page) = self.page.as_ref() {
            return page.can_go();
        }
        (false, false)
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
            // What is being typed into the address bar, or into the search
            // field, is not for the page.
            if view.typing_beside_the_page(window, cx) {
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
            if view.typing_beside_the_page(window, cx) {
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
    fn render_page(&self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        if self.showing_live_page() {
            return self.page_area(window, cx);
        }
        #[cfg(feature = "servo")]
        if self.page.is_some() {
            // The engine has the document and is drawing it. Showing the
            // Markdown rendering of the source in the meantime, and replacing it
            // with the page a moment later, looks like two previews arriving one
            // after the other; an empty pane is what a browser shows while a
            // page is on its way.
            return div()
                .size_full()
                .bg(cx.theme().colors().editor_background)
                .into_any_element();
        }
        self.render_markdown_element(window, cx).into_any_element()
    }

    /// The area the page is drawn in, and everything the pointer does over it.
    ///
    /// The page's own input is delivered through window-level listeners rather
    /// than the element's: an element only hears about the mouse while it is
    /// hovered, and a drag -- which is how text is selected -- is exactly the
    /// case where that stops being true. The right button is the exception. Its
    /// click is the editor's own menu, so it is taken from the element, where a
    /// menu that has opened over the page can occlude it.
    fn page_area(&self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let bounds_cell = self.page_bounds.clone();
        let area = div()
            .size_full()
            .relative()
            .debug_selector(|| PAGE_AREA.into())
            // The area's own bounds are what a click is measured against, and a
            // div does not report its own: they are read from a child that fills
            // it. An absolutely positioned child is used so that nothing here
            // takes room from the picture.
            .child(
                canvas(move |bounds, _, _| bounds_cell.set(bounds), |_, _, _, _| ())
                    .absolute()
                    .size_full(),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|view, event: &MouseDownEvent, window, cx| {
                    view.right_clicked_on_the_page(event.position, window, cx);
                    cx.stop_propagation();
                }),
            );
        #[cfg(feature = "servo")]
        let area = self.with_page_mouse(area, window, cx);
        #[cfg(not(feature = "servo"))]
        let area = {
            let _ = window;
            area.into_any_element()
        };
        area
    }

    /// The picture the engine painted, and the window-level listeners that hand
    /// the pointer to it.
    #[cfg(feature = "servo")]
    fn with_page_mouse(
        &self,
        area: gpui::Div,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        #[cfg(any(target_os = "linux", target_os = "windows"))]
        let shared = self.shared_frame.clone();
        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        let shared: Option<()> = None;
        let frame = self.frame.clone();
        let bounds_cell = self.page_bounds.clone();
        let holding = self.page_pressed.clone();
        let view = cx.entity().downgrade();
        let scroll = self.page_scroll.clone();
        let focus = self.focus_handle.clone();
        area.when_some(frame, |this, frame| {
            this.child(gpui::img(gpui::ImageSource::Render(frame)).size_full())
        })
        .child(
            gpui::canvas(
                // A hitbox of the page's own. The listeners below are hung from
                // the window, so they hear every click in it, including the ones
                // that land on whatever is drawn over the page -- its own menu,
                // say. This is what they ask whether the pointer really is on
                // the page rather than on something above it.
                |bounds, window, _| window.insert_hitbox(bounds, gpui::HitboxBehavior::Normal),
                move |painted_bounds, page: gpui::Hitbox, window, _| {
                    // The page's own buffer, drawn where the graphics card
                    // already holds it.
                    #[cfg(any(target_os = "linux", target_os = "windows"))]
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
                        let page = page.clone();
                        move |event: &gpui::MouseMoveEvent, phase, window, cx| {
                            if phase != gpui::DispatchPhase::Bubble {
                                return;
                            }
                            // A drag that started on the page keeps steering it
                            // even when the pointer wanders off the edge, which
                            // is what selecting to the end of a line does. A
                            // drag that started elsewhere is somebody else's.
                            let on_the_page = inside(event.position) && page.is_hovered(window);
                            if !on_the_page && holding.get().is_none() {
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
                        let page = page.clone();
                        move |event: &gpui::MouseDownEvent, phase, window, cx| {
                            if phase != gpui::DispatchPhase::Bubble
                                || !inside(event.position)
                                || !page.is_hovered(window)
                            {
                                return;
                            }
                            let Some(button) = servo_button(event.button) else {
                                return;
                            };
                            holding.set(Some(event.button));
                            view.update(cx, |view, _| {
                                if let Some(page) = view.page.as_ref() {
                                    page.mouse_down(page_point(event.position), button);
                                }
                            })
                            .ok();
                            // Whoever is clicked on is who types next: a page
                            // that takes the mouse but leaves the keyboard
                            // with the source cannot be typed into. Asked for
                            // outside the update above, so that whatever
                            // focusing sets off does not arrive in the middle
                            // of it.
                            focus.focus(window, cx);
                        }
                    });
                    window.on_mouse_event({
                        let view = view.clone();
                        let holding = holding.clone();
                        let page = page.clone();
                        move |event: &gpui::MouseUpEvent, phase, window, cx| {
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
                            } else if !inside(event.position) || !page.is_hovered(window) {
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
                        // The wheel asks a question of its own: a scrollbar over
                        // the page takes the pointer but still turns the page
                        // under it.
                        move |event: &gpui::ScrollWheelEvent, phase, window, cx| {
                            if phase != gpui::DispatchPhase::Bubble
                                || !inside(event.position)
                                || !page.should_handle_scroll(window)
                            {
                                return;
                            }
                            let delta = event.delta.pixel_delta(window.line_height());
                            view.update(cx, |view, _| {
                                if let Some(page) = view.page.as_ref() {
                                    page.scrolled(page_point(event.position), delta);
                                }
                                // The bar moves with the wheel rather than at the
                                // next time the page is asked where it stands.
                                view.page_scroll.wheeled_by(-f32::from(delta.y));
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

    /// The page was right-clicked. What the menu should offer depends on what is
    /// under the pointer, and only the page knows that, so nothing opens here:
    /// the page is asked, and the menu opens when the answer arrives.
    fn right_clicked_on_the_page(
        &mut self,
        at: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Measured against the area the page is drawn in, the same way every
        // other click the page is told about is.
        let bounds = self.page_bounds.get();
        let in_the_page = gpui::point(at.x - bounds.origin.x, at.y - bounds.origin.y);
        #[cfg(test)]
        if let Some(stand_in) = self.stand_in.clone() {
            self.asked_about_the_point.set(Some(in_the_page));
            // The engine's answer would arrive on a later turn; this one is to
            // hand, and goes through the same place the engine's would.
            *self.what_is_under_the_pointer.borrow_mut() = Some((at, stand_in.under));
            take_what_is_under_the_pointer(&self.what_is_under_the_pointer, window, cx);
            return;
        }
        #[cfg(feature = "servo")]
        if let Some(page) = self.page.as_ref() {
            let waiting = self.what_is_under_the_pointer.clone();
            page.what_is_under(in_the_page, move |answer| {
                *waiting.borrow_mut() = Some((at, PageTarget::parse(&answer)));
            });
            return;
        }
        let _ = (in_the_page, window, cx);
    }

    /// Puts the page's menu on screen where the reader clicked, offering what
    /// belongs to whatever the page has there.
    fn deploy_page_menu(
        &mut self,
        at: Point<Pixels>,
        target: PageTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (behind, ahead) = self.can_go();
        let view = cx.entity().downgrade();
        let context = self.focus_handle.clone();
        let menu = ContextMenu::build(window, cx, move |menu, _, _| {
            let menu = menu
                .context(context)
                .custom_row({
                    let view = view.clone();
                    move |_, _| navigation_row(&view, behind, ahead)
                })
                .separator();
            let menu = match target.link {
                Some(link) => {
                    let copying = link.clone();
                    menu.entry(OPEN_LINK, None, {
                        let view = view.clone();
                        move |window, cx| open_a_browser_tab(&view, &link, window, cx)
                    })
                    .entry(COPY_LINK, None, move |_, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copying.clone()))
                    })
                    .separator()
                }
                None => menu,
            };
            let menu = match target.image {
                Some(image) => {
                    let copying = image.clone();
                    menu.entry(OPEN_IMAGE, None, {
                        let view = view.clone();
                        move |window, cx| open_a_browser_tab(&view, &image, window, cx)
                    })
                    .entry(COPY_IMAGE, None, move |_, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copying.clone()))
                    })
                    .separator()
                }
                None => menu,
            };
            let menu = match target.selection {
                Some(selection) => menu
                    .entry(COPY_SELECTION, None, {
                        let view = view.clone();
                        move |_, cx| {
                            view.update(cx, |view, cx| {
                                view.tell_the_page(PageRequest::CopySelection, cx);
                            })
                            .ok();
                        }
                    })
                    .entry(SEARCH_THE_WEB, None, {
                        let view = view.clone();
                        move |window, cx| search_the_web_for(&view, &selection, window, cx)
                    })
                    .separator(),
                None => menu,
            };
            menu.entry(SELECT_ALL, None, {
                let view = view.clone();
                move |_, cx| {
                    view.update(cx, |view, cx| {
                        view.tell_the_page(PageRequest::SelectAll, cx);
                    })
                    .ok();
                }
            })
            .separator()
            .entry(SAVE_PAGE, None, {
                let view = view.clone();
                move |window, cx| {
                    ask_the_page_for_its_source(&view, SourceGoes::ToAFile, window, cx)
                }
            })
            .entry(VIEW_SOURCE, None, move |window, cx| {
                ask_the_page_for_its_source(&view, SourceGoes::ToATab, window, cx)
            })
            .action(INSPECT, Box::new(crate::ToggleFocus))
        });
        window.focus(&menu.focus_handle(cx), cx);
        let subscription = cx.subscribe(&menu, |view, _, _: &DismissEvent, cx| {
            view.context_menu.take();
            cx.notify();
        });
        self.context_menu = Some((menu, at, subscription));
        cx.notify();
    }

    /// The menu, drawn over everything else at the point it was opened at.
    ///
    /// Anchored to the window rather than to the element it hangs from, so it
    /// lands where the reader clicked however far whatever holds this preview
    /// has been scrolled.
    fn render_page_menu(&self) -> Option<gpui::AnyElement> {
        let (menu, at, _) = self.context_menu.as_ref()?;
        Some(
            deferred(
                anchored()
                    .position(*at)
                    .anchor(gpui::Anchor::TopLeft)
                    .child(menu.clone()),
            )
            .with_priority(3)
            .into_any_element(),
        )
    }

    /// Takes the page's menu away and gives the keyboard back to the page.
    fn dismiss_page_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.context_menu.take().is_some() {
            self.focus_handle.focus(window, cx);
            cx.notify();
        }
    }

    /// Hands a request to the page. Only the engine can carry any of these out,
    /// so a build without one has nothing to do.
    fn tell_the_page(&mut self, request: PageRequest, cx: &mut Context<Self>) {
        #[cfg(test)]
        self.asked_of_the_page.borrow_mut().push(request);
        #[cfg(feature = "servo")]
        if let Some(page) = self.page.as_ref() {
            match request {
                PageRequest::GoBack => page.go_back(),
                PageRequest::GoForward => page.go_forward(),
                PageRequest::Reload => page.refresh(),
                PageRequest::SelectAll => page.select_all(),
                PageRequest::CopySelection => {
                    // The page answers on a turn of its own, and the driver puts
                    // what comes back on the clipboard.
                    let waiting = self.pending_copy.clone();
                    page.selected_text(move |text| {
                        *waiting.borrow_mut() = Some(text);
                    });
                }
            }
        }
        #[cfg(not(any(test, feature = "servo")))]
        let _ = request;
        cx.notify();
    }

    /// Sends this preview's page to an address, when the build has an engine to
    /// send it with.
    fn go_to(&mut self, going: url::Url, window: &mut Window, cx: &mut Context<Self>) {
        #[cfg(feature = "servo")]
        self.open_the_page(Some(going), window, cx);
        #[cfg(not(feature = "servo"))]
        let _ = (going, window, cx);
    }

    /// The line above the page: where it is, where it has been, and where the
    /// reader would like to go. What is typed here is an address if it looks
    /// like one and a search if it does not.
    #[cfg(feature = "servo")]
    fn render_address_bar(&self, _window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let loading = self.page.as_ref().and_then(|page| page.how_far_loaded());
        let (behind, ahead) = self.can_go();
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
            .py_0p5()
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
                IconButton::new("html-preview-refresh", IconName::RotateCw)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Reload this page"))
                    .on_click(cx.listener(|view, _, _, cx| {
                        if let Some(page) = view.page.as_ref() {
                            page.refresh();
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
            .child({
                // The switch that floats over a document is painted over the
                // source, not over this: a preview opened in a tab of its own
                // has nowhere else to offer the choice.
                let appearance = workspace::preview_appearance::preview_appearance(cx);
                Button::new("html-preview-reading-theme", appearance.initial())
                    .label_size(LabelSize::Small)
                    .tooltip(Tooltip::text(appearance.tooltip()))
                    .on_click(move |_, _, cx| {
                        workspace::preview_appearance::set_preview_appearance(
                            appearance.next(),
                            cx,
                        );
                    })
            })
            .children(self.looking_for.as_ref().map(|field| {
                let (at, total) = self.found;
                h_flex()
                    .gap_1()
                    .child(
                        div()
                            .w(gpui::rems(12.))
                            .px_1p5()
                            .py_0p5()
                            .rounded_sm()
                            .bg(colors.editor_background)
                            .border_1()
                            .border_color(colors.border_variant)
                            .key_context("HtmlPreviewFind")
                            .on_action(cx.listener(|view, _: &menu::Confirm, _, cx| {
                                view.look_again(true, cx);
                            }))
                            .child(field.clone()),
                    )
                    .child(
                        Label::new(if total == 0 {
                            "no matches".to_string()
                        } else {
                            format!("{at} of {total}")
                        })
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    )
                    .child(
                        IconButton::new("html-preview-find-previous", IconName::ChevronUp)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Previous match"))
                            .on_click(cx.listener(|view, _, _, cx| view.look_again(false, cx))),
                    )
                    .child(
                        IconButton::new("html-preview-find-next", IconName::ChevronDown)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Next match"))
                            .on_click(cx.listener(|view, _, _, cx| view.look_again(true, cx))),
                    )
                    .child(
                        IconButton::new("html-preview-find-close", IconName::Close)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Stop looking"))
                            .on_click(
                                cx.listener(|view, _, window, cx| view.stop_looking(window, cx)),
                            ),
                    )
            }))
            // A page on its way says so along the bottom of the bar. The engine
            // reports three stages and no more, so the strip moves in steps
            // rather than pretending to know how much is left.
            .children(loading.map(|far| {
                div()
                    .absolute()
                    .bottom_0()
                    .left_0()
                    .w(gpui::relative(far))
                    .h(gpui::px(2.))
                    .bg(cx.theme().colors().text_accent)
            }))
            .into_any_element()
    }

    /// Whether the reader is typing into one of the fields above the page
    /// rather than into the page itself.
    #[cfg(feature = "servo")]
    fn typing_beside_the_page(&self, window: &Window, cx: &App) -> bool {
        self.address.focus_handle(cx).contains_focused(window, cx)
            || self
                .looking_for
                .as_ref()
                .is_some_and(|field| field.focus_handle(cx).contains_focused(window, cx))
    }

    /// Opens the search field, or puts the cursor back in it.
    #[cfg(feature = "servo")]
    fn start_looking(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.looking_for.is_none() {
            let field = cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_placeholder_text("Find in page", window, cx);
                editor
            });
            self.looking_for = Some(field);
        }
        if let Some(field) = self.looking_for.as_ref() {
            field.focus_handle(cx).focus(window, cx);
            field.update(cx, |field, cx| {
                field.select_all(&Default::default(), window, cx)
            });
        }
        cx.notify();
    }

    /// Looks for what has been typed, and takes the reader to the next place it
    /// appears -- or the one before.
    #[cfg(feature = "servo")]
    fn look_again(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(query) = self
            .looking_for
            .as_ref()
            .map(|field| field.read(cx).text(cx))
        else {
            return;
        };
        let Some(page) = self.page.as_ref() else {
            return;
        };
        if query.trim().is_empty() {
            page.stop_looking();
            self.found = (0, 0);
            cx.notify();
            return;
        }
        // The answer comes back on the engine's own turn, when there is no
        // context to hand, so it is left here and the pump picks it up.
        let answer = self.answer_from_the_page.clone();
        page.find(&query, forward, move |at, total| {
            answer.set(Some((at, total)));
        });
    }

    /// Closes the search field and leaves the page as it was.
    #[cfg(feature = "servo")]
    fn stop_looking(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.looking_for.take().is_some() {
            if let Some(page) = self.page.as_ref() {
                page.stop_looking();
            }
            self.found = (0, 0);
            self.focus_handle.focus(window, cx);
            cx.notify();
        }
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

    /// Nothing to show above a page there is no engine for.
    #[cfg(not(feature = "servo"))]
    fn render_address_bar(&self, _: &mut Window, _: &mut Context<Self>) -> gpui::AnyElement {
        div().into_any_element()
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

/// The row across the top of the page's menu, as a browser has: back, forward,
/// and fetch this page again.
fn navigation_row(
    view: &WeakEntity<HtmlPreviewView>,
    behind: bool,
    ahead: bool,
) -> gpui::AnyElement {
    h_flex()
        .gap_1()
        .px_1()
        .py_0p5()
        .child(navigation_button(
            "page-menu-back",
            IconName::ArrowLeft,
            "Back",
            !behind,
            view,
            PageRequest::GoBack,
        ))
        .child(navigation_button(
            "page-menu-forward",
            IconName::ArrowRight,
            "Forward",
            !ahead,
            view,
            PageRequest::GoForward,
        ))
        .child(navigation_button(
            "page-menu-reload",
            IconName::RotateCw,
            "Reload this page",
            false,
            view,
            PageRequest::Reload,
        ))
        .into_any_element()
}

fn navigation_button(
    id: &'static str,
    icon: IconName,
    tooltip: &'static str,
    disabled: bool,
    view: &WeakEntity<HtmlPreviewView>,
    request: PageRequest,
) -> impl IntoElement {
    let view = view.clone();
    div()
        .debug_selector(move || format!("PAGE_MENU-{id}"))
        .child(
            IconButton::new(id, icon)
                .icon_size(IconSize::Small)
                .disabled(disabled)
                .tooltip(Tooltip::text(tooltip))
                .on_click(move |_, window, cx| {
                    view.update(cx, |view, cx| {
                        view.tell_the_page(request, cx);
                        view.dismiss_page_menu(window, cx);
                    })
                    .ok();
                }),
        )
}

/// Opens an address in a browser tab of its own, beside the page it came from.
fn open_a_browser_tab(
    view: &WeakEntity<HtmlPreviewView>,
    address: &str,
    window: &mut Window,
    cx: &mut App,
) {
    let Ok(going) = url::Url::parse(address) else {
        log::warn!("the page offered an address the editor cannot read: {address}");
        return;
    };
    let Some(preview) = view.upgrade() else {
        return;
    };
    let Some(workspace) = preview.read(cx).workspace.upgrade() else {
        return;
    };
    workspace.update(cx, |workspace, cx| {
        let languages = workspace.project().read(cx).languages().clone();
        let tab = HtmlPreviewView::new_empty(workspace.weak_handle(), languages, window, cx);
        tab.update(cx, |tab, cx| tab.go_to(going, window, cx));
        workspace.active_pane().update(cx, |pane, cx| {
            pane.add_item(Box::new(tab), true, true, None, window, cx);
        });
    });
}

/// Asks the page for its own HTML, and says where it should end up. The engine
/// answers on a later turn and the page's driver takes it from there; only a
/// page standing in for one, in a test, has the answer to hand.
fn ask_the_page_for_its_source(
    view: &WeakEntity<HtmlPreviewView>,
    goes: SourceGoes,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(preview) = view.upgrade() else {
        return;
    };
    #[cfg(test)]
    {
        let seeded = {
            let preview = preview.read(cx);
            preview
                .stand_in
                .as_ref()
                .map(|stand_in| (preview.workspace.clone(), stand_in.source.clone()))
        };
        if let Some((workspace, source)) = seeded {
            match goes {
                SourceGoes::ToATab => show_the_page_source(&workspace, source, window, cx),
                SourceGoes::ToAFile => {
                    if let Some(files) = the_projects_files(&workspace, cx) {
                        save_the_page_source(source, files, cx);
                    }
                }
            }
            return;
        }
    }
    #[cfg(feature = "servo")]
    {
        let preview = preview.read(cx);
        if let Some(page) = preview.page.as_ref() {
            let waiting = preview.page_source.clone();
            page.evaluate("document.documentElement.outerHTML", move |source| {
                *waiting.borrow_mut() = Some((goes, source));
            });
        }
    }
    let _ = (preview, goes, window, cx);
}

/// Opens the menu once the page has said what is under the pointer. The answer
/// lands on a turn of the engine's own loop, so it is collected by whoever drives
/// the page rather than by the click that asked.
///
/// Takes the answer alone rather than the view: the loop that drives the page
/// holds the page mutably while this runs, and borrowing the whole view there
/// would not be allowed. Opening the menu is deferred for the same reason.
fn take_what_is_under_the_pointer(
    asked: &std::rc::Rc<std::cell::RefCell<Option<(Point<Pixels>, PageTarget)>>>,
    window: &mut Window,
    cx: &mut Context<HtmlPreviewView>,
) {
    let answer = asked.borrow_mut().take();
    if let Some((at, target)) = answer {
        cx.defer_in(window, move |view, window, cx| {
            view.deploy_page_menu(at, target, window, cx);
        });
    }
}

/// Writes the page's own HTML wherever the reader says. The dialog is the
/// platform's, so nothing here knows whether it was answered until it is.
///
/// The writing goes through the project's own file system rather than straight
/// to disk: that is the one the editor is holding, and it is the only way the
/// write is observable from a test -- a bare `smol::fs::write` lands on a thread
/// pool the test scheduler does not drive, so the file appears some time after
/// the test has finished looking for it.
fn save_the_page_source(source: String, fs: Arc<dyn fs::Fs>, cx: &mut App) {
    let where_to = cx.prompt_for_new_path(paths::home_dir(), Some(SAVED_PAGE));
    cx.background_spawn(async move {
        let chosen = match where_to.await {
            Ok(Ok(chosen)) => chosen,
            Ok(Err(error)) => {
                log::error!("the editor could not ask where to save the page: {error:#}");
                return;
            }
            // The dialog was closed without an answer, which is a reader
            // changing their mind rather than anything going wrong.
            Err(_) => return,
        };
        let Some(chosen) = chosen else {
            return;
        };
        if let Err(error) = fs.write(&chosen, source.as_bytes()).await {
            log::error!("the page could not be saved to {chosen:?}: {error:#}");
        }
    })
    .detach();
}

/// The file system the editor is holding, which is a real one in the editor and
/// a stand-in under test.
fn the_projects_files(workspace: &WeakEntity<Workspace>, cx: &App) -> Option<Arc<dyn fs::Fs>> {
    let workspace = workspace.upgrade()?;
    Some(workspace.read(cx).project().read(cx).fs().clone())
}

/// Opens a page's HTML as a read-only editor tab, so it can be read and searched
/// the way any other file is. A page is not a file, so this is a buffer of its
/// own rather than something on disk.
fn show_the_page_source(
    workspace: &WeakEntity<Workspace>,
    source: String,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(workspace) = workspace.upgrade() else {
        return;
    };
    workspace.update(cx, |workspace, cx| {
        let languages = workspace.project().read(cx).languages().clone();
        let buffer = cx.new(|cx| Buffer::local(source, cx));
        // The tab opens now; the colouring appears once the grammar is in.
        cx.spawn({
            let buffer = buffer.clone();
            async move |_, cx| match languages.language_for_name("HTML").await {
                Ok(html) => {
                    buffer.update(cx, |buffer, cx| buffer.set_language(Some(html), cx));
                }
                Err(error) => log::warn!("the page's source cannot be coloured: {error:#}"),
            }
        })
        .detach();
        let editor = cx.new(|cx| {
            let mut editor = Editor::for_buffer(buffer, None, window, cx);
            editor.set_read_only(true);
            editor
                .buffer()
                .update(cx, |buffer, cx| buffer.set_title(SOURCE_TITLE.into(), cx));
            editor
        });
        let pane = workspace.active_pane().clone();
        workspace.add_item(pane, Box::new(editor), None, true, true, window, cx);
    });
}

/// Looks the selection up wherever the reader has said searches go, in a browser
/// tab of its own.
fn search_the_web_for(
    view: &WeakEntity<HtmlPreviewView>,
    selection: &str,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(going) =
        crate::html_preview_settings::HtmlPreviewSettings::get_global(cx).where_to_go(selection)
    else {
        return;
    };
    open_a_browser_tab(view, going.as_str(), window, cx);
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
///
/// The right button is deliberately among those the page never hears. Its click
/// opens the editor's own menu, and a page that also sees it can open a second
/// one of its own over the top.
#[cfg(feature = "servo")]
fn servo_button(button: MouseButton) -> Option<html_render::MouseButton> {
    match button {
        MouseButton::Left => Some(html_render::MouseButton::Left),
        MouseButton::Middle => Some(html_render::MouseButton::Middle),
        MouseButton::Right | MouseButton::Navigate(_) => None,
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
        #[cfg(feature = "servo")]
        {
            // Applied here rather than only on the pump's next turn: the reading
            // theme is chosen with a button, and a button should take effect
            // when it is pressed.
            let dark = reading_in_the_dark(cx);
            if let Some(page) = self.page.as_mut() {
                page.set_dark(dark);
            }
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
        #[cfg(feature = "servo")]
        let root = root
            .on_action(cx.listener(|view, _: &FindInPage, window, cx| {
                view.start_looking(window, cx);
            }))
            .on_action(cx.listener(|view, _: &FindNextInPage, _, cx| {
                view.look_again(true, cx);
            }))
            .on_action(cx.listener(|view, _: &FindPreviousInPage, _, cx| {
                view.look_again(false, cx);
            }))
            .on_action(cx.listener(|view, _: &StopFindingInPage, window, cx| {
                view.stop_looking(window, cx);
            }));
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
                // Hung from the root rather than from the page's own area, so
                // nothing the page draws is ever over the menu.
                .children(self.render_page_menu())
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
                format!("Browser {}", buffer.title(cx)).into()
            })
            .unwrap_or_else(|| SharedString::from("Browser Page"))
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Browser Page Opened")
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

/// Hands the engine what it is to be set up with, before it is started. Only
/// what is said before then counts: there is one engine for the life of the
/// editor and Servo takes these when it starts.
#[cfg(feature = "servo")]
fn pass_engine_options(cx: &mut App) {
    let settings = crate::html_preview_settings::HtmlPreviewSettings::get_global(cx);
    let options = html_render::EngineOptions {
        devtools_port: settings.devtools_port,
        proxy: settings.proxy.as_deref().map(str::to_string),
    };
    html_render::set_engine_options(options, cx);
}

/// Whether a page should read as dark. The reader's choice for previews comes
/// first -- prose is often easier light while the code around it stays dark --
/// and the editor's own theme answers when no choice has been made.
#[cfg(feature = "servo")]
fn reading_in_the_dark(cx: &gpui::App) -> bool {
    use workspace::preview_appearance::preview_appearance;

    !preview_appearance(cx).appearance().is_light()
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
/// How the frames actually go, for the log.
///
/// A preview that feels slow is worth a number, and the number has to come from
/// the machine it feels slow on. What is timed is the whole way round: from the
/// moment work arrived to the frame that came of it, and how many turns of the
/// engine that took -- a frame needs several, and each one costs.
#[cfg(feature = "servo")]
#[derive(Default)]
struct HowTheFramesGo {
    /// When the work that this frame came of first arrived.
    began: Option<std::time::Instant>,
    turns: u32,
    took: Vec<Duration>,
    turns_each: Vec<u32>,
    said: Option<std::time::Instant>,
}

#[cfg(feature = "servo")]
impl HowTheFramesGo {
    /// How often what it has gathered reaches the log: often enough to watch a
    /// page being scrolled, seldom enough that the log is not the slow part.
    const HOW_OFTEN: Duration = Duration::from_secs(5);

    fn turned(&mut self) {
        self.turns = self.turns.saturating_add(1);
        self.began.get_or_insert_with(std::time::Instant::now);
    }

    fn painted(&mut self, at: gpui::Size<gpui::Pixels>, scale: f32) {
        let Some(began) = self.began.take() else {
            return;
        };
        self.took.push(began.elapsed());
        self.turns_each.push(self.turns);
        self.turns = 0;
        let said = *self.said.get_or_insert_with(std::time::Instant::now);
        if said.elapsed() < Self::HOW_OFTEN {
            return;
        }
        self.said = Some(std::time::Instant::now());
        self.took.sort_unstable();
        let middle = self.took.get(self.took.len() / 2).copied();
        let worst = self.took.iter().max().copied();
        let turns = match self.turns_each.is_empty() {
            true => 0,
            false => self.turns_each.iter().sum::<u32>() / self.turns_each.len() as u32,
        };
        if let (Some(middle), Some(worst)) = (middle, worst) {
            log::info!(
                "the page drew {} frames at {}x{} and {scale} pixels to one: {middle:?} at the \
                 middle, {worst:?} at worst, {turns} turns of the engine each",
                self.took.len(),
                (f32::from(at.width) * scale).round() as u32,
                (f32::from(at.height) * scale).round() as u32,
            );
        }
        self.took.clear();
        self.turns_each.clear();
    }
}

fn page_scale(window: &gpui::Window, cx: &gpui::App) -> f32 {
    use crate::html_preview_settings::HtmlPreviewSettings;

    HtmlPreviewSettings::get_global(cx).scale_in(window.scale_factor())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Bounds, Modifiers, TestAppContext, VisualTestContext, point, px};
    use project::Project;
    use workspace::AppState;

    const A_LINK: &str = "https://example.com/somewhere";

    const AN_IMAGE: &str = "https://example.com/picture.png";
    const A_SELECTION: &str = "how tall is a giraffe";
    const A_SOURCE: &str = "<html><body><p>Hello</p></body></html>";

    #[test]
    fn a_new_tab_starts_at_the_web_and_not_at_a_file_of_ours() {
        let start = url::Url::parse(NEW_TAB_PAGE).expect("the new tab's address has to parse");
        assert_eq!(
            start.scheme(),
            "https",
            "a new tab that starts at a file shows a path in /tmp instead of a page"
        );
        assert_eq!(start.host_str(), Some("www.google.com"));
        assert_eq!(
            start.query(),
            None,
            "the address bar reads whatever we send the tab to, so it carries no settings"
        );
    }

    /// The preview inside something that scrolls, because chrome drawn over a
    /// page has to land where the reader clicked at any scroll offset, not only
    /// at rest.
    struct PageFrame {
        preview: Entity<HtmlPreviewView>,
        workspace: Entity<Workspace>,
        scroll: ScrollHandle,
    }

    impl Render for PageFrame {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .id("page-frame")
                .size_full()
                .overflow_y_scroll()
                .track_scroll(&self.scroll)
                .child(div().w(px(900.)).h(px(600.)).child(self.preview.clone()))
                // Something below the preview, so the frame can really scroll.
                .child(div().w(px(900.)).h(px(1500.)))
        }
    }

    fn a_page_showing(under: PageTarget) -> PageStandIn {
        PageStandIn {
            under,
            behind_and_ahead: (false, false),
            source: A_SOURCE.to_string(),
        }
    }

    fn nothing_in_particular() -> PageTarget {
        PageTarget::default()
    }

    async fn a_page_frame(
        stand_in: PageStandIn,
        cx: &mut TestAppContext,
    ) -> (Entity<PageFrame>, &mut VisualTestContext) {
        let app_state = cx.update(|cx| {
            let app_state = AppState::test(cx);
            editor::init(cx);
            app_state
        });
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (frame, cx) = cx.add_window_view(|window, cx| {
            let workspace = cx.new(|cx| Workspace::test_new(project.clone(), window, cx));
            let languages = workspace.read(cx).project().read(cx).languages().clone();
            let preview = HtmlPreviewView::new_empty(workspace.downgrade(), languages, window, cx);
            preview.update(cx, |preview, _| preview.stand_in = Some(stand_in));
            PageFrame {
                preview,
                workspace,
                scroll: ScrollHandle::new(),
            }
        });
        cx.run_until_parked();
        draw(cx);
        (frame, cx)
    }

    fn draw(cx: &mut VisualTestContext) {
        cx.update(|window, cx| {
            window.refresh();
            window.draw(cx).clear();
        });
        cx.run_until_parked();
    }

    fn painted(cx: &mut VisualTestContext, selector: &'static str) -> Bounds<Pixels> {
        cx.debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} was expected to have been painted"))
    }

    fn right_click(cx: &mut VisualTestContext, at: Point<Pixels>) {
        cx.simulate_event(MouseDownEvent {
            position: at,
            button: MouseButton::Right,
            modifiers: Modifiers::none(),
            click_count: 1,
            first_mouse: false,
        });
        draw(cx);
    }

    /// Right-clicks the middle of the page and hands back where that was.
    fn right_click_the_page(cx: &mut VisualTestContext) -> Point<Pixels> {
        let page = painted(cx, PAGE_AREA);
        assert!(
            page.size.width > px(1.) && page.size.height > px(1.),
            "the page has to occupy real screen area, not {:?}",
            page.size
        );
        let at = page.center();
        right_click(cx, at);
        at
    }

    fn click(cx: &mut VisualTestContext, selector: &'static str) {
        let at = painted(cx, selector).center();
        cx.simulate_click(at, Modifiers::none());
        draw(cx);
    }

    /// How far from the click the menu's own first row may sit. Enough for the
    /// menu's padding, and far short of the distance to the page's own corner,
    /// which is where a menu positioned by its parent instead of by the click
    /// would land.
    const BESIDE_THE_CLICK: Pixels = px(64.);

    /// Checks that the menu is really on screen, and where the reader clicked.
    fn menu_is_at(cx: &mut VisualTestContext, at: Point<Pixels>) {
        let first = painted(cx, "PAGE_MENU-page-menu-back");
        assert!(
            first.size.width > px(1.) && first.size.height > px(1.),
            "the menu has to occupy real screen area, not {:?}",
            first.size
        );
        assert!(
            first.origin.x >= at.x && first.origin.x - at.x < BESIDE_THE_CLICK,
            "the menu has to open beside the click at {at:?}, not at {:?}",
            first.origin
        );
        assert!(
            first.origin.y >= at.y && first.origin.y - at.y < BESIDE_THE_CLICK,
            "the menu has to open below the click at {at:?}, not at {:?}",
            first.origin
        );
        let item = painted(cx, "MENU_ITEM-Select All");
        assert!(
            item.size.width > px(1.) && item.size.height > px(1.),
            "the menu's items have to occupy real screen area, not {:?}",
            item.size
        );
    }

    #[gpui::test]
    async fn a_right_click_over_the_page_opens_a_menu_where_it_was_clicked(
        cx: &mut TestAppContext,
    ) {
        let (frame, cx) = a_page_frame(a_page_showing(nothing_in_particular()), cx).await;

        let page = painted(cx, PAGE_AREA);
        let at = right_click_the_page(cx);
        menu_is_at(cx, at);

        // What the page would have been asked about is the click measured
        // against the area the page is drawn in, not the window.
        let asked = frame
            .read_with(cx, |frame, cx| {
                frame.preview.read(cx).asked_about_the_point.get()
            })
            .expect("the page has to be asked what is under the pointer");
        let expected = point(at.x - page.origin.x, at.y - page.origin.y);
        assert!(
            f32::from(asked.x - expected.x).abs() < 1.
                && f32::from(asked.y - expected.y).abs() < 1.,
            "the page has to be asked about {expected:?} in its own coordinates, not {asked:?}"
        );
    }

    #[gpui::test]
    async fn a_link_under_the_pointer_is_offered(cx: &mut TestAppContext) {
        let (_frame, cx) = a_page_frame(
            a_page_showing(PageTarget {
                link: Some(A_LINK.to_string()),
                ..PageTarget::default()
            }),
            cx,
        )
        .await;

        right_click_the_page(cx);

        assert!(
            cx.debug_bounds("MENU_ITEM-Open Link in New Browser Tab")
                .is_some()
        );
        assert!(cx.debug_bounds("MENU_ITEM-Copy Link Address").is_some());
        assert!(
            cx.debug_bounds("MENU_ITEM-Copy Image Address").is_none(),
            "there is no image under the pointer, so nothing about one belongs in the menu"
        );
        assert!(
            cx.debug_bounds("MENU_ITEM-Copy").is_none(),
            "nothing is selected, so there is nothing to copy"
        );
    }

    #[gpui::test]
    async fn an_image_under_the_pointer_is_offered(cx: &mut TestAppContext) {
        let (_frame, cx) = a_page_frame(
            a_page_showing(PageTarget {
                image: Some(AN_IMAGE.to_string()),
                ..PageTarget::default()
            }),
            cx,
        )
        .await;

        right_click_the_page(cx);

        assert!(
            cx.debug_bounds("MENU_ITEM-Open Image in New Browser Tab")
                .is_some()
        );
        assert!(cx.debug_bounds("MENU_ITEM-Copy Image Address").is_some());
        assert!(
            cx.debug_bounds("MENU_ITEM-Copy Link Address").is_none(),
            "there is no link under the pointer, so nothing about one belongs in the menu"
        );
    }

    #[gpui::test]
    async fn a_selection_is_offered_for_copying_and_searching(cx: &mut TestAppContext) {
        let (_frame, cx) = a_page_frame(
            a_page_showing(PageTarget {
                selection: Some(A_SELECTION.to_string()),
                ..PageTarget::default()
            }),
            cx,
        )
        .await;

        right_click_the_page(cx);

        assert!(cx.debug_bounds("MENU_ITEM-Copy").is_some());
        assert!(
            cx.debug_bounds("MENU_ITEM-Search the Web for the Selection")
                .is_some()
        );
        assert!(cx.debug_bounds("MENU_ITEM-Copy Link Address").is_none());
    }

    #[gpui::test]
    async fn a_page_with_nothing_under_the_pointer_offers_only_the_page(cx: &mut TestAppContext) {
        let (_frame, cx) = a_page_frame(a_page_showing(nothing_in_particular()), cx).await;

        right_click_the_page(cx);

        assert!(
            cx.debug_bounds("MENU_ITEM-Open Link in New Browser Tab")
                .is_none()
        );
        assert!(cx.debug_bounds("MENU_ITEM-Copy Link Address").is_none());
        assert!(
            cx.debug_bounds("MENU_ITEM-Open Image in New Browser Tab")
                .is_none()
        );
        assert!(cx.debug_bounds("MENU_ITEM-Copy Image Address").is_none());
        assert!(cx.debug_bounds("MENU_ITEM-Copy").is_none());
        assert!(
            cx.debug_bounds("MENU_ITEM-Search the Web for the Selection")
                .is_none()
        );
        // What is always there, whatever the pointer is over.
        assert!(cx.debug_bounds("MENU_ITEM-Select All").is_some());
        assert!(cx.debug_bounds("MENU_ITEM-Save Page As…").is_some());
        assert!(cx.debug_bounds("MENU_ITEM-View Page Source").is_some());
        assert!(cx.debug_bounds("MENU_ITEM-Inspect").is_some());
        assert!(cx.debug_bounds("PAGE_MENU-page-menu-back").is_some());
        assert!(cx.debug_bounds("PAGE_MENU-page-menu-forward").is_some());
        assert!(cx.debug_bounds("PAGE_MENU-page-menu-reload").is_some());
    }

    #[gpui::test]
    async fn the_menu_lands_where_it_was_clicked_with_the_view_scrolled(cx: &mut TestAppContext) {
        let (frame, cx) = a_page_frame(a_page_showing(nothing_in_particular()), cx).await;

        let at_rest = painted(cx, PAGE_AREA);
        frame.read_with(cx, |frame, _| {
            frame.scroll.set_offset(point(px(0.), px(-200.)));
        });
        draw(cx);
        let scrolled = painted(cx, PAGE_AREA);
        assert!(
            scrolled.origin.y < at_rest.origin.y,
            "the frame has to have really scrolled: {:?} then {:?}",
            at_rest.origin,
            scrolled.origin
        );

        let at = scrolled.center();
        right_click(cx, at);
        menu_is_at(cx, at);
    }

    /// A press the page took would also take the keyboard from the menu, and a
    /// menu that has lost the keyboard closes before the button is let go --
    /// which is how every item in it stopped doing anything.
    #[cfg(feature = "servo")]
    #[gpui::test]
    async fn a_press_on_the_menu_is_not_handed_to_the_page(cx: &mut TestAppContext) {
        let (frame, cx) = a_page_frame(a_page_showing(nothing_in_particular()), cx).await;

        right_click_the_page(cx);
        let item = painted(cx, "MENU_ITEM-Select All").center();
        cx.simulate_event(MouseDownEvent {
            position: item,
            button: MouseButton::Left,
            modifiers: Modifiers::none(),
            click_count: 1,
            first_mouse: false,
        });

        assert!(
            frame.read_with(cx, |frame, cx| frame
                .preview
                .read(cx)
                .page_pressed
                .get()
                .is_none()),
            "a press on the menu belongs to the menu, not to the page under it"
        );
        assert!(
            cx.debug_bounds("MENU_ITEM-Select All").is_some(),
            "the menu has to still be there when the button is let go, or nothing in it can be chosen"
        );
    }

    #[gpui::test]
    async fn the_menu_goes_away_when_the_reader_clicks_elsewhere(cx: &mut TestAppContext) {
        let (frame, cx) = a_page_frame(a_page_showing(nothing_in_particular()), cx).await;

        let page = painted(cx, PAGE_AREA);
        right_click_the_page(cx);
        assert!(cx.debug_bounds("MENU_ITEM-Select All").is_some());

        // The menu opened in the middle of the page, so its top left corner is
        // well clear of it.
        cx.simulate_click(page.origin + point(px(8.), px(8.)), Modifiers::none());
        draw(cx);

        assert!(
            cx.debug_bounds("MENU_ITEM-Select All").is_none(),
            "a click away from the menu has to close it"
        );
        assert!(
            frame.read_with(cx, |frame, cx| frame
                .preview
                .read(cx)
                .context_menu
                .is_none()),
            "the closed menu has to be let go of, not just left unpainted"
        );
    }

    #[gpui::test]
    async fn the_row_of_buttons_takes_the_page_back(cx: &mut TestAppContext) {
        let (frame, cx) = a_page_frame(
            PageStandIn {
                under: nothing_in_particular(),
                behind_and_ahead: (true, true),
                source: A_SOURCE.to_string(),
            },
            cx,
        )
        .await;

        right_click_the_page(cx);
        click(cx, "PAGE_MENU-page-menu-back");

        assert_eq!(
            frame.read_with(cx, |frame, cx| frame
                .preview
                .read(cx)
                .asked_of_the_page
                .borrow()
                .clone()),
            vec![PageRequest::GoBack]
        );
        assert!(
            cx.debug_bounds("MENU_ITEM-Select All").is_none(),
            "a button in the row has to close the menu, as every other item does"
        );
    }

    #[gpui::test]
    async fn a_page_with_nowhere_to_go_back_to_does_not_go_back(cx: &mut TestAppContext) {
        let (frame, cx) = a_page_frame(a_page_showing(nothing_in_particular()), cx).await;

        right_click_the_page(cx);
        click(cx, "PAGE_MENU-page-menu-back");

        assert!(
            frame.read_with(cx, |frame, cx| frame
                .preview
                .read(cx)
                .asked_of_the_page
                .borrow()
                .is_empty()),
            "a page with nothing behind it must not be sent back"
        );
    }

    #[gpui::test]
    async fn choosing_select_all_asks_the_page_for_it(cx: &mut TestAppContext) {
        let (frame, cx) = a_page_frame(a_page_showing(nothing_in_particular()), cx).await;

        right_click_the_page(cx);
        click(cx, "MENU_ITEM-Select All");

        assert_eq!(
            frame.read_with(cx, |frame, cx| frame
                .preview
                .read(cx)
                .asked_of_the_page
                .borrow()
                .clone()),
            vec![PageRequest::SelectAll]
        );
    }

    #[gpui::test]
    async fn copying_a_link_address_puts_it_on_the_clipboard(cx: &mut TestAppContext) {
        let (_frame, cx) = a_page_frame(
            a_page_showing(PageTarget {
                link: Some(A_LINK.to_string()),
                ..PageTarget::default()
            }),
            cx,
        )
        .await;

        right_click_the_page(cx);
        click(cx, "MENU_ITEM-Copy Link Address");

        let copied = cx.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text()));
        assert_eq!(copied, Some(A_LINK.to_string()));
    }

    #[gpui::test]
    async fn viewing_the_source_opens_it_in_an_editor_tab(cx: &mut TestAppContext) {
        let (frame, cx) = a_page_frame(a_page_showing(nothing_in_particular()), cx).await;

        right_click_the_page(cx);
        click(cx, "MENU_ITEM-View Page Source");
        cx.run_until_parked();

        let opened = frame.read_with(cx, |frame, cx| {
            frame
                .workspace
                .read(cx)
                .active_pane()
                .read(cx)
                .items_of_type::<Editor>()
                .next()
        });
        let opened = opened.expect("the page's source has to open in an editor tab");
        assert_eq!(
            opened.read_with(cx, |editor, cx| editor.text(cx)),
            A_SOURCE.to_string()
        );
    }

    #[gpui::test]
    async fn saving_the_page_writes_its_html_where_the_reader_says(cx: &mut TestAppContext) {
        let (frame, cx) = a_page_frame(a_page_showing(nothing_in_particular()), cx).await;
        let files = frame.read_with(cx, |frame, cx| {
            frame.workspace.read(cx).project().read(cx).fs().clone()
        });

        right_click_the_page(cx);
        click(cx, "MENU_ITEM-Save Page As…");

        let somewhere = tempfile::tempdir().expect("a directory to save into");
        let saved = somewhere.path().join("saved.html");
        cx.simulate_new_path_selection({
            let saved = saved.clone();
            move |_| Some(saved)
        });
        cx.run_until_parked();

        // Read back through the file system the editor was given, which is the
        // one it wrote to: a real read would race a write the test scheduler
        // does not drive.
        let written = files
            .load(&saved)
            .await
            .expect("the page has to reach the file the reader chose");
        assert_eq!(
            written, A_SOURCE,
            "the page's own HTML has to reach the file the reader chose"
        );
    }

    #[test]
    fn what_the_page_answers_is_read_as_what_is_under_the_pointer() {
        let target = PageTarget::parse(
            "{\"link\":\"https://example.com/a\",\"image\":null,\"selection\":\"a giraffe\"}",
        );
        assert_eq!(
            target,
            PageTarget {
                link: Some("https://example.com/a".to_string()),
                image: None,
                selection: Some("a giraffe".to_string()),
            }
        );

        // A link with no address is not a link, and neither is a page that
        // answered nothing at all.
        assert_eq!(
            PageTarget::parse("{\"link\":\"\",\"image\":\"  \",\"selection\":\"\"}"),
            PageTarget::default()
        );
        assert_eq!(PageTarget::parse(""), PageTarget::default());
    }
}
